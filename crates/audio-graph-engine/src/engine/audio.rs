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
            let mut row = 0usize;
            while start < total {
                let len = step.min(total - start);
                self.run_chunk(
                    &program, stage, ctx, nodes, daw_in, daw_out, start, len, row,
                );
                start += len;
                row += 1;
            }
        }

        self.program = Some(program);
    }

    /// Where the rows `first..end` sit in note buffer `buf`.
    ///
    /// The buffer holds the whole block, so this is how the audio half asks
    /// for its own chunk's events without the note half having to run again.
    /// A row past what the parameter half has filled reads to the end, which
    /// is what makes the last chunk right whether or not the block divides
    /// evenly by the quantum.
    fn note_slice(&self, buf: u16, first: usize, end: usize) -> std::ops::Range<usize> {
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
        };
        from..to.max(from)
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
        row: usize,
    ) {
        let block = ctx.frames as usize;
        let Ok(schedule) = ScheduleView::from_parts(
            ctx.lanes,
            ctx.lanes_per_row,
            ctx.frames.div_ceil(ctx.quantum.max(1)) as usize,
            ctx.quantum,
            ctx.frames,
        ) else {
            daw_out.fill(0.0);
            return;
        };
        let mut tap = 0usize;
        // Which rows of the note buffers this chunk covers. The buffers were
        // filled by the parameter half and hold the whole block; a chunk is a
        // contiguous run of rows, so its events are a contiguous slice.
        let first_row = row;
        let end_row = row + frames.div_ceil((ctx.quantum as usize).max(1));
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
                    let heard = notes.map(|buf| self.note_slice(buf, first_row, end_row));
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
                        let gain = input
                            .lane
                            .and_then(|lane| ctx.lane(row, lane))
                            .map(|db| db_to_linear(db) as f32)
                            .unwrap_or(input.gain as f32);
                        for ch in 0..width.min(MAX_CHANNELS) {
                            let from = self.at(input.buf, ch, win);
                            let to = self.at(*out, ch, win);
                            if from == to && gain == 1.0 {
                                // Already in place and unchanged: unity gain on destination buffer.
                                continue;
                            }
                            for i in 0..frames {
                                let value = self.pool[from + i] * gain;
                                if n == 0 {
                                    self.pool[to + i] = value;
                                } else {
                                    self.pool[to + i] += value;
                                }
                            }
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
                    let quantum = (ctx.quantum as usize).max(1);
                    let rate = ctx.sample_rate.max(1.0);
                    // Where the last block left the ramp. NaN until it has
                    // ever run, which the first target resolves.
                    let mut from = self
                        .latches
                        .get(*state as usize)
                        .map(|latch| latch.value)
                        .unwrap_or(f64::NAN);
                    // One segment per sub-block the chunk covers, so a chunk
                    // that spans the whole block still follows the lane.
                    let mut target = *gain;
                    let mut done = 0usize;
                    while done < frames {
                        let at = start + done;
                        let seg = (quantum - at % quantum).min(frames - done);
                        // A row the lane grid does not reach holds the target
                        // where it was. Opening a gate because a block ran off
                        // the end of the schedule is not a defensible answer.
                        if let Some(value) = lane.and_then(|lane| ctx.lane(at / quantum, lane)) {
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
                    lane,
                    time,
                    max_time,
                } => {
                    let index = tap;
                    tap += 1;
                    let seconds = lane
                        .and_then(|lane| ctx.lane(row, lane))
                        .unwrap_or(*time)
                        .max(0.0);
                    let width = program.buffers[*out as usize] as usize;
                    self.delay_read(*line as usize, index, *out, width, win, {
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
    /// Smooths the read pointer across chunks to prevent clicks during delay modulation.
    fn delay_read(
        &mut self,
        line: usize,
        tap: usize,
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
        // NaN on the first chunk after a swap, and on the very first block.
        let from = match self.tap_distance.get(tap).copied() {
            Some(previous) if previous.is_finite() => previous,
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
        if let Some(slot) = self.tap_distance.get_mut(tap) {
            *slot = distance;
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
