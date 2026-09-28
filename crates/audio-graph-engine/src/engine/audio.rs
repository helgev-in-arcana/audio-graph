//! The audio half: the buffer pool, sub-plugin calls and delay lines.

use super::*;

/// Where one chunk of a block sits inside the buffer pool.
///
/// Channels are packed at the DAW's block length, never at the chunk's, so
/// where a sample lives does not depend on how big the call that wrote it was.
/// A buffer filled in one go has to be readable a sub-block at a time, and the
/// other way round, the moment two parts of a program run at different
/// granularities.
///
/// The sub-plugin boundary is the one place that still wants the chunk's own
/// packing, because that is what every plugin format's buffer layout means.
/// See [`AudioOp::Plugin`] in `run_chunk`.
#[derive(Clone, Copy)]
pub(super) struct Window {
    /// Frames in the DAW's block: the distance between one channel and the
    /// next inside a buffer.
    pub(super) block: usize,
    /// Where this chunk starts inside the block.
    pub(super) start: usize,
    /// Frames this chunk covers.
    pub(super) frames: usize,
}

/// Four-point cubic Hermite, the interpolator a modulated delay asks for.
///
/// Linear interpolation loses audible high end while the delay time is moving,
/// and an all-pass interpolator misbehaves under exactly the modulation this
/// exists to support. `x` is the fractional
/// offset between `y1` and `y2`.
fn hermite(y0: f32, y1: f32, y2: f32, y3: f32, x: f32) -> f32 {
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * x + c2) * x + c1) * x + y1
}

impl Engine {
    /// One stage's audio ops, over the whole block.
    ///
    /// A stage covers the block before the next one starts, so a stage that
    /// steps a sub-block at a time hands the one after it a finished buffer.
    /// Only the stage holding a delay line's two ends steps; the rest of the
    /// program is called once, however many loops are drawn elsewhere in the
    /// patch.
    pub fn run_audio_stage(
        &mut self,
        stage: usize,
        ctx: &AudioContext<'_>,
        daw_in: &[f32],
        daw_out: &mut [f32],
        nodes: &mut dyn AudioInstances,
    ) {
        let total = ctx.frames as usize;
        if self.stride == 0 || total > self.stride {
            return;
        }
        let Some(program) = self.program.take() else {
            return;
        };
        if let Some(&stage) = program.stages.get(stage) {
            let step = match stage.chunking {
                Chunking::WholeBlock => total.max(1),
                // The last chunk is short whenever the block is not a multiple
                // of the quantum.
                Chunking::SubBlock => (ctx.quantum as usize).max(1),
            };
            let mut start = 0usize;
            while start < total {
                let len = step.min(total - start);
                self.run_chunk(&program, stage, ctx, nodes, daw_in, daw_out, start, len);
                start += len;
            }
        }

        self.program = Some(program);
    }

    /// Where the events of samples `window` sit in note buffer `buf`, given
    /// that they lie in rows `first..end`.
    ///
    /// The buffer holds the whole block, so this is how the audio half asks
    /// for its own chunk's events without the note half having to run again.
    /// A row past what the parameter half has filled reads to the end, which
    /// is what makes the last chunk right whether or not the block divides
    /// evenly by the resolution.
    ///
    /// The rows find the stretch; the window trims it. A row coarser than the
    /// chunk holds events on both sides of it, and an instance is owed exactly
    /// what falls inside its own call.
    fn note_slice(
        &self,
        buf: u16,
        (first, end): (usize, usize),
        window: std::ops::Range<usize>,
    ) -> std::ops::Range<usize> {
        let len = self
            .notes
            .bufs
            .get(buf as usize)
            .map_or(0, |buf| buf.events.len());
        let mark = |row: usize| {
            self.note_marks
                .get(row)
                .and_then(|marks| marks.get(buf as usize))
                .map_or(len, |&at| (at as usize).min(len))
        };
        // Not zero for row 0: what sits before it is the carry-over from the
        // previous block, which the parameter half reads and the plugins have
        // already been handed.
        let from = mark(first);
        let to = if end >= self.note_rows {
            len
        } else {
            mark(end)
        }
        .max(from);
        let Some(events) = self
            .notes
            .bufs
            .get(buf as usize)
            .map(|b| &b.events[from..to])
        else {
            return from..to;
        };
        let before = |at: usize| move |e: &Event| (e.sample_offset() as usize) < at;
        from + events.partition_point(before(window.start))
            ..from + events.partition_point(before(window.end))
    }

    /// How many events have been dropped for want of buffer space.
    pub fn notes_dropped(&self) -> u64 {
        self.notes.dropped
    }

    /// One chunk of `run_audio`: every op, over `len` frames starting at
    /// `start` inside the DAW's block.
    #[allow(clippy::too_many_arguments)]
    fn run_chunk(
        &mut self,
        program: &Program,
        stage: Stage,
        ctx: &AudioContext<'_>,
        nodes: &mut dyn AudioInstances,
        daw_in: &[f32],
        daw_out: &mut [f32],
        start: usize,
        frames: usize,
    ) {
        let block = ctx.frames as usize;
        let Ok(schedule) = ScheduleView::from_parts(
            ctx.lanes,
            ctx.lanes_per_row,
            ctx.frames.div_ceil(ctx.resolution.max(1)) as usize,
            ctx.resolution,
            ctx.frames,
        )
        .and_then(|view| view.with_end(ctx.end)) else {
            daw_out.fill(0.0);
            return;
        };
        // Which rows of the note buffers this chunk covers. The buffers were
        // filled by the parameter half and hold the whole block, and the rows
        // touching a chunk are a contiguous run, so its events are a
        // contiguous slice. A chunk inside one row is handed that row's
        // events whole, and the adapter keeps only the ones inside the chunk.
        let first_row = ctx.row_at(start);
        let end_row = (start + frames).div_ceil((ctx.resolution as usize).max(1));
        let win = Window {
            block,
            start,
            frames,
        };

        for op in &program.audio_ops[stage.audio.range()] {
            match op {
                AudioOp::Silence { out } => self.fill(*out, win, 0.0),
                AudioOp::Input { out, bus } => {
                    let width = program.buffers[*out as usize] as usize;
                    // daw_in holds interleaved planar buses.
                    let bus = *bus as usize;
                    let Some(&have) = self.daw_inputs.get(bus) else {
                        self.fill(*out, win, 0.0);
                        continue;
                    };
                    // Both sides are packed at the block's length, so the two
                    // walk in step and only the bus base differs.
                    let base: usize = self.daw_inputs[..bus]
                        .iter()
                        .map(|&c| c as usize * block)
                        .sum();
                    for ch in 0..width.min(MAX_CHANNELS) {
                        let to = self.at(*out, ch, win);
                        if ch >= have as usize {
                            self.pool[to..to + frames].fill(0.0);
                            continue;
                        }
                        let from = base + ch * block + start;
                        for i in 0..frames {
                            self.pool[to + i] = daw_in.get(from + i).copied().unwrap_or(0.0);
                        }
                    }
                }
                AudioOp::Output { a, bus } => {
                    if *bus != 0 {
                        continue;
                    }
                    let width = program.buffers[*a as usize] as usize;
                    for ch in 0..width.min(MAX_CHANNELS) {
                        let from = self.at(*a, ch, win);
                        let to = ch * block + start;
                        if to + frames <= daw_out.len() {
                            daw_out[to..to + frames]
                                .copy_from_slice(&self.pool[from..from + frames]);
                        }
                    }
                }
                AudioOp::Gather { out, buses } => {
                    // Assembles the plugin input buffer across connected buses.
                    // Width conversions are performed here during assembly.
                    let mut at = 0usize;
                    for &(from, want) in buses {
                        let have = program.buffers[from as usize];
                        for ch in 0..want {
                            let to = self.at(*out, at + ch as usize, win);
                            if have == 1 && want > 1 {
                                // Mono into a wider bus: the same signal on
                                // every channel, which is what a host does.
                                let src = self.at(from, 0, win);
                                self.pool.copy_within(src..src + frames, to);
                            } else if want == 1 && have > 1 {
                                // Wider into mono: averaged, the inverse of the
                                // branch above, so a round trip keeps its
                                // level. Taking the left channel alone would
                                // ignore half the signal.
                                let first = self.at(from, 0, win);
                                self.pool.copy_within(first..first + frames, to);
                                for other in 1..have {
                                    let src = self.at(from, other as usize, win);
                                    for i in 0..frames {
                                        self.pool[to + i] += self.pool[src + i];
                                    }
                                }
                                let scale = 1.0 / have as f32;
                                for i in 0..frames {
                                    self.pool[to + i] *= scale;
                                }
                            } else if ch < have {
                                let src = self.at(from, ch as usize, win);
                                self.pool.copy_within(src..src + frames, to);
                            } else {
                                self.pool[to..to + frames].fill(0.0);
                            }
                        }
                        at += want as usize;
                    }
                }
                AudioOp::Split {
                    from,
                    out,
                    channel,
                    width,
                } => {
                    // One bus out of a plugin's output region. No conversion:
                    // both sides are the width the plugin negotiated.
                    for ch in 0..*width as usize {
                        let src = self.at(*from, *channel as usize + ch, win);
                        let dst = self.at(*out, ch, win);
                        self.pool.copy_within(src..src + frames, dst);
                    }
                }
                AudioOp::Plugin {
                    instance,
                    input,
                    input_buses,
                    output,
                    output_buses,
                    notes,
                } => {
                    // Worked out before the pool is split, because that borrow
                    // covers the rest of the arm.
                    let heard = notes.map(|buf| {
                        self.note_slice(buf, (first_row, end_row), start..start + frames)
                    });
                    // The compiler guarantees these differ, so the two regions
                    // cannot overlap and `split_at_mut` is enough to prove it.
                    let span = MAX_BUFFER_CHANNELS * self.stride;
                    let (lo, hi) = if input < output {
                        (*input as usize, *output as usize)
                    } else {
                        (*output as usize, *input as usize)
                    };
                    let (front, back) = self.pool.split_at_mut(hi * span);
                    let low = &mut front[lo * span..lo * span + span];
                    let high = &mut back[..span];
                    let (source, dest) = if input < output {
                        (&low[..], high)
                    } else {
                        (&high[..], low)
                    };
                    let in_width: u16 = input_buses.iter().sum();
                    let out_width: u16 = output_buses.iter().sum();
                    // Only what the plugin will actually read is handed over.
                    // The buffer behind it is as wide as any buffer in the
                    // pool; the region it owns is sized for its active buses.
                    let packed_in = in_width as usize * frames;
                    let packed_out = out_width as usize * frames;
                    // A plugin is handed its channels packed at the length of
                    // the call, which is what every format's buffer layout
                    // means. The pool packs at the block's length instead, so
                    // that a buffer written whole can be read a sub-block at a
                    // time. The two agree whenever the chunk *is* the block —
                    // the common case, handed over where it lies — and a
                    // shorter chunk is gathered into a scratch and scattered
                    // back. Only a program with a feedback loop in it pays
                    // that, and it is already paying for the extra calls.
                    let short = frames != block;
                    if short {
                        for ch in 0..in_width as usize {
                            let from = ch * block + start;
                            let to = ch * frames;
                            self.chunk_in[to..to + frames]
                                .copy_from_slice(&source[from..from + frames]);
                        }
                    }
                    // An unwired notes port hears nothing, which is not the
                    // same as hearing an empty buffer only because this chunk
                    // was quiet.
                    let events: &[Event] = match (notes, heard) {
                        (Some(buf), Some(range)) => &self.notes.bufs[*buf as usize].events[range],
                        _ => &[],
                    };
                    // Counted here, where the note is actually handed over, and
                    // not where a wire branches: a branch a gate later swallows
                    // would never be counted back down, and the note would
                    // never be reported ended.
                    for event in events {
                        match event {
                            Event::Note(NoteEvent::NoteOn {
                                note_id: Some(id),
                                port,
                                ..
                            }) => {
                                if nodes.reports_note_end(*instance, *port) {
                                    self.ledger.delivered(*id);
                                } else {
                                    self.ledger.delivered_with_fallback(*id, *instance, *port);
                                }
                            }
                            Event::Note(NoteEvent::NoteOff {
                                note_id: Some(id),
                                port,
                                ..
                            }) if !nodes.reports_note_end(*instance, *port) => {
                                self.ledger.released_to(*id, *instance, *port)
                            }
                            _ => {}
                        }
                    }
                    let (heard_in, heard_out): (&[f32], &mut [f32]) = if short {
                        (
                            &self.chunk_in[..packed_in],
                            &mut self.chunk_out[..packed_out],
                        )
                    } else {
                        (&source[..packed_in], &mut dest[..packed_out])
                    };
                    nodes.process(
                        *instance,
                        events,
                        heard_in,
                        heard_out,
                        AudioChunk {
                            input_channels: in_width,
                            output_channels: out_width,
                            aux_inputs: plugin_host::AuxBuses::new(
                                input_buses.get(1..).unwrap_or(&[]),
                            ),
                            aux_outputs: plugin_host::AuxBuses::new(
                                output_buses.get(1..).unwrap_or(&[]),
                            ),
                            frames: frames as u32,
                            offset: start as u32,
                        },
                        schedule,
                    );
                    if short {
                        for ch in 0..out_width as usize {
                            let from = ch * frames;
                            let to = ch * block + start;
                            dest[to..to + frames]
                                .copy_from_slice(&self.chunk_out[from..from + frames]);
                        }
                    }
                }
                AudioOp::Mix { out, inputs } => {
                    if inputs.is_empty() {
                        self.fill(*out, win, 0.0);
                        continue;
                    }
                    let width = program.buffers[*out as usize] as usize;
                    for (n, input) in inputs.iter().enumerate() {
                        // A wired gain follows its lane's line a row at a
                        // time, however the audio is chunked. Drawn between
                        // linear gains rather than decibels: cheaper per
                        // sample, and over one row the two are the same move.
                        let mut at = start;
                        while at < start + frames {
                            let seg = ctx.run_from(at, start + frames);
                            let (line, from_gain, to_gain) =
                                match input.lane.and_then(|lane| ctx.lane_line(at, lane)) {
                                    Some(line) => {
                                        (Some(line), db_to_linear(line.from), db_to_linear(line.to))
                                    }
                                    None => (None, input.gain, input.gain),
                                };
                            let done = at - start;
                            for ch in 0..width.min(MAX_CHANNELS) {
                                let from = self.at(input.buf, ch, win) + done;
                                let to = self.at(*out, ch, win) + done;
                                if from == to && from_gain == 1.0 && to_gain == 1.0 {
                                    // Already in place and unchanged: unity
                                    // gain on the destination buffer.
                                    continue;
                                }
                                for i in 0..seg {
                                    let gain = match line {
                                        Some(line) => {
                                            from_gain + (to_gain - from_gain) * line.along(at + i)
                                        }
                                        None => from_gain,
                                    } as f32;
                                    let value = self.pool[from + i] * gain;
                                    if n == 0 {
                                        self.pool[to + i] = value;
                                    } else {
                                        self.pool[to + i] += value;
                                    }
                                }
                            }
                            at += seg;
                        }
                    }
                }
                AudioOp::Fade {
                    out,
                    a,
                    state,
                    lane,
                    gain,
                    rise,
                    fall,
                } => {
                    let width = program.buffers[*out as usize] as usize;
                    let rate = ctx.sample_rate.max(1.0);
                    // Where the last block left the ramp. NaN until it has
                    // ever run, which the first target resolves.
                    let mut from = self
                        .latches
                        .get(*state as usize)
                        .map(|latch| latch.value)
                        .unwrap_or(f64::NAN);
                    // One segment per row the chunk covers, so a chunk
                    // that spans the whole block still follows the lane.
                    let mut target = *gain;
                    let mut done = 0usize;
                    while done < frames {
                        let at = start + done;
                        let seg = ctx.run_from(at, start + frames);
                        // A row the lane grid does not reach holds the target
                        // where it was. Opening a gate because a block ran off
                        // the end of the schedule is not a defensible answer.
                        if let Some(value) = lane.and_then(|lane| ctx.lane(ctx.row_at(at), lane)) {
                            target = db_to_linear(value);
                        }
                        if from.is_nan() {
                            from = target;
                        }
                        let seconds = if target > from { *rise } else { *fall };
                        // Per sample, of the whole 0..1 travel. A fade of no
                        // time is a step, and arrives within the first sample.
                        let step = if seconds > 0.0 {
                            1.0 / (seconds * rate)
                        } else {
                            f64::INFINITY
                        };
                        let delta = target - from;
                        // Where the ramp stops and the constant tail begins.
                        let ramp = if step.is_finite() {
                            ((delta.abs() / step).ceil() as usize).min(seg)
                        } else {
                            0
                        };
                        // Off the segment's own start rather than accumulated
                        // per sample, so both channels travel identically and
                        // the value carried out is the one they reached.
                        let ramped = |i: usize| {
                            let moved = from + delta.signum() * step * (i + 1) as f64;
                            if delta > 0.0 {
                                moved.min(target)
                            } else {
                                moved.max(target)
                            }
                        };
                        for ch in 0..width.min(MAX_CHANNELS) {
                            let src = self.at(*a, ch, win) + done;
                            let dst = self.at(*out, ch, win) + done;
                            for i in 0..ramp {
                                self.pool[dst + i] = self.pool[src + i] * ramped(i) as f32;
                            }
                            for i in ramp..seg {
                                self.pool[dst + i] = self.pool[src + i] * target as f32;
                            }
                        }
                        from = if ramp < seg { target } else { ramped(ramp - 1) };
                        done += seg;
                    }
                    if let Some(latch) = self
                        .latches
                        .get_mut(*state as usize)
                        .map(|latch| &mut latch.value)
                    {
                        *latch = from;
                    }
                }
                AudioOp::Compensate { buf, slot, samples } => {
                    let width = program.buffers[*buf as usize] as usize;
                    self.compensate(*buf, *slot as usize, *samples as usize, width, win);
                }
                AudioOp::DelayRead {
                    out,
                    line,
                    state,
                    lane,
                    time,
                    max_time,
                } => {
                    // Where the lane is at the chunk's last sample, because
                    // that is where the read's sweep lands.
                    let seconds = lane
                        .and_then(|lane| ctx.lane_value(start + frames.max(1) - 1, lane))
                        .unwrap_or(*time)
                        .max(0.0);
                    let width = program.buffers[*out as usize] as usize;
                    self.delay_read(*line as usize, *state as usize, *out, width, win, {
                        // Minimum floor in samples, plus the two samples the
                        // interpolator needs ahead of the read pointer.
                        let floor = frames as f64 + 2.0;
                        let ceiling = (max_time * ctx.sample_rate)
                            .min(self.audio_lines[*line as usize].len.saturating_sub(4) as f64)
                            .max(floor);
                        (seconds * ctx.sample_rate).clamp(floor, ceiling)
                    });
                }
                AudioOp::DelayWrite { line, a } => {
                    let width = program.buffers[*a as usize] as usize;
                    self.delay_write(*line as usize, *a, width, win);
                }
                AudioOp::DelaySilence { line } => {
                    self.delay_silence(*line as usize, frames);
                }
                AudioOp::Tremolo {
                    out,
                    a,
                    state,
                    lane,
                    depth,
                    waveform,
                    rate,
                } => {
                    let width = (program.buffers[*out as usize] as usize).min(MAX_CHANNELS);
                    self.tremolo(
                        *out,
                        *a,
                        *state as usize,
                        (*lane, *depth),
                        (*waveform, *rate),
                        width,
                        win,
                        ctx,
                    );
                }
                AudioOp::Math {
                    out,
                    a,
                    b,
                    op,
                    state,
                } => {
                    let width = (program.buffers[*out as usize] as usize).min(MAX_CHANNELS);
                    self.audio_math(
                        *out,
                        *a,
                        *b,
                        *op,
                        *state as usize,
                        width,
                        win,
                        ctx.sample_rate,
                    );
                }
            }
        }
    }

    /// One chunk of an [`AudioOp::Tremolo`].
    ///
    /// The state holds the phase in value 0, the depth last applied in value
    /// 1, and in value 2 whether it has ever run: a tremolo loaded at full
    /// depth starts at full depth rather than fading in from none.
    #[allow(clippy::too_many_arguments)]
    fn tremolo(
        &mut self,
        out: Buf,
        a: Buf,
        state: usize,
        (lane, depth): (Option<u16>, f64),
        (waveform, rate): (Waveform, RateSpec),
        width: usize,
        win: Window,
        ctx: &AudioContext<'_>,
    ) {
        let Some(held) = self.dsp.get(state) else {
            return;
        };
        let (mut phase, mut from, started) =
            (held.values[0], held.values[1], held.values[2] != 0.0);
        let hz = match rate {
            RateSpec::Hz(hz) => hz,
            RateSpec::CyclesPerBeat(cpb) => cpb * ctx.tempo_bpm / 60.0,
        };
        let step = hz.max(0.0) / ctx.sample_rate.max(1.0);
        // A random level held per cycle is a stepped gain — a click at every
        // step — so a tremolo reads it as the sine instead.
        let wave = match waveform {
            Waveform::Random => Waveform::Sine,
            other => other,
        };
        let target = depth.clamp(0.0, 1.0);
        if !started {
            from = target;
        }
        let mut done = 0usize;
        while done < win.frames {
            let at = win.start + done;
            let seg = ctx.run_from(at, win.start + win.frames);
            // A wired depth follows its lane's line. A set one slides from the
            // depth last applied to the one set across the segment, so a
            // recompile that changes it does not click; the oscillator never
            // stops either way.
            let line = lane.and_then(|lane| ctx.lane_line(at, lane));
            for i in 0..seg {
                let d = match line {
                    Some(line) => line.at(at + i).clamp(0.0, 1.0),
                    None => from + (target - from) * (i + 1) as f64 / seg as f64,
                };
                let shape = wave
                    .shape((phase + step * i as f64).rem_euclid(1.0))
                    .unwrap_or(0.0);
                let gain = (1.0 - d * (1.0 - shape) * 0.5) as f32;
                for ch in 0..width {
                    let src = self.at(a, ch, win) + done + i;
                    let dst = self.at(out, ch, win) + done + i;
                    self.pool[dst] = self.pool[src] * gain;
                }
            }
            phase = (phase + step * seg as f64).rem_euclid(1.0);
            from = match line {
                Some(line) => line.at(at + seg).clamp(0.0, 1.0),
                None => target,
            };
            done += seg;
        }
        if let Some(held) = self.dsp.get_mut(state) {
            held.values[0] = phase;
            held.values[1] = from;
            held.values[2] = 1.0;
        }
    }

    /// One chunk of an [`AudioOp::Math`], channel by channel.
    #[allow(clippy::too_many_arguments)]
    fn audio_math(
        &mut self,
        out: Buf,
        a: Buf,
        b: Option<Buf>,
        op: AudioMathOp,
        state: usize,
        width: usize,
        win: Window,
        sample_rate: f64,
    ) {
        // The pole of a first-order DC blocker at `DC_CUTOFF_HZ`, from the
        // one-pole approximation `1 - 2πfc/fs` — exact enough three decades
        // below the sample rate, and cheaper than the exponential.
        let pole =
            (1.0 - std::f64::consts::TAU * DC_CUTOFF_HZ / sample_rate.max(1.0)).clamp(0.0, 1.0);
        for ch in 0..width {
            let from = self.at(a, ch, win);
            let to = self.at(out, ch, win);
            let with = b.map(|b| self.at(b, ch, win));
            let pool = &mut self.pool;
            match op {
                AudioMathOp::RemoveDc => {
                    let Some(held) = self.dsp.get_mut(state) else {
                        continue;
                    };
                    // `y[n] = x[n] - x[n-1] + pole * y[n-1]`, the previous
                    // input and output carried in the node's state.
                    let (mut x1, mut y1) = (held.values[2 * ch], held.values[2 * ch + 1]);
                    for i in 0..win.frames {
                        let x = f64::from(pool[from + i]);
                        let y = x - x1 + pole * y1;
                        pool[to + i] = y as f32;
                        (x1, y1) = (x, y);
                    }
                    held.values[2 * ch] = x1;
                    held.values[2 * ch + 1] = y1;
                }
                AudioMathOp::Invert => {
                    for i in 0..win.frames {
                        pool[to + i] = -pool[from + i];
                    }
                }
                AudioMathOp::Rectify => {
                    for i in 0..win.frames {
                        pool[to + i] = pool[from + i].abs();
                    }
                }
                AudioMathOp::Multiply => match with {
                    Some(with) => {
                        for i in 0..win.frames {
                            pool[to + i] = pool[from + i] * pool[with + i];
                        }
                    }
                    None => {
                        if to != from {
                            pool.copy_within(from..from + win.frames, to);
                        }
                    }
                },
            }
        }
    }

    /// Where this chunk of one channel of one buffer starts in the pool.
    ///
    /// Each buffer owns a region sized for the longest block, with the channels
    /// packed at the block's length. See [`Window`].
    pub(super) fn at(&self, buf: Buf, channel: usize, win: Window) -> usize {
        buf as usize * MAX_BUFFER_CHANNELS * self.stride + channel * win.block + win.start
    }

    pub(super) fn fill(&mut self, buf: Buf, win: Window, value: f32) {
        for ch in 0..MAX_CHANNELS {
            let start = self.at(buf, ch, win);
            self.pool[start..start + win.frames].fill(value);
        }
    }

    /// Reads samples from an audio delay line into `buf` with cubic Hermite interpolation.
    ///
    /// Smooths the read pointer across chunks to prevent clicks during delay
    /// modulation. `state` is the latch holding where the pointer stood at the
    /// end of the last chunk; see [`AudioOp::DelayRead`].
    fn delay_read(
        &mut self,
        line: usize,
        state: usize,
        buf: Buf,
        width: usize,
        win: Window,
        distance: f64,
    ) {
        let frames = win.frames;
        let ring_len = self.audio_lines.get(line).map_or(0, |held| held.len);
        if ring_len == 0 || self.audio_lines[line].ring.len() < MAX_CHANNELS * ring_len {
            self.fill(buf, win, 0.0);
            return;
        }
        // NaN for a read no program has run yet, which starts where it is
        // asked to. A previous position is held to what this chunk may read:
        // the ring may have shrunk under it, or the chunk grown past it.
        let from = match self.latches.get(state).map(|latch| latch.value) {
            Some(previous) if previous.is_finite() => previous
                .min(ring_len.saturating_sub(4) as f64)
                .max(frames as f64 + 2.0),
            _ => distance,
        };
        let head = self.audio_lines[line].head;
        for ch in 0..width.min(MAX_CHANNELS) {
            let ring = ch * ring_len;
            let to = self.at(buf, ch, win);
            for i in 0..frames {
                // The sweep lands exactly on `distance` at the last sample.
                let t = (i + 1) as f64 / frames as f64;
                let d = from + (distance - from) * t;
                let position = (head + i) as f64 - d;
                let whole = position.floor();
                let fraction = position - whole;
                let at = whole as i64;
                let y = |offset: i64| -> f32 {
                    let index = (at + offset).rem_euclid(ring_len as i64) as usize;
                    self.audio_lines[line].ring[ring + index]
                };
                self.pool[to + i] = hermite(y(-1), y(0), y(1), y(2), fraction as f32);
            }
        }
        if let Some(latch) = self.latches.get_mut(state) {
            latch.value = distance;
        }
    }

    /// Append this chunk of `buf` to `line`.
    ///
    /// Every read in the chunk has already run — the compiler holds the writes
    /// back for exactly that reason — so the head this advances is the one the
    /// reads saw, and a delay of one chunk reads the chunk before it rather
    /// than itself.
    fn delay_write(&mut self, line: usize, buf: Buf, width: usize, win: Window) {
        let frames = win.frames;
        let ring_len = self.audio_lines.get(line).map_or(0, |held| held.len);
        if ring_len == 0 || self.audio_lines[line].ring.len() < MAX_CHANNELS * ring_len {
            return;
        }
        let head = self.audio_lines[line].head;
        for ch in 0..MAX_CHANNELS {
            let ring = ch * ring_len;
            // A channel the source does not have still has to be written, or
            // the line would keep replaying whatever a wider patch left there.
            let from = self.at(buf, ch, win);
            for i in 0..frames {
                let at = (head + i) % ring_len;
                self.audio_lines[line].ring[ring + at] = if ch < width.min(MAX_CHANNELS) {
                    self.pool[from + i]
                } else {
                    0.0
                };
            }
        }
        self.audio_lines[line].head = (head + frames) % ring_len;
    }

    /// Advance `line`'s write head over this chunk without a source.
    ///
    /// The head moves at the rate a connected write would move it, so the line
    /// drains over its delay time rather than holding still.
    fn delay_silence(&mut self, line: usize, frames: usize) {
        let ring_len = self.audio_lines.get(line).map_or(0, |held| held.len);
        if ring_len == 0 || self.audio_lines[line].ring.len() < MAX_CHANNELS * ring_len {
            return;
        }
        let head = self.audio_lines[line].head;
        for ch in 0..MAX_CHANNELS {
            let ring = ch * ring_len;
            for i in 0..frames {
                self.audio_lines[line].ring[ring + (head + i) % ring_len] = 0.0;
            }
        }
        self.audio_lines[line].head = (head + frames) % ring_len;
    }

    /// Delays a buffer in place by a fixed sample count for latency compensation.
    fn compensate(&mut self, buf: Buf, slot: usize, samples: usize, width: usize, win: Window) {
        if slot >= MAX_COMPENSATORS || samples == 0 || samples >= MAX_COMPENSATION {
            return;
        }
        let frames = win.frames;
        let mut head = self.compensator_heads[slot];
        for ch in 0..width.min(MAX_CHANNELS) {
            // Every channel walks the same distance, so each starts from the
            // same head and only the last one leaves it moved.
            head = self.compensator_heads[slot];
            let ring = slot * MAX_CHANNELS * MAX_COMPENSATION + ch * MAX_COMPENSATION;
            let signal = self.at(buf, ch, win);
            for i in 0..frames {
                let read = (head + MAX_COMPENSATION - samples) % MAX_COMPENSATION;
                let delayed = self.compensators[ring + read];
                self.compensators[ring + head] = self.pool[signal + i];
                self.pool[signal + i] = delayed;
                head = (head + 1) % MAX_COMPENSATION;
            }
        }
        self.compensator_heads[slot] = head;
    }
}
