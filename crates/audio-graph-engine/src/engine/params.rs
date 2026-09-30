//! The parameter half: scalar ops over the register file, one row at a time.

use super::*;

impl Engine {
    /// Every stage's parameter and note ops for one row.
    ///
    /// Overwrites slot table values for lanes driven by the graph.
    ///
    /// Paired with [`run_audio`][Engine::run_audio] this puts every parameter
    /// of the block before any of its audio, which is one block behind the
    /// order the stages describe. What that costs depends on the graph:
    ///
    /// * with nothing reading a parameter off audio there is only one stage
    ///   and the two orders are the same walk;
    /// * with an [`Op::Follow`] whose value never comes back round to audio —
    ///   a meter, or anything else the editor reads and the block does not —
    ///   the audio is identical and the lane is a block old, which is what a
    ///   meter is anyway;
    /// * with one that *does* reach audio again, through a sub-plugin's
    ///   parameter or a generated controller, the block is rendered against
    ///   the level of the block before it.
    ///
    /// The buffer read is the pool as the last block left it, so that last
    /// case is a block of latency rather than nonsense. A caller that wants
    /// none of it walks the stages itself; see [`Engine::stages`].
    pub fn run(&mut self, ctx: &BlockContext, slots: &mut [f64]) {
        for stage in 0..self.stages() {
            self.run_stage(stage, ctx, slots);
        }
    }

    /// One stage's parameter and note ops for one row.
    ///
    /// Called once per row, in order, before that stage's audio ops.
    /// What a parameter op reads out of a note buffer is everything the buffer
    /// holds, which is the stream up to the boundary this row starts on.
    pub fn run_stage(&mut self, stage: usize, ctx: &BlockContext, slots: &mut [f64]) {
        // Moved out and put back rather than borrowed.
        let Some(program) = self.program.take() else {
            return;
        };
        let Some(&stage) = program.stages.get(stage) else {
            self.program = Some(program);
            return;
        };
        if program.is_empty() {
            self.program = Some(program);
            return;
        }

        self.eval_row(&program, stage, ctx, slots);

        // Last, so that a reader in *this* row saw the previous one's
        // stream. A parameter signal has the resolution of a row, so the value it
        // wants is the one in effect at the boundary it just crossed, not one
        // from the middle of the row about to start.
        //
        // The buffers are appended to rather than refilled, so where they
        // stand now is both what this row's ops must skip and where the audio
        // half will later find this row's events.
        let row = ctx.row as usize;
        let mut base = [0usize; MAX_NOTE_BUFS];
        for (slot, buf) in base.iter_mut().zip(self.notes.bufs.iter()) {
            *slot = buf.events.len();
        }
        // Only for the buffers this stage fills. A later stage passing over
        // the same rows would otherwise overwrite every mark with the length
        // the buffer finished at, and the audio half would read the whole
        // block as one row.
        if let Some(marks) = self.note_marks.get_mut(row) {
            for (buf, mark) in marks.iter_mut().enumerate() {
                if stage.note_bufs & (1 << buf) != 0 {
                    *mark = base[buf] as u32;
                }
            }
        }
        // Two disjoint fields, which is the whole reason the note half's
        // state is a type of its own: the pass wants the block's stream by
        // shared reference and everything it fills by exclusive one.
        self.notes.run_notes_step(
            &program,
            stage,
            &self.translated,
            ctx.offset,
            ctx.frames,
            slots,
            &base,
            (ctx.sample_rate, ctx.tempo_bpm),
            &mut self.ledger,
        );
        self.note_rows = self.note_rows.max(row + 1);

        self.program = Some(program);
    }

    /// One stage's parameter ops at the end of the block, into `slots`,
    /// leaving everything the ops carry from row to row where it was.
    ///
    /// `ctx` names the block's last sample boundary with a row of no frames.
    /// The next block's first row is evaluated at that same instant, so this
    /// is its value worked out early: what a consumer joining points with
    /// lines needs to draw the last row's line to where the value is going.
    /// Nothing moves because of it — no phase advances, no latch is set, no
    /// delay line is written and no note is generated — so the next block
    /// starts exactly where it would have without it.
    pub fn run_stage_end(&mut self, stage: usize, ctx: &BlockContext, slots: &mut [f64]) {
        let Some(program) = self.program.take() else {
            return;
        };
        if let Some(&stage) = program.stages.get(stage)
            && !program.is_empty()
        {
            self.peek.save(
                &self.lfos,
                &self.latches,
                &self.lines,
                &self.registers,
                self.rng,
            );
            self.eval_row(&program, stage, ctx, slots);
            self.peek.restore(
                &mut self.lfos,
                &mut self.latches,
                &mut self.lines,
                &mut self.registers,
                &mut self.rng,
            );
        }
        self.program = Some(program);
    }

    /// One stage's parameter ops for one row, and the lanes they drive.
    fn eval_row(&mut self, program: &Program, stage: Stage, ctx: &BlockContext, slots: &mut [f64]) {
        let dt = if ctx.sample_rate > 0.0 {
            f64::from(ctx.frames) / ctx.sample_rate
        } else {
            0.0
        };

        for op in &program.ops[stage.params.range()] {
            match *op {
                Op::DelayRead {
                    out,
                    line,
                    time,
                    time_reg,
                } => {
                    // A wired time control overrides the static node setting.
                    let time = match time_reg {
                        Some(reg) => self.registers[reg as usize].max(0.0),
                        None => time,
                    };
                    self.registers[out as usize] = match self.lines.get(line as usize) {
                        Some(held) if held.len > 1 && held.ring.len() >= held.len => {
                            // At least one sample back, which is the last
                            // sample the previous row wrote: this row's own
                            // write comes after every read, and a read that
                            // could see it would close a loop with no delay.
                            let back = (time * ctx.sample_rate)
                                .round()
                                .clamp(1.0, (held.len - 1) as f64)
                                as usize;
                            held.ring[(held.head + held.len - back) % held.len]
                        }
                        _ => 0.0,
                    };
                }
                Op::DelayWrite { line, a } => {
                    // The row's value, over every sample the row covers: a
                    // read a row's length back and one a sample back then
                    // agree on what that row was.
                    let value = self.registers[a as usize];
                    if let Some(held) = self.lines.get_mut(line as usize)
                        && held.len > 1
                        && held.ring.len() >= held.len
                    {
                        for i in 0..ctx.frames as usize {
                            held.ring[(held.head + i) % held.len] = value;
                        }
                        held.head = (held.head + ctx.frames as usize) % held.len;
                    }
                }
                Op::Const { out, value } => self.registers[out as usize] = value,
                Op::Slot { out, slot } => {
                    self.registers[out as usize] = slots.get(slot as usize).copied().unwrap_or(0.0)
                }
                Op::Lfo {
                    out,
                    state,
                    waveform,
                    rate,
                    offset_phase,
                    depth,
                    centre,
                } => {
                    let i = state as usize;
                    let phase = (self.lfos[i].phase + offset_phase).rem_euclid(1.0);
                    let shape = waveform.shape(phase).unwrap_or(self.lfos[i].hold);
                    self.registers[out as usize] = centre + depth * shape;

                    let hz = match rate {
                        RateSpec::Hz(hz) => hz,
                        RateSpec::CyclesPerBeat(cpb) => cpb * ctx.tempo_bpm / 60.0,
                    };
                    let advanced = self.lfos[i].phase + hz * dt;
                    if advanced >= 1.0 && waveform == Waveform::Random {
                        self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        self.lfos[i].hold = f64::from(self.rng >> 8) / f64::from(1u32 << 23) - 1.0;
                    }
                    self.lfos[i].phase = advanced.rem_euclid(1.0);
                }
                Op::Select {
                    out,
                    control,
                    threshold,
                    low,
                    high,
                } => {
                    let pick = if self.registers[control as usize] >= threshold {
                        high
                    } else {
                        low
                    };
                    self.registers[out as usize] = match pick {
                        Operand::Reg(reg) => self.registers[reg as usize],
                        Operand::Value(value) => value,
                    };
                }
                Op::KeyHeld { out, buf, key } => {
                    self.registers[out as usize] = f64::from(self.held(buf, key));
                }
                Op::Follow {
                    out,
                    buf,
                    state,
                    detect,
                    attack,
                    release,
                } => {
                    let win = Window {
                        block: ctx.block as usize,
                        start: ctx.offset as usize,
                        frames: ctx.frames as usize,
                    };
                    let width = program
                        .buffers
                        .get(buf as usize)
                        .map_or(0, |&w| (w as usize).min(MAX_CHANNELS));
                    let level = self.loudness(buf, width, win, detect);
                    // One pole per row, which is as often as a parameter
                    // is allowed to move. `dt` is this row's length, so
                    // the times mean the same thing at any resolution and any
                    // block size. A time of zero is a coefficient of zero,
                    // which is following exactly.
                    let held = self
                        .latches
                        .get(state as usize)
                        .map(|latch| latch.value)
                        .unwrap_or(0.0);
                    let held = if held.is_finite() { held } else { 0.0 };
                    let time = if level > held { attack } else { release };
                    // An empty window says nothing about how loud anything is,
                    // which is how the end of a block is read: as where the
                    // envelope stands, not as silence.
                    let value = if win.frames == 0 {
                        held
                    } else if time > 0.0 && dt > 0.0 {
                        let coeff = (-dt / time).exp();
                        held + (level - held) * (1.0 - coeff)
                    } else {
                        level
                    };
                    if let Some(latch) = self
                        .latches
                        .get_mut(state as usize)
                        .map(|latch| &mut latch.value)
                    {
                        *latch = value;
                    }
                    self.registers[out as usize] = value;
                }
                Op::NoteFollow {
                    out,
                    buf,
                    state,
                    what,
                } => {
                    let index = buf as usize;
                    self.registers[out as usize] = match what {
                        Follow::Velocity => {
                            self.notes.bufs.get(index).map_or(0.0, |buf| buf.velocity)
                        }
                        Follow::KeyTrack => self.notes.bufs.get(index).map_or(0.5, |buf| buf.key),
                        Follow::Gate => f64::from(u8::from(
                            self.notes.bufs.get(index).is_some_and(|buf| buf.count > 0),
                        )),
                        // The mask rather than the count next to it: the same
                        // key on two channels is one key under a hand, and a
                        // key struck again before it was let go is one key
                        // too. See [`Follow::HeldKeys`].
                        Follow::HeldKeys => self
                            .notes
                            .bufs
                            .get(index)
                            .map_or(0.0, |buf| f64::from(buf.held.count_ones())),
                    };
                    // The latch is not read back — the tables above already
                    // survive a program swap — but keeping the value in it
                    // means the editor can show what the node is reading.
                    if let Some(latch) = self
                        .latches
                        .get_mut(state as usize)
                        .map(|latch| &mut latch.value)
                    {
                        *latch = self.registers[out as usize];
                    }
                }
                Op::KeySelect {
                    state,
                    buf,
                    keys,
                    count,
                    cycle,
                } => {
                    let keys = &keys[..usize::from(count).min(keys.len())];
                    if let (Some(source), Some(latch)) = (
                        self.notes.bufs.get(buf as usize),
                        self.latches.get_mut(state as usize),
                    ) {
                        for event in &source.events[source.row_start.min(source.events.len())..] {
                            let Event::Note(NoteEvent::NoteOn { key, velocity, .. }) = *event
                            else {
                                continue;
                            };
                            if cycle {
                                if keys
                                    .first()
                                    .is_some_and(|trigger| trigger.matches(key, velocity))
                                {
                                    let at = if latch.value.is_nan() {
                                        0.0
                                    } else {
                                        latch.value
                                    };
                                    latch.value = (at + 1.0).rem_euclid(keys.len() as f64);
                                }
                            } else if let Some(index) = keys
                                .iter()
                                .rposition(|trigger| trigger.matches(key, velocity))
                            {
                                latch.value = index as f64;
                            }
                        }
                    }
                }
                Op::KeyStep {
                    state,
                    buf,
                    key,
                    count,
                } => {
                    if self.struck(buf, key)
                        && count > 0
                        && let Some(latch) = self
                            .latches
                            .get_mut(state as usize)
                            .map(|latch| &mut latch.value)
                    {
                        let at = if latch.is_nan() { 0.0 } else { *latch };
                        *latch = (at + 1.0).rem_euclid(f64::from(count));
                    }
                }
                Op::KeyLatch {
                    state,
                    buf,
                    key,
                    value,
                } => {
                    if self.struck(buf, key)
                        && let Some(latch) = self
                            .latches
                            .get_mut(state as usize)
                            .map(|latch| &mut latch.value)
                    {
                        *latch = value;
                    }
                }
                Op::LatchIs {
                    out,
                    state,
                    value,
                    initial,
                } => {
                    let at = self
                        .latches
                        .get(state as usize)
                        .map(|latch| latch.value)
                        .unwrap_or(f64::NAN);
                    let at = if at.is_nan() { initial } else { at };
                    self.registers[out as usize] = f64::from(at == value);
                }
                Op::NoteCc {
                    out,
                    buf,
                    state,
                    channel,
                    cc,
                    initial,
                } => {
                    // The last matching event wins: within one row a
                    // controller may move several times, and what the boundary
                    // carries is where it ended up.
                    let latest = self
                        .notes
                        .bufs
                        .get(buf as usize)
                        .into_iter()
                        .flat_map(|buf| buf.events.iter())
                        .rev()
                        .find_map(|event| match *event {
                            Event::Note(NoteEvent::Cc {
                                channel: on,
                                cc: number,
                                value,
                                ..
                            }) if number == cc && (channel < 0 || channel == on) => Some(value),
                            _ => None,
                        });
                    let held = &mut self.latches[state as usize].value;
                    if let Some(value) = latest {
                        *held = value;
                    }
                    self.registers[out as usize] = if held.is_nan() { initial } else { *held };
                }
                Op::Latch {
                    out,
                    state,
                    initial,
                } => {
                    let value = self
                        .latches
                        .get(state as usize)
                        .map(|latch| latch.value)
                        .unwrap_or(f64::NAN);
                    self.registers[out as usize] = if value.is_nan() { initial } else { value };
                }
                Op::Math { out, a, b, op } => {
                    let a = self.registers[a as usize];
                    let b = match b {
                        Operand::Reg(reg) => self.registers[reg as usize],
                        Operand::Value(value) => value,
                    };
                    self.registers[out as usize] = match op {
                        MathOp::Add => a + b,
                        MathOp::Subtract => a - b,
                        MathOp::Multiply => a * b,
                        MathOp::Min => a.min(b),
                        MathOp::Max => a.max(b),
                        // Clamping the exponent to at least 0.01 prevents yielding Infinity,
                        // which can crash third-party plugins if fed to their parameters.
                        MathOp::Curve => a.clamp(0.0, 1.0).powf(b.clamp(0.01, 100.0)),
                    };
                }
                Op::Range {
                    out,
                    a,
                    in_lo,
                    in_span,
                    out_lo,
                    out_span,
                    clamp,
                } => {
                    let value = self.registers[a as usize];
                    let t = if in_span == 0.0 {
                        0.0
                    } else {
                        (value - in_lo) / in_span
                    };
                    let t = if clamp { t.clamp(0.0, 1.0) } else { t };
                    self.registers[out as usize] = out_lo + t * out_span;
                }
            }
        }

        for &(lane, reg) in &program.outputs[stage.outputs.range()] {
            if let Some(target) = slots.get_mut(lane as usize) {
                // Host automation and parameter slots are normalized to 0..1, while audio lanes
                // carry physical units (decibels, seconds) without clamping.
                let value = self.registers[reg as usize];
                *target = if !value.is_finite() {
                    0.0
                } else if lane < program.audio_lane_base {
                    value.clamp(0.0, 1.0)
                } else {
                    value
                };
            }
        }
    }

    /// How loud one window of an audio buffer is, across its channels.
    ///
    /// Linear amplitude, not decibels: a parameter lane is a plain number and
    /// the graph has arithmetic nodes for anyone who wants the log of it.
    fn loudness(&self, buf: Buf, width: usize, win: Window, detect: Detect) -> f64 {
        if width == 0 || win.frames == 0 {
            return 0.0;
        }
        let mut peak = 0.0f32;
        let mut sum = 0.0f64;
        for ch in 0..width {
            let at = self.at(buf, ch, win);
            for &sample in &self.pool[at..at + win.frames] {
                match detect {
                    Detect::Peak => peak = peak.max(sample.abs()),
                    Detect::Rms => sum += f64::from(sample) * f64::from(sample),
                }
            }
        }
        match detect {
            Detect::Peak => f64::from(peak),
            // Across every channel at once rather than per channel and
            // averaged: what is wanted is how loud the signal is, and a stereo
            // pair carrying the same thing twice is not twice as loud.
            Detect::Rms => (sum / (width * win.frames) as f64).sqrt(),
        }
    }

    /// Whether `key` is down on `buf`. Out of range is never down.
    fn held(&self, buf: u16, key: u8) -> bool {
        let table = self.notes.bufs.get(buf as usize).map_or(0, |buf| buf.held);
        key_bit(i16::from(key)).is_some_and(|bit| table & bit != 0)
    }

    /// Whether `key` was struck in the row `buf` last carried.
    fn struck(&self, buf: u16, key: u8) -> bool {
        let table = self
            .notes
            .bufs
            .get(buf as usize)
            .map_or(0, |buf| buf.struck);
        key_bit(i16::from(key)).is_some_and(|bit| table & bit != 0)
    }
}
