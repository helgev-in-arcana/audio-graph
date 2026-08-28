//! Realtime audio engine execution runtime.
//!
//! Executes compiled [`Program`] instructions on the audio thread.
//! Guarantees realtime safety: no heap allocation, no locking, and no blocking operations.
//! State that persists across program recompilations (e.g. LFO phases, delay ring buffers,
//! latch registers, note expressions) is maintained here.

use plugin_host::{NoteEvent, NoteExpression};

use crate::handoff::Handoff;
use crate::ir::{
    AudioOp, Buf, Chunking, ExprSource, MAX_AUDIO_DELAY_LINES, MAX_BUFFER_CHANNELS, MAX_BUFFERS,
    MAX_CHANNELS, MAX_COMPENSATION, MAX_COMPENSATORS, MAX_DELAY_LINES, MAX_DELAY_TAPS, MAX_LATCHES,
    MAX_LFOS, MAX_REGISTERS, MathOp, Op, Operand, Program, RateSpec, Waveform,
};
use crate::nodes::db_to_linear;
use subhost_adapter::{AudioChunk, AudioInstances};

/// Maximum number of `DelayRead` taps supported in a single program.
///
/// Multiple delay reads can share the same underlying delay line. The engine tracks
/// the previous fractional read distance for each tap to support smooth interpolation
/// across sub-block boundaries without clicks.
pub const MAX_AUDIO_TAPS: usize = 16;

/// Sentinel indicating that no ring buffer currently holds this delay line.
const NOT_PRESENT: usize = usize::MAX;

/// Context for evaluating one parameter sub-block.
#[derive(Debug, Clone, Copy)]
pub struct BlockContext {
    pub sample_rate: f64,
    pub tempo_bpm: f64,
    /// Number of audio frames processed during this evaluation.
    pub frames: u32,
}

/// Context for evaluating one whole block of audio.
///
/// Contains automation lanes, chunking metadata, and sample rate.
#[derive(Debug, Clone, Copy)]
pub struct AudioContext<'a> {
    pub frames: u32,
    /// Sub-block chunk size in frames.
    pub quantum: u32,
    pub sample_rate: f64,
    pub lanes: &'a [f64],
    /// Number of lanes per sub-block row.
    pub lanes_per_row: usize,
}

impl AudioContext<'_> {
    fn lane(&self, row: usize, lane: u16) -> Option<f64> {
        self.lanes
            .get(row * self.lanes_per_row + lane as usize)
            .copied()
    }
}

/// Monophonic note controller values and expression state.
#[derive(Debug, Clone, Copy)]
struct Expressions {
    /// Indexed by `NoteExpression as usize`.
    values: [f64; 7],
    velocity: f64,
    key: f64,
    held: u32,
}

impl Default for Expressions {
    fn default() -> Self {
        Expressions {
            values: [1.0, 0.5, 0.0, 0.0, 0.0, 0.0, 0.0],
            velocity: 0.0,
            key: 0.5,
            held: 0,
        }
    }
}

fn expression_index(kind: NoteExpression) -> usize {
    match kind {
        NoteExpression::Volume => 0,
        NoteExpression::Pan => 1,
        NoteExpression::Tuning => 2,
        NoteExpression::Vibrato => 3,
        NoteExpression::Expression => 4,
        NoteExpression::Brightness => 5,
        NoteExpression::Pressure => 6,
    }
}

pub struct Engine {
    program: Option<Box<Program>>,
    registers: Vec<f64>,
    /// LFO phase, 0..1, per state index of the current program.
    phases: Vec<f64>,
    /// Sample-and-hold values, per state index.
    holds: Vec<f64>,
    /// Scratch buffer for preserving LFO phases across program swaps.
    carry: Vec<(u32, f64, f64)>,
    /// Node IDs associated with active LFO phases.
    phase_nodes: Vec<u32>,
    /// Parameter delay line ring buffers, indexed by line ID.
    /// Uses `Vec<Vec<f64>>` to allow lock-free outer pointer swapping during program
    /// swaps without copying large sample buffers.
    rings: Vec<Vec<f64>>,
    /// Current write head position per parameter delay line.
    ring_heads: Vec<usize>,
    /// Node IDs associated with active parameter delay lines.
    ring_nodes: Vec<u32>,
    /// Scratch buffer for reordering delay line buffers during swaps.
    ring_order: Vec<usize>,
    /// Preallocated audio buffer pool.
    pool: Vec<f32>,
    /// Stride in frames per channel buffer.
    stride: usize,
    /// Channel widths of host input buses.
    daw_inputs: Vec<u16>,
    /// Audio delay line ring buffers.
    audio_rings: Vec<Vec<f32>>,
    /// Samples per channel in each audio delay ring buffer.
    audio_ring_len: Vec<usize>,
    audio_ring_heads: Vec<usize>,
    audio_ring_nodes: Vec<u32>,
    audio_ring_order: Vec<usize>,
    /// Previous read pointer distances for audio delay taps.
    tap_distance: Vec<f64>,
    /// Ring buffers for delay latency compensation.
    compensators: Vec<f32>,
    compensator_heads: Vec<usize>,
    expressions: Expressions,
    /// Bitmask of actively held MIDI keys (128 keys).
    keys_held: u128,
    /// Bitmask of MIDI keys struck during the current evaluation block.
    keys_struck: u128,
    /// Persistent latch values for switch and stepped controls.
    latches: Vec<f64>,
    /// Node IDs associated with active latch states.
    latch_nodes: Vec<u32>,
    /// Scratch buffer for carrying latch values across program swaps.
    latch_carry: Vec<(u32, f64)>,
    rng: u32,
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new()
    }
}

/// Move a delay line's contents into a differently sized ring.
///
/// The samples that matter are the most recent ones, so the copy walks
/// backwards from the old head and lands them at the end of the new ring.
/// Anything that no longer fits is the oldest of it, which is the part a
/// shorter line was never going to read again anyway.
///
/// `head` comes in pointing into the old ring and goes out pointing into the
/// new one.
fn copy_ring(from: &[f32], from_len: usize, to: &mut [f32], to_len: usize, head: &mut usize) {
    if from_len == 0 || to_len == 0 {
        *head = 0;
        return;
    }
    let keep = from_len.min(to_len);
    for ch in 0..MAX_CHANNELS {
        let (src, dst) = (ch * from_len, ch * to_len);
        for i in 0..keep {
            // `keep` samples ending at the old head, laid down ending at the
            // new one — which is `keep`, since the new ring starts empty.
            let at = (*head + from_len - keep + i) % from_len;
            if src + at < from.len() && dst + i < to.len() {
                to[dst + i] = from[src + at];
            }
        }
    }
    *head = keep % to_len;
}

/// Four-point cubic Hermite polynomial interpolator.
///
/// Used for fractional delay line read pointer interpolation. `x` is the fractional
/// offset between `y1` and `y2`.
fn hermite(y0: f32, y1: f32, y2: f32, y3: f32, x: f32) -> f32 {
    let c1 = 0.5 * (y2 - y0);
    let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
    let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
    ((c3 * x + c2) * x + c1) * x + y1
}

/// Move each ring to the index the new program gave its node, contents intact.
///
/// Work out the permutation first, then apply it by swapping the outer `Vec`
/// entries — no allocation, and no copying of ring contents, which for an audio
/// line is 96 000 samples a channel.
fn reorder<T: Copy>(
    rings: &mut [Vec<T>],
    heads: &mut [usize],
    nodes: &mut [u32],
    order: &mut [usize],
    want: &[u32],
    zero: T,
) {
    let lines = want.len().min(rings.len());
    for (i, slot) in order[..lines].iter_mut().enumerate() {
        *slot = nodes
            .iter()
            .position(|&n| n == want[i])
            .unwrap_or(NOT_PRESENT);
    }

    // Move the surviving rings into place first. Clearing as we went would wipe
    // a ring that is still sitting in a slot some later line wants.
    for i in 0..lines {
        let from = order[i];
        // `from` is never below `i`: slots below `i` already hold the rings of
        // earlier lines, whose nodes are all different from this one's.
        if from == NOT_PRESENT || from == i {
            continue;
        }
        rings.swap(i, from);
        heads.swap(i, from);
        // Whatever was at `i` now sits at `from`; a line still pointing at `i`
        // has to follow it there.
        for slot in order[i + 1..lines].iter_mut() {
            if *slot == i {
                *slot = from;
            }
        }
        order[i] = i;
    }
    // Whatever is left in a new line's slot belonged to a line that is gone.
    for i in 0..lines {
        if order[i] == NOT_PRESENT {
            rings[i].fill(zero);
            heads[i] = 0;
        }
        nodes[i] = want[i];
    }
    for node in nodes[lines..].iter_mut() {
        *node = u32::MAX;
    }
}

impl Engine {
    pub fn new() -> Engine {
        Engine {
            program: None,
            registers: vec![0.0; MAX_REGISTERS],
            phases: vec![0.0; MAX_LFOS],
            holds: vec![0.0; MAX_LFOS],
            carry: vec![(0, 0.0, 0.0); MAX_LFOS],
            phase_nodes: vec![u32::MAX; MAX_LFOS],
            rings: (0..MAX_DELAY_LINES)
                .map(|_| vec![0.0; MAX_DELAY_TAPS])
                .collect(),
            ring_heads: vec![0; MAX_DELAY_LINES],
            ring_nodes: vec![u32::MAX; MAX_DELAY_LINES],
            ring_order: vec![0; MAX_DELAY_LINES],
            // Empty until a program with a delay line in it arrives, and then
            // only as long as that line asked for.
            audio_rings: (0..MAX_AUDIO_DELAY_LINES).map(|_| Vec::new()).collect(),
            audio_ring_len: vec![0; MAX_AUDIO_DELAY_LINES],
            audio_ring_heads: vec![0; MAX_AUDIO_DELAY_LINES],
            audio_ring_nodes: vec![u32::MAX; MAX_AUDIO_DELAY_LINES],
            audio_ring_order: vec![0; MAX_AUDIO_DELAY_LINES],
            tap_distance: vec![f64::NAN; MAX_AUDIO_TAPS],
            pool: Vec::new(),
            daw_inputs: Vec::new(),
            stride: 0,
            compensators: Vec::new(),
            compensator_heads: vec![0; MAX_COMPENSATORS],
            expressions: Expressions::default(),
            keys_held: 0,
            keys_struck: 0,
            latches: vec![f64::NAN; MAX_LATCHES],
            latch_nodes: vec![u32::MAX; MAX_LATCHES],
            latch_carry: vec![(u32::MAX, f64::NAN); MAX_LATCHES],
            // Any odd seed; the sequence only has to be uncorrelated, not
            // unpredictable.
            rng: 0x2545_F491,
        }
    }

    /// Whether the graph currently drives `lane` — see
    /// [`Program::drives_lane`].
    pub fn drives_lane(&self, lane: usize) -> bool {
        self.program.as_ref().is_some_and(|p| p.drives_lane(lane))
    }

    pub fn has_program(&self) -> bool {
        self.program.as_ref().is_some_and(|p| !p.is_empty())
    }

    /// Picks up a newly compiled program if one is waiting in the handoff channel.
    ///
    /// Returns `true` if a new program was adopted. Realtime-safe: does not allocate or lock.
    pub fn adopt(&mut self, handoff: &Handoff<Program>) -> bool {
        // Remember which node each running phase belongs to *before* the swap;
        // afterwards the old program is gone.
        let live = self.phase_nodes.len().min(self.phases.len());
        for i in 0..live {
            self.carry[i] = (self.phase_nodes[i], self.phases[i], self.holds[i]);
        }
        let carried = self
            .program
            .as_ref()
            .map_or(0, |p| p.lfo_nodes.len().min(MAX_LFOS));

        if !handoff.take(&mut self.program) {
            return false;
        }
        let next = self.program.as_ref().expect("take reported a swap");

        for (i, &node) in next.lfo_nodes.iter().take(MAX_LFOS).enumerate() {
            // Linear over at most MAX_LFOS entries.
            match self.carry[..carried].iter().find(|&&(id, _, _)| id == node) {
                Some(&(_, phase, hold)) => {
                    self.phases[i] = phase;
                    self.holds[i] = hold;
                }
                None => {
                    self.phases[i] = 0.0;
                    self.holds[i] = 0.0;
                }
            }
            self.phase_nodes[i] = node;
        }
        for i in next.lfo_nodes.len()..MAX_LFOS {
            self.phase_nodes[i] = u32::MAX;
        }

        // Latches keep their values across the swap so user switch settings persist.
        let latched = self
            .latch_carry
            .len()
            .min(self.latch_nodes.len())
            .min(self.latches.len());
        for i in 0..latched {
            self.latch_carry[i] = (self.latch_nodes[i], self.latches[i]);
        }
        for (i, &node) in next.latch_nodes.iter().take(MAX_LATCHES).enumerate() {
            self.latches[i] = self.latch_carry[..latched]
                .iter()
                .find(|&&(id, _)| id == node)
                .map_or(f64::NAN, |&(_, value)| value);
            self.latch_nodes[i] = node;
        }
        for i in next.latch_nodes.len()..MAX_LATCHES {
            self.latch_nodes[i] = u32::MAX;
            self.latches[i] = f64::NAN;
        }

        // Delay line ring buffers retain their contents across program swaps.
        reorder(
            &mut self.rings,
            &mut self.ring_heads,
            &mut self.ring_nodes,
            &mut self.ring_order,
            &next.delay_nodes,
            0.0,
        );
        reorder(
            &mut self.audio_rings,
            &mut self.audio_ring_heads,
            &mut self.audio_ring_nodes,
            &mut self.audio_ring_order,
            &next.audio_delay_nodes,
            0.0,
        );
        // When ring lengths change, new buffers provided by the main thread are swapped in.
        let next = self.program.as_mut().expect("take reported a swap");
        for line in 0..next.audio_delay_nodes.len().min(MAX_AUDIO_DELAY_LINES) {
            let len = next.audio_ring_len.get(line).copied().unwrap_or(0);
            if next.audio_rings.get(line).is_some_and(|r| !r.is_empty()) {
                std::mem::swap(&mut self.audio_rings[line], &mut next.audio_rings[line]);
                // Carry over what will still fit, most recent samples last.
                let from = &next.audio_rings[line];
                copy_ring(
                    from,
                    self.audio_ring_len[line],
                    &mut self.audio_rings[line],
                    len,
                    &mut self.audio_ring_heads[line],
                );
                self.audio_ring_len[line] = len;
            } else if self.audio_ring_len[line] != len {
                self.audio_ring_len[line] = 0;
            }
        }
        for line in next.audio_delay_nodes.len()..MAX_AUDIO_DELAY_LINES {
            self.audio_ring_len[line] = 0;
        }
        self.tap_distance.iter_mut().for_each(|d| *d = f64::NAN);
        true
    }

    /// Give back whatever program is loaded, so the main thread can free it.
    ///
    /// Called when the plugin is torn down, from the main thread.
    pub fn release(&mut self) -> Option<Box<Program>> {
        self.program.take()
    }

    /// Fold one note event into the expression state.
    pub fn note(&mut self, event: &NoteEvent) {
        match *event {
            NoteEvent::NoteOn { key, velocity, .. } => {
                self.expressions.velocity = velocity;
                self.expressions.key = f64::from(key).clamp(0.0, 127.0) / 127.0;
                self.expressions.held = self.expressions.held.saturating_add(1);
                if let Some(bit) = key_bit(key) {
                    self.keys_held |= bit;
                    self.keys_struck |= bit;
                }
            }
            NoteEvent::NoteOff { key, .. } | NoteEvent::NoteEnd { key, .. } => {
                self.expressions.held = self.expressions.held.saturating_sub(1);
                if let Some(bit) = key_bit(key) {
                    self.keys_held &= !bit;
                }
            }
            NoteEvent::Expression {
                expression, value, ..
            } => {
                self.expressions.values[expression_index(expression)] = value;
            }
            NoteEvent::Midi { .. } => {}
        }
    }

    /// Clears held notes and resets internal phase/delay head counters on host transport jump.
    ///
    /// This intentionally does NOT clear key switch latches, because latches should survive
    /// transport jumps (e.g. seeking in the DAW timeline).
    pub fn reset(&mut self) {
        self.expressions = Expressions::default();
        self.keys_held = 0;
        self.keys_struck = 0;
        self.phases.iter_mut().for_each(|p| *p = 0.0);
        self.rings.iter_mut().for_each(|r| r.fill(0.0));
        self.ring_heads.iter_mut().for_each(|h| *h = 0);
        self.pool.fill(0.0);
        self.compensators.fill(0.0);
        self.compensator_heads.iter_mut().for_each(|h| *h = 0);
        // Audio delay rings are sized and allocated by the main thread via Program::size_rings.
        self.audio_ring_heads.iter_mut().for_each(|h| *h = 0);
    }

    /// Allocates and sizes audio buffers for worst-case limits. Called from the main thread on activation.
    pub fn prepare(&mut self, max_frames: u32, daw_inputs: &[u16]) {
        self.stride = max_frames as usize;
        self.daw_inputs.clear();
        self.daw_inputs.extend_from_slice(daw_inputs);
        self.pool.clear();
        self.pool
            .resize(MAX_BUFFERS * MAX_BUFFER_CHANNELS * self.stride, 0.0);
        self.compensators.clear();
        self.compensators
            .resize(MAX_COMPENSATORS * MAX_CHANNELS * MAX_COMPENSATION, 0.0);
        self.compensator_heads.iter_mut().for_each(|h| *h = 0);
        // Audio delay rings are sized and allocated by the main thread via Program::size_rings.
        self.audio_ring_heads.iter_mut().for_each(|h| *h = 0);
    }

    /// Returns the latency in samples reported by the compiled program.
    pub fn latency(&self) -> u32 {
        self.program.as_ref().map_or(0, |p| p.latency)
    }

    /// Returns whether the current program contains audio operations.
    pub fn has_audio(&self) -> bool {
        self.program
            .as_ref()
            .is_some_and(|p| !p.audio_ops.is_empty())
    }

    /// Returns the evaluation chunking granularity required by the active program.
    pub fn chunking(&self) -> Chunking {
        self.program
            .as_ref()
            .map_or(Chunking::WholeBlock, |p| p.chunking)
    }

    /// Executes the audio pipeline for a block provided by the audio host.
    ///
    /// Evaluates operations at whole-block or sub-block chunking depending on whether
    /// audio feedback delay loops are present.
    pub fn run_audio(
        &mut self,
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

        let step = match program.chunking {
            Chunking::WholeBlock => total.max(1),
            // The last chunk is short whenever the block is not a multiple of the quantum.
            Chunking::SubBlock => (ctx.quantum as usize).max(1),
        };
        let mut start = 0usize;
        let mut row = 0usize;
        while start < total {
            let len = step.min(total - start);
            self.run_chunk(&program, ctx, nodes, daw_in, daw_out, start, len, row);
            start += len;
            row += 1;
        }

        self.program = Some(program);
    }

    /// One chunk of `run_audio`: every op, over `len` frames starting at
    /// `start` inside the DAW's block.
    #[allow(clippy::too_many_arguments)]
    fn run_chunk(
        &mut self,
        program: &Program,
        ctx: &AudioContext<'_>,
        nodes: &mut dyn AudioInstances,
        daw_in: &[f32],
        daw_out: &mut [f32],
        start: usize,
        frames: usize,
        row: usize,
    ) {
        let block = ctx.frames as usize;
        let mut tap = 0usize;
        for op in &program.audio_ops {
            match op {
                AudioOp::Silence { out } => self.fill(*out, frames, 0.0),
                AudioOp::Input { out, bus } => {
                    let width = program.buffers[*out as usize] as usize;
                    // daw_in holds interleaved planar buses.
                    let bus = *bus as usize;
                    let Some(&have) = self.daw_inputs.get(bus) else {
                        self.fill(*out, frames, 0.0);
                        continue;
                    };
                    // The DAW's buffer is packed at the *block* length; the
                    // pool is packed at the chunk's. This op is where the two
                    // meet, and where `start` stops being the engine's problem.
                    let base: usize = self.daw_inputs[..bus]
                        .iter()
                        .map(|&c| c as usize * block)
                        .sum();
                    for ch in 0..width.min(MAX_CHANNELS) {
                        let to = self.at(*out, ch, frames);
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
                        let from = self.at(*a, ch, frames);
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
                            let to = self.at(*out, at + ch as usize, frames);
                            if have == 1 && want > 1 {
                                // Mono into a wider bus: the same signal on
                                // every channel, which is what a host does.
                                let src = self.at(from, 0, frames);
                                self.pool.copy_within(src..src + frames, to);
                            } else if want == 1 && have > 1 {
                                // Wider into mono: summed. A sidechain detector
                                // wants both channels to count, and taking the
                                // left one would silently ignore half the
                                // signal.
                                let first = self.at(from, 0, frames);
                                self.pool.copy_within(first..first + frames, to);
                                for other in 1..have {
                                    let src = self.at(from, other as usize, frames);
                                    for i in 0..frames {
                                        self.pool[to + i] += self.pool[src + i];
                                    }
                                }
                            } else if ch < have {
                                let src = self.at(from, ch as usize, frames);
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
                        let src = self.at(*from, *channel as usize + ch, frames);
                        let dst = self.at(*out, ch, frames);
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
                    // The gate lane is sampled per chunk.
                    let notes = notes.resolve(notes.gate.and_then(|lane| ctx.lane(row, lane)));
                    nodes.process(
                        *instance,
                        notes,
                        &source[..packed_in],
                        &mut dest[..packed_out],
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
                    );
                }
                AudioOp::Mix { out, inputs } => {
                    if inputs.is_empty() {
                        self.fill(*out, frames, 0.0);
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
                            let from = self.at(input.buf, ch, frames);
                            let to = self.at(*out, ch, frames);
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
                AudioOp::Compensate { buf, slot, samples } => {
                    let width = program.buffers[*buf as usize] as usize;
                    self.compensate(*buf, *slot as usize, *samples as usize, width, frames);
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
                    self.delay_read(*line as usize, index, *out, width, frames, {
                        // Minimum floor in samples, plus the two samples the
                        // interpolator needs ahead of the read pointer.
                        let floor = frames as f64 + 2.0;
                        let ceiling = (max_time * ctx.sample_rate)
                            .min(self.audio_ring_len[*line as usize].saturating_sub(4) as f64)
                            .max(floor);
                        (seconds * ctx.sample_rate).clamp(floor, ceiling)
                    });
                }
                AudioOp::DelayWrite { line, a } => {
                    let width = program.buffers[*a as usize] as usize;
                    self.delay_write(*line as usize, *a, width, frames);
                }
            }
        }
    }

    /// Where one channel of one buffer starts in the pool.
    ///
    /// Each buffer owns a region sized for the longest block; the channels
    /// inside it are packed at `frames`, so the region is always big enough and
    /// the packed part is exactly what a sub-plugin expects to be handed.
    fn at(&self, buf: Buf, channel: usize, frames: usize) -> usize {
        buf as usize * MAX_BUFFER_CHANNELS * self.stride + channel * frames
    }

    fn fill(&mut self, buf: Buf, frames: usize, value: f32) {
        for ch in 0..MAX_CHANNELS {
            let start = self.at(buf, ch, frames);
            self.pool[start..start + frames].fill(value);
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
        frames: usize,
        distance: f64,
    ) {
        let ring_len = self.audio_ring_len.get(line).copied().unwrap_or(0);
        if ring_len == 0 || self.audio_rings[line].len() < MAX_CHANNELS * ring_len {
            self.fill(buf, frames, 0.0);
            return;
        }
        // NaN on the first chunk after a swap, and on the very first block.
        let from = match self.tap_distance.get(tap).copied() {
            Some(previous) if previous.is_finite() => previous,
            _ => distance,
        };
        let head = self.audio_ring_heads[line];
        for ch in 0..width.min(MAX_CHANNELS) {
            let ring = ch * ring_len;
            let to = self.at(buf, ch, frames);
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
                    self.audio_rings[line][ring + index]
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
    fn delay_write(&mut self, line: usize, buf: Buf, width: usize, frames: usize) {
        let ring_len = self.audio_ring_len.get(line).copied().unwrap_or(0);
        if ring_len == 0 || self.audio_rings[line].len() < MAX_CHANNELS * ring_len {
            return;
        }
        let head = self.audio_ring_heads[line];
        for ch in 0..MAX_CHANNELS {
            let ring = ch * ring_len;
            // A channel the source does not have still has to be written, or
            // the line would keep replaying whatever a wider patch left there.
            let from = self.at(buf, ch, frames);
            for i in 0..frames {
                let at = (head + i) % ring_len;
                self.audio_rings[line][ring + at] = if ch < width.min(MAX_CHANNELS) {
                    self.pool[from + i]
                } else {
                    0.0
                };
            }
        }
        self.audio_ring_heads[line] = (head + frames) % ring_len;
    }

    /// Delays a buffer in place by a fixed sample count for latency compensation.
    fn compensate(&mut self, buf: Buf, slot: usize, samples: usize, width: usize, frames: usize) {
        if slot >= MAX_COMPENSATORS || samples == 0 || samples >= MAX_COMPENSATION {
            return;
        }
        let mut head = self.compensator_heads[slot];
        for ch in 0..width.min(MAX_CHANNELS) {
            // Every channel walks the same distance, so each starts from the
            // same head and only the last one leaves it moved.
            head = self.compensator_heads[slot];
            let ring = slot * MAX_CHANNELS * MAX_COMPENSATION + ch * MAX_COMPENSATION;
            let signal = self.at(buf, ch, frames);
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

    /// Evaluates parameter operations for one sub-block.
    ///
    /// Overwrites slot table values for lanes driven by the graph.
    pub fn run(&mut self, ctx: &BlockContext, slots: &mut [f64]) {
        // Moved out and put back rather than borrowed.
        let Some(program) = self.program.take() else {
            return;
        };
        if program.is_empty() {
            self.program = Some(program);
            return;
        }

        let dt = if ctx.sample_rate > 0.0 {
            f64::from(ctx.frames) / ctx.sample_rate
        } else {
            0.0
        };

        // Parameter delay distance converted to sub-block taps.
        let taps_per_second = if ctx.frames > 0 && ctx.sample_rate > 0.0 {
            ctx.sample_rate / f64::from(ctx.frames)
        } else {
            0.0
        };

        for op in &program.ops {
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
                    let index = line as usize;
                    self.registers[out as usize] = if index < self.rings.len() {
                        let taps = (time * taps_per_second)
                            .round()
                            .clamp(1.0, (MAX_DELAY_TAPS - 1) as f64)
                            as usize;
                        let head = self.ring_heads[index];
                        let at = (head + MAX_DELAY_TAPS - taps) % MAX_DELAY_TAPS;
                        self.rings[index][at]
                    } else {
                        0.0
                    };
                }
                Op::DelayWrite { line, a } => {
                    let index = line as usize;
                    if index < self.rings.len() {
                        let head = self.ring_heads[index];
                        self.rings[index][head] = self.registers[a as usize];
                        self.ring_heads[index] = (head + 1) % MAX_DELAY_TAPS;
                    }
                }
                Op::Const { out, value } => self.registers[out as usize] = value,
                Op::Slot { out, slot } => {
                    self.registers[out as usize] = slots.get(slot as usize).copied().unwrap_or(0.0)
                }
                Op::Expr { out, source } => {
                    self.registers[out as usize] = read_expression(&self.expressions, source)
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
                    let phase = (self.phases[i] + offset_phase).rem_euclid(1.0);
                    let shape = match waveform {
                        Waveform::Sine => (phase * std::f64::consts::TAU).sin(),
                        Waveform::Triangle => 1.0 - 4.0 * (phase - 0.5).abs(),
                        Waveform::Saw => 2.0 * phase - 1.0,
                        Waveform::Square => {
                            if phase < 0.5 {
                                -1.0
                            } else {
                                1.0
                            }
                        }
                        Waveform::Random => self.holds[i],
                    };
                    self.registers[out as usize] = centre + depth * shape;

                    let hz = match rate {
                        RateSpec::Hz(hz) => hz,
                        RateSpec::CyclesPerBeat(cpb) => cpb * ctx.tempo_bpm / 60.0,
                    };
                    let advanced = self.phases[i] + hz * dt;
                    if advanced >= 1.0 && waveform == Waveform::Random {
                        self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        self.holds[i] = f64::from(self.rng >> 8) / f64::from(1u32 << 23) - 1.0;
                    }
                    self.phases[i] = advanced.rem_euclid(1.0);
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
                Op::KeyHeld { out, key } => {
                    self.registers[out as usize] = f64::from(self.held(key));
                }
                Op::KeyStep { state, key, count } => {
                    if self.struck(key)
                        && count > 0
                        && let Some(latch) = self.latches.get_mut(state as usize)
                    {
                        let at = if latch.is_nan() { 0.0 } else { *latch };
                        *latch = (at + 1.0).rem_euclid(f64::from(count));
                    }
                }
                Op::KeyLatch { state, key, value } => {
                    if self.struck(key)
                        && let Some(latch) = self.latches.get_mut(state as usize)
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
                        .copied()
                        .unwrap_or(f64::NAN);
                    let at = if at.is_nan() { initial } else { at };
                    self.registers[out as usize] = f64::from(at == value);
                }
                Op::Latch {
                    out,
                    state,
                    initial,
                } => {
                    let value = self
                        .latches
                        .get(state as usize)
                        .copied()
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

        for &(lane, reg) in &program.outputs {
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

        self.keys_struck = 0;

        self.program = Some(program);
    }

    /// Whether `key` is down. Out of range is never down.
    fn held(&self, key: u8) -> bool {
        key_bit(i16::from(key)).is_some_and(|bit| self.keys_held & bit != 0)
    }

    /// Whether `key` has been struck since the last evaluation.
    fn struck(&self, key: u8) -> bool {
        key_bit(i16::from(key)).is_some_and(|bit| self.keys_struck & bit != 0)
    }
}

/// One key's bit in the held/struck tables, or `None` for a key outside the
/// MIDI range — which a malformed event can carry and a bit shift cannot.
fn key_bit(key: i16) -> Option<u128> {
    (0..128).contains(&key).then(|| 1u128 << key)
}

fn read_expression(state: &Expressions, source: ExprSource) -> f64 {
    match source {
        ExprSource::Volume => state.values[0],
        ExprSource::Pan => state.values[1],
        ExprSource::Tuning => state.values[2],
        ExprSource::Vibrato => state.values[3],
        ExprSource::Expression => state.values[4],
        ExprSource::Brightness => state.values[5],
        ExprSource::Pressure => state.values[6],
        ExprSource::Velocity => state.velocity,
        ExprSource::Gate => f64::from(u8::from(state.held > 0)),
        ExprSource::KeyTrack => state.key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::compile;
    use crate::graph::{Graph, NodeId};
    use crate::ir::MathOp;
    use crate::nodes::{
        AudioIn, AudioOut, Constant, DelayRead, DelayWrite, Expression, Gate, KeyParam,
        KeyParamMode, KeySwitch, KeySwitchMode, Lfo, Math, Mix, NodeKind, ParamPort, Plugin,
        PluginPorts, Rate, SlotIn, Switch, linear_to_db,
    };
    use crate::port::PortType;
    use subhost_adapter::NoteStream;

    const SLOTS: usize = 32;

    /// Helper creating a parameter sink plugin node.
    fn param_sink(graph: &mut Graph) -> NodeId {
        graph.add(
            NodeKind::Plugin(Plugin {
                instance: 0,
                ports: PluginPorts {
                    params: vec![ParamPort {
                        id: 0,
                        name: "p".into(),
                    }],
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        )
    }

    /// The lane [`param_sink`]'s parameter is driven through.
    const SINK: usize = SLOTS;

    /// A lane row: the slot table, and the sink's lane after it.
    fn lanes() -> Vec<f64> {
        vec![0.0; SLOTS + 1]
    }

    fn ctx(frames: u32) -> BlockContext {
        BlockContext {
            sample_rate: 48_000.0,
            tempo_bpm: 120.0,
            frames,
        }
    }

    /// The rate the audio tests run at. Real rather than convenient, because
    /// the delay rings are sized in seconds and a fake rate would make a
    /// sensible `max_time` come out as four samples.
    const RATE: f64 = 48_000.0;

    /// Samples, as the seconds a delay node wants.
    fn seconds(samples: f64) -> f64 {
        samples / RATE
    }

    /// A context for a test with no automation in it.
    fn audio_ctx(frames: u32) -> AudioContext<'static> {
        AudioContext {
            frames,
            quantum: 32,
            sample_rate: RATE,
            lanes: &[],
            lanes_per_row: 0,
        }
    }

    fn load(engine: &mut Engine, graph: &Graph) {
        let handoff = Handoff::new();
        let mut program = compile(graph, SLOTS).unwrap();
        // What the wrapper's `publish_graph` does, and for the same reason: the
        // rings are allocated on this side and ride over with the program.
        program.size_rings(RATE, &[]);
        handoff.send(Box::new(program));
        assert!(engine.adopt(&handoff));
    }

    #[test]
    fn a_lane_the_graph_does_not_drive_keeps_the_daws_value() {
        let mut graph = Graph::new();
        let c = graph.add(NodeKind::Constant(Constant { value: 0.25 }), [0.0, 0.0]);
        let out = param_sink(&mut graph);
        graph.connect(c, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = vec![0.9; SLOTS + 1];
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.25);
        assert_eq!(slots[1], 0.9, "an undriven slot is left alone");
    }

    #[test]
    fn the_daws_automation_can_be_read_shaped_and_written_back() {
        let mut graph = Graph::new();
        let input = graph.add(NodeKind::SlotIn(SlotIn { slot: 3 }), [0.0, 0.0]);
        let half = graph.add(
            NodeKind::Math(Math {
                op: MathOp::Multiply,
                b: 0.5,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(input, 0, half, 0);
        graph.connect(half, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        slots[3] = 0.8;
        engine.run(&ctx(32), &mut slots);
        assert!((slots[SINK] - 0.4).abs() < 1e-12);
    }

    /// The parameter half's switch: one value below the threshold, another at
    /// it and above.
    #[test]
    fn a_switch_picks_by_threshold() {
        let mut graph = Graph::new();
        let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 1 }), [0.0, 0.0]);
        let switch = graph.add(
            NodeKind::Switch(Switch {
                values: vec![0.2, 0.9],
                thresholds: vec![0.6],
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(control, 0, switch, 0);
        graph.connect(switch, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        slots[1] = 0.59;
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.2);

        slots[1] = 0.6;
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.9, "the threshold itself is on");
    }

    /// More than two rungs: the last threshold the control has passed is the
    /// one that wins, and below all of them the first value is what is read.
    #[test]
    fn a_switch_climbs_a_ladder_of_thresholds() {
        let mut graph = Graph::new();
        let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 1 }), [0.0, 0.0]);
        let switch = graph.add(
            NodeKind::Switch(Switch {
                values: vec![0.1, 0.4, 0.7],
                thresholds: vec![0.3, 0.8],
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(control, 0, switch, 0);
        graph.connect(switch, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        for (control, expected) in [(0.0, 0.1), (0.29, 0.1), (0.3, 0.4), (0.79, 0.4), (0.8, 0.7)] {
            slots[1] = control;
            engine.run(&ctx(32), &mut slots);
            assert_eq!(slots[SINK], expected, "at {control}");
        }
    }

    /// Either side of a switch can be a signal rather than a number, which is
    /// what makes it a router as well as a chooser.
    #[test]
    fn a_switch_can_pick_between_two_signals() {
        let mut graph = Graph::new();
        let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 1 }), [0.0, 0.0]);
        let a = graph.add(NodeKind::SlotIn(SlotIn { slot: 2 }), [0.0, 0.0]);
        let b = graph.add(NodeKind::SlotIn(SlotIn { slot: 3 }), [0.0, 0.0]);
        let switch = graph.add(
            NodeKind::Switch(Switch {
                values: vec![0.0, 1.0],
                thresholds: vec![0.5],
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(control, 0, switch, 0);
        graph.connect(a, 0, switch, 1);
        graph.connect(b, 0, switch, 2);
        graph.connect(switch, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        slots[2] = 0.25;
        slots[3] = 0.75;
        slots[1] = 0.0;
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.25);

        slots[2] = 0.25;
        slots[3] = 0.75;
        slots[1] = 1.0;
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.75);
    }

    #[test]
    fn an_lfo_sweeps_and_comes_back() {
        let mut graph = Graph::new();
        let lfo = graph.add(
            NodeKind::Lfo(Lfo {
                waveform: Waveform::Saw,
                rate: Rate::Hz(1.0),
                phase: 0.0,
                depth: 0.5,
                offset: 0.5,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(lfo, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        let mut seen: Vec<f64> = Vec::new();
        // One second at 48 kHz in 32-sample sub-blocks: a whole cycle.
        for _ in 0..1500 {
            engine.run(&ctx(32), &mut slots);
            seen.push(slots[SINK]);
        }
        let lowest = seen.iter().cloned().fold(f64::INFINITY, f64::min);
        let highest = seen.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        assert!(lowest < 0.02, "a saw should reach the bottom, got {lowest}");
        assert!(highest > 0.98, "a saw should reach the top, got {highest}");
        assert!(seen.iter().all(|v| (0.0..=1.0).contains(v)));
    }

    #[test]
    fn tempo_sync_follows_the_host() {
        let mut graph = Graph::new();
        let lfo = graph.add(
            NodeKind::Lfo(Lfo {
                waveform: Waveform::Saw,
                // One cycle per beat: at 120 bpm that is 2 Hz.
                rate: Rate::Beats(1.0),
                phase: 0.0,
                depth: 0.5,
                offset: 0.5,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(lfo, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        // Quarter of a beat at 120 bpm = 0.125 s = 6000 samples.
        let mut slots = lanes();
        engine.run(
            &BlockContext {
                sample_rate: 48_000.0,
                tempo_bpm: 120.0,
                frames: 6000,
            },
            &mut slots,
        );
        engine.run(
            &BlockContext {
                sample_rate: 48_000.0,
                tempo_bpm: 120.0,
                frames: 1,
            },
            &mut slots,
        );
        assert!(
            (slots[SINK] - 0.25).abs() < 1e-3,
            "expected a quarter cycle, got {}",
            slots[SINK]
        );
    }

    /// Helper creating a parameter feedback loop test graph.
    fn feedback_graph(time: f64) -> (Graph, NodeId) {
        let mut graph = Graph::new();
        let seed = graph.add(NodeKind::SlotIn(SlotIn { slot: 1 }), [0.0, 0.0]);
        let read = graph.add(
            NodeKind::DelayRead(DelayRead {
                line: 0,
                ty: PortType::Param,
                max_time: 1.0,
                time,
            }),
            [0.0, 0.0],
        );
        // The loop: (input + what came back) * 0.5, written back to the line.
        let mixed = graph.add(
            NodeKind::Math(Math {
                op: MathOp::Add,
                b: 0.0,
            }),
            [0.0, 0.0],
        );
        let decayed = graph.add(
            NodeKind::Math(Math {
                op: MathOp::Multiply,
                b: 0.5,
            }),
            [0.0, 0.0],
        );
        let write = graph.add(
            NodeKind::DelayWrite(DelayWrite {
                line: 0,
                ty: PortType::Param,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);

        graph.connect(seed, 0, mixed, 0);
        graph.connect(read, 0, mixed, 1);
        graph.connect(mixed, 0, decayed, 0);
        graph.connect(decayed, 0, write, 0);
        graph.connect(decayed, 0, out, 0);
        (graph, write)
    }

    #[test]
    fn a_delay_line_carries_a_value_round_the_loop() {
        // One sub-block of delay, so each run reads exactly what the previous
        // one wrote.
        let (graph, _) = feedback_graph(32.0 / 48_000.0);
        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        slots[1] = 1.0;
        engine.run(&ctx(32), &mut slots);
        // (1 + 0) * 0.5
        assert!(
            (slots[SINK] - 0.5).abs() < 1e-9,
            "first pass: {}",
            slots[SINK]
        );

        slots[1] = 1.0;
        engine.run(&ctx(32), &mut slots);
        // (1 + 0.5) * 0.5 — the 0.5 came back round.
        assert!(
            (slots[SINK] - 0.75).abs() < 1e-9,
            "second pass: {}",
            slots[SINK]
        );
    }

    /// Verifies that recompiling does not clear parameter delay line state.
    #[test]
    fn recompiling_does_not_empty_a_delay_line() {
        let (mut graph, _) = feedback_graph(32.0 / 48_000.0);
        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        slots[1] = 1.0;
        engine.run(&ctx(32), &mut slots);
        assert!((slots[SINK] - 0.5).abs() < 1e-9);

        // An unrelated node appears, as it does on any edit.
        graph.add(NodeKind::Constant(Constant { value: 0.0 }), [0.0, 0.0]);
        load(&mut engine, &graph);

        slots[1] = 1.0;
        engine.run(&ctx(32), &mut slots);
        assert!(
            (slots[SINK] - 0.75).abs() < 1e-9,
            "the line was emptied by the swap: {}",
            slots[SINK]
        );
    }

    /// Verifies that parameter delay times below one sub-block are clamped to the minimum floor.
    #[test]
    fn a_delay_shorter_than_a_sub_block_is_held_at_one() {
        let (graph, _) = feedback_graph(0.0);
        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        slots[1] = 1.0;
        engine.run(&ctx(32), &mut slots);
        slots[1] = 1.0;
        engine.run(&ctx(32), &mut slots);
        assert!(
            (slots[SINK] - 0.75).abs() < 1e-9,
            "a zero time should behave as one sub-block, not as zero: {}",
            slots[SINK]
        );
    }

    /// The DAW's buffer size is not the sub-block size, and the loop is defined
    /// in sub-blocks. Two runs of 32 must land where one run of 64 does not.
    #[test]
    fn the_loop_is_measured_in_sub_blocks_not_daw_blocks() {
        let (graph, _) = feedback_graph(2.0 * 32.0 / 48_000.0);
        let run = |frames: u32, passes: usize| {
            let mut engine = Engine::new();
            load(&mut engine, &graph);
            let mut slots = lanes();
            for _ in 0..passes {
                slots[1] = 1.0;
                engine.run(&ctx(frames), &mut slots);
            }
            slots[SINK]
        };
        // Same sub-block size, same answer, however the DAW hands us the block.
        assert!((run(32, 4) - run(32, 4)).abs() < 1e-12);
        // Two sub-blocks of delay: nothing has come back yet after two passes.
        assert!((run(32, 2) - 0.5).abs() < 1e-9);
        // By the third, it has.
        assert!(run(32, 3) > 0.5);
    }

    /// A stand-in for the sub-plugins, so the engine's routing can be tested
    /// without one. Each instance adds its own number to every sample, which
    /// makes the order it ran in readable off the output.
    struct Adders;

    impl AudioInstances for Adders {
        fn process(
            &mut self,
            instance: u32,
            _notes: NoteStream,
            input: &[f32],
            output: &mut [f32],
            chunk: AudioChunk,
        ) {
            for ch in 0..chunk.output_channels {
                let range = chunk.channel(ch);
                for (o, i) in output[range.clone()].iter_mut().zip(input[range].iter()) {
                    *o = *i + (instance + 1) as f32;
                }
            }
        }
    }

    fn audio_plugin(graph: &mut Graph, instance: usize, latency: u32) -> NodeId {
        graph.add(
            NodeKind::Plugin(Plugin {
                instance,
                ports: PluginPorts {
                    audio_in: vec![2],
                    audio_out: vec![2],
                    audio_out_shown: Vec::new(),
                    latency,
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        )
    }

    #[test]
    fn audio_runs_through_two_plugins_in_order() {
        let mut graph = Graph::new();
        let input = graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        let first = audio_plugin(&mut graph, 0, 0);
        let second = audio_plugin(&mut graph, 1, 0);
        let output = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, first, 0);
        graph.connect(first, 0, second, 0);
        graph.connect(second, 0, output, 0);

        let mut engine = Engine::new();
        engine.prepare(64, &[2]);
        load(&mut engine, &graph);

        let daw_in = vec![10.0f32; 2 * 8];
        let mut daw_out = vec![0.0f32; 2 * 8];
        engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);

        // 10, then +1 from instance 0, then +2 from instance 1.
        assert!(
            daw_out.iter().all(|&s| (s - 13.0).abs() < 1e-6),
            "{daw_out:?}"
        );
    }

    /// Verifies latency compensation delay aligns parallel audio branches.
    #[test]
    fn a_compensated_branch_arrives_with_the_late_one() {
        let mut graph = Graph::new();
        let input = graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        // Latency 4, but the stand-in does not actually delay: what is being
        // tested is that the *other* branch is delayed by the same 4.
        let slow = audio_plugin(&mut graph, 0, 4);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 2,
                // Empty is unity: what a mix did before it had gains.
                gains: Vec::new(),
            }),
            [0.0, 0.0],
        );
        let output = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, slow, 0);
        graph.connect(slow, 0, mix, 0);
        graph.connect(input, 0, mix, 2);
        graph.connect(mix, 0, output, 0);

        let mut engine = Engine::new();
        engine.prepare(64, &[2]);
        load(&mut engine, &graph);

        // An impulse on the first sample of each channel.
        let mut daw_in = vec![0.0f32; 2 * 8];
        daw_in[0] = 1.0;
        daw_in[8] = 1.0;
        let mut daw_out = vec![0.0f32; 2 * 8];
        engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);

        // The wet branch is the stand-in: input + 1, so the impulse shows as
        // 2.0 at sample 0 and 1.0 everywhere else. The dry branch is held back
        // 4 samples, so its impulse lands at sample 4 and nowhere else.
        assert!((daw_out[0] - 2.0).abs() < 1e-6, "wet at 0: {}", daw_out[0]);
        assert!(
            (daw_out[4] - 2.0).abs() < 1e-6,
            "dry arrives at 4: {}",
            daw_out[4]
        );
        assert!(
            (daw_out[1] - 1.0).abs() < 1e-6,
            "quiet between: {}",
            daw_out[1]
        );
    }

    /// The patch a new instance starts with has to be the through-connection
    /// the wrapper used to make for itself, or every fresh instance is silent.
    #[test]
    fn the_default_patch_passes_audio_through() {
        let mut engine = Engine::new();
        engine.prepare(64, &[2]);
        load(&mut engine, &Graph::default_patch());

        let daw_in: Vec<f32> = (0..2 * 8).map(|i| i as f32 * 0.1).collect();
        let mut daw_out = vec![0.0f32; 2 * 8];
        engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
        assert_eq!(daw_out, daw_in);
    }

    #[test]
    fn an_unconnected_output_leaves_the_daw_buffer_alone() {
        let mut graph = Graph::new();
        graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        let mut engine = Engine::new();
        engine.prepare(64, &[2]);
        load(&mut engine, &graph);

        let daw_in = vec![0.0f32; 2 * 8];
        let mut daw_out = vec![7.0f32; 2 * 8];
        engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
        assert!(daw_out.iter().all(|&s| s == 7.0));
    }

    /// `prepare` is the only thing that allocates, so running without it — or
    /// with a longer block than promised — has to be a no-op rather than a
    /// panic or a read past the end.
    #[test]
    fn running_audio_unprepared_does_nothing() {
        let mut graph = Graph::new();
        let input = graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        let output = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, output, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);
        let daw_in = vec![1.0f32; 2 * 8];
        let mut daw_out = vec![0.0f32; 2 * 8];
        engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
        assert!(daw_out.iter().all(|&s| s == 0.0));

        engine.prepare(4, &[2]);
        engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
        assert!(
            daw_out.iter().all(|&s| s == 0.0),
            "8 frames were promised 4"
        );
    }

    #[test]
    fn recompiling_does_not_restart_a_running_lfo() {
        let mut graph = Graph::new();
        let lfo = graph.add(
            NodeKind::Lfo(Lfo {
                waveform: Waveform::Saw,
                rate: Rate::Hz(1.0),
                phase: 0.0,
                depth: 0.5,
                offset: 0.5,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(lfo, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        for _ in 0..200 {
            engine.run(&ctx(32), &mut slots);
        }
        let before = slots[SINK];
        assert!(before > 0.05, "the LFO should have moved by now");

        // Something unrelated changes — a new node appears — and the graph is
        // recompiled, as it is on every edit.
        graph.add(NodeKind::Constant(Constant { value: 0.0 }), [0.0, 0.0]);
        load(&mut engine, &graph);
        engine.run(&ctx(1), &mut slots);

        assert!(
            (slots[SINK] - before).abs() < 0.01,
            "the phase jumped across a recompile: {before} -> {}",
            slots[SINK]
        );
    }

    #[test]
    fn note_expression_reaches_the_graph() {
        let mut graph = Graph::new();
        let expr = graph.add(
            NodeKind::Expression(Expression {
                source: ExprSource::Pressure,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(expr, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = lanes();
        engine.note(&NoteEvent::Expression {
            note_id: 1,
            port: 0,
            channel: 0,
            key: 60,
            expression: NoteExpression::Pressure,
            value: 0.7,
            sample_offset: 0,
        });
        engine.run(&ctx(32), &mut slots);
        assert!((slots[SINK] - 0.7).abs() < 1e-12);
    }

    /// A key switch watches one key, whatever has been played since — which
    /// is exactly what `Expression`'s sources cannot answer.
    #[test]
    fn a_held_key_switch_follows_its_own_key() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let switch = graph.add(
            NodeKind::KeySwitch(KeySwitch {
                keys: vec![24],
                mode: KeySwitchMode::Hold,
                mute_keys: true,
            }),
            [0.0, 0.0],
        );
        let synth = graph.add(
            NodeKind::Plugin(Plugin {
                instance: 0,
                ports: PluginPorts {
                    audio_out: vec![2],
                    audio_out_shown: Vec::new(),
                    accepts_notes: true,
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        );
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(notes, 0, switch, 0);
        graph.connect(switch, 0, synth, 0);
        graph.connect(synth, 0, out, 0);

        let mut engine = Engine::new();
        engine.prepare(8, &[]);
        load(&mut engine, &graph);

        let program = compile(&graph, SLOTS).unwrap();
        let lane = program
            .audio_ops
            .iter()
            .find_map(|op| match op {
                AudioOp::Plugin { notes, .. } => notes.gate,
                _ => None,
            })
            .expect("the key switch booked a gate lane") as usize;

        let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
        let mut lanes = vec![0.0; width];
        engine.run(&ctx(8), &mut lanes);
        assert_eq!(lanes[lane], 0.0, "nothing is held yet");

        engine.note(&NoteEvent::NoteOn {
            note_id: 1,
            port: 0,
            channel: 0,
            key: 24,
            velocity: 1.0,
            sample_offset: 0,
        });
        engine.run(&ctx(8), &mut lanes);
        assert_eq!(lanes[lane], 1.0, "the switch key is down");

        // A different key coming and going must not move it.
        engine.note(&NoteEvent::NoteOn {
            note_id: 2,
            port: 0,
            channel: 0,
            key: 60,
            velocity: 1.0,
            sample_offset: 0,
        });
        engine.note(&NoteEvent::NoteOff {
            note_id: 2,
            port: 0,
            channel: 0,
            key: 60,
            velocity: 0.0,
            sample_offset: 0,
        });
        engine.run(&ctx(8), &mut lanes);
        assert_eq!(lanes[lane], 1.0, "another key came and went");

        engine.note(&NoteEvent::NoteOff {
            note_id: 1,
            port: 0,
            channel: 0,
            key: 24,
            velocity: 0.0,
            sample_offset: 0,
        });
        engine.run(&ctx(8), &mut lanes);
        assert_eq!(lanes[lane], 0.0, "let go");
    }

    /// A toggling switch moves on each strike and stays where it was put —
    /// including across the recompile that every edit causes.
    #[test]
    fn a_toggling_key_switch_latches_and_survives_a_recompile() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let switch = graph.add(
            NodeKind::KeySwitch(KeySwitch {
                keys: vec![24, 25],
                mode: KeySwitchMode::Toggle,
                mute_keys: true,
            }),
            [0.0, 0.0],
        );
        let synth = graph.add(
            NodeKind::Plugin(Plugin {
                instance: 0,
                ports: PluginPorts {
                    audio_out: vec![2],
                    audio_out_shown: Vec::new(),
                    accepts_notes: true,
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        );
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(notes, 0, switch, 0);
        graph.connect(switch, 1, synth, 0);
        graph.connect(synth, 0, out, 0);

        let mut engine = Engine::new();
        engine.prepare(8, &[]);
        load(&mut engine, &graph);

        let lane = compile(&graph, SLOTS)
            .unwrap()
            .audio_ops
            .iter()
            .find_map(|op| match op {
                AudioOp::Plugin { notes, .. } => notes.gate,
                _ => None,
            })
            .expect("output b got a gate lane") as usize;

        let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
        let mut lanes = vec![0.0; width];
        let strike = NoteEvent::NoteOn {
            note_id: 1,
            port: 0,
            channel: 0,
            key: 24,
            velocity: 1.0,
            sample_offset: 0,
        };

        engine.run(&ctx(8), &mut lanes);
        assert_eq!(lanes[lane], 0.0, "b is shut until the switch is thrown");

        engine.note(&strike);
        engine.run(&ctx(8), &mut lanes);
        assert_eq!(lanes[lane], 1.0, "thrown");
        engine.run(&ctx(8), &mut lanes);
        assert_eq!(lanes[lane], 1.0, "and it stays thrown");

        // An unrelated edit, and the recompile it causes.
        graph.add(NodeKind::Constant(Constant { value: 0.0 }), [0.0, 0.0]);
        load(&mut engine, &graph);
        engine.run(&ctx(8), &mut lanes);
        assert_eq!(lanes[lane], 1.0, "a recompile must not move the switch");

        engine.note(&strike);
        engine.run(&ctx(8), &mut lanes);
        assert_eq!(lanes[lane], 0.0, "thrown back");
    }

    /// One key stepping a parameter through its values, and staying where it
    /// was put. With two values that is a plain toggle.
    #[test]
    fn a_key_parameter_toggles_between_two_values() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let key = graph.add(
            NodeKind::KeyParam(KeyParam {
                mode: KeyParamMode::Toggle,
                keys: vec![24, 25],
                values: vec![0.2, 0.8],
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(notes, 0, key, 0);
        graph.connect(key, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);
        let mut slots = lanes();
        let strike = NoteEvent::NoteOn {
            note_id: 1,
            port: 0,
            channel: 0,
            key: 24,
            velocity: 1.0,
            sample_offset: 0,
        };

        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.2, "untouched, it reads its first value");

        engine.note(&strike);
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.8);
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.8, "one strike is one step, not one per run");

        engine.note(&strike);
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.2, "and round again");
    }

    /// A bank of keys, one value each: the last one struck wins, which is what
    /// a row of switches does.
    #[test]
    fn a_key_parameter_selects_by_the_last_key_struck() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let key = graph.add(
            NodeKind::KeyParam(KeyParam {
                mode: KeyParamMode::Select,
                keys: vec![24, 25, 26],
                values: vec![0.25, 0.5, 1.0],
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(notes, 0, key, 0);
        graph.connect(key, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);
        let mut slots = lanes();
        let strike = |key: i16| NoteEvent::NoteOn {
            note_id: key as i32,
            port: 0,
            channel: 0,
            key,
            velocity: 1.0,
            sample_offset: 0,
        };

        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.25);

        engine.note(&strike(25));
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.5);

        engine.note(&strike(26));
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 1.0);

        // A key the bank does not name changes nothing.
        engine.note(&strike(60));
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 1.0);
    }

    /// Nothing wired to the notes port means no keys are read at all. An
    /// unwired node that quietly followed the keyboard anyway would be a node
    /// whose links say nothing about what it does.
    #[test]
    fn a_key_parameter_with_no_notes_wired_stays_put() {
        let mut graph = Graph::new();
        let key = graph.add(
            NodeKind::KeyParam(KeyParam {
                mode: KeyParamMode::Select,
                keys: vec![24, 25],
                values: vec![0.25, 0.75],
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(key, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);
        let mut slots = lanes();

        engine.note(&NoteEvent::NoteOn {
            note_id: 1,
            port: 0,
            channel: 0,
            key: 25,
            velocity: 1.0,
            sample_offset: 0,
        });
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.25);
    }

    /// A value socket wins over the number on its row, the same way `Math`'s
    /// `b` gives way to its input — so a key switch can pick between two
    /// signals, not only two numbers.
    #[test]
    fn a_key_parameter_value_can_be_a_signal() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let signal = graph.add(NodeKind::SlotIn(SlotIn { slot: 3 }), [0.0, 0.0]);
        let key = graph.add(
            NodeKind::KeyParam(KeyParam {
                mode: KeyParamMode::Select,
                keys: vec![24, 25],
                values: vec![0.25, 0.75],
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(notes, 0, key, 0);
        // Socket 0 is the notes port, so value 2 is socket 2.
        graph.connect(signal, 0, key, 2);
        graph.connect(key, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);
        let mut slots = lanes();
        slots[3] = 0.6;
        engine.note(&NoteEvent::NoteOn {
            note_id: 1,
            port: 0,
            channel: 0,
            key: 25,
            velocity: 1.0,
            sample_offset: 0,
        });
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.6, "the wired socket wins over the number");
    }

    #[test]
    fn the_gate_follows_held_notes() {
        let mut graph = Graph::new();
        let gate = graph.add(
            NodeKind::Expression(Expression {
                source: ExprSource::Gate,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(gate, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);
        let mut slots = lanes();

        let on = |key: i16| NoteEvent::NoteOn {
            note_id: key as i32,
            port: 0,
            channel: 0,
            key,
            velocity: 1.0,
            sample_offset: 0,
        };
        let off = |key: i16| NoteEvent::NoteOff {
            note_id: key as i32,
            port: 0,
            channel: 0,
            key,
            velocity: 0.0,
            sample_offset: 0,
        };

        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.0);

        engine.note(&on(60));
        engine.note(&on(64));
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 1.0);

        // Releasing one of two held notes must not drop the gate.
        engine.note(&off(60));
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 1.0);

        engine.note(&off(64));
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[SINK], 0.0);
    }

    #[test]
    fn a_degenerate_graph_cannot_hand_a_nan_to_the_sub_plugin() {
        let mut graph = Graph::new();
        let a = graph.add(NodeKind::Constant(Constant { value: 0.0 }), [0.0, 0.0]);
        let div = graph.add(
            NodeKind::Math(Math {
                op: MathOp::Curve,
                b: 0.0,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(a, 0, div, 0);
        graph.connect(div, 0, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);
        let mut slots = vec![0.5; SLOTS + 1];
        engine.run(&ctx(32), &mut slots);
        assert!(slots[SINK].is_finite());
        assert!((0.0..=1.0).contains(&slots[SINK]));
    }

    #[test]
    fn an_engine_with_no_program_leaves_everything_alone() {
        let mut engine = Engine::new();
        let mut slots = vec![0.3; SLOTS];
        engine.run(&ctx(32), &mut slots);
        assert!(slots.iter().all(|&v| v == 0.3));
        assert!(!engine.has_program());
    }

    fn stereo_in(graph: &mut Graph) -> NodeId {
        graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        )
    }

    fn stereo_out(graph: &mut Graph) -> NodeId {
        graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        )
    }

    /// The two halves of an audio delay line, on line 0, `samples` back.
    fn audio_delay(graph: &mut Graph, samples: f64) -> (NodeId, NodeId) {
        let write = graph.add(
            NodeKind::DelayWrite(DelayWrite {
                line: 0,
                ty: PortType::STEREO,
            }),
            [0.0, 0.0],
        );
        let read = graph.add(
            NodeKind::DelayRead(DelayRead {
                line: 0,
                ty: PortType::STEREO,
                // Room for the sweeps, without asking for a megabyte of ring.
                max_time: 0.05,
                time: seconds(samples),
            }),
            [0.0, 0.0],
        );
        (write, read)
    }

    /// An impulse on both channels of a stereo block.
    fn impulse(frames: usize, at: usize) -> Vec<f32> {
        let mut daw_in = vec![0.0f32; 2 * frames];
        daw_in[at] = 1.0;
        daw_in[frames + at] = 1.0;
        daw_in
    }

    /// Verifies that an audio delay line outputs samples delayed by the expected duration.
    #[test]
    fn an_audio_delay_returns_what_it_was_given_a_delay_later() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let (write, read) = audio_delay(&mut graph, 64.0);
        graph.connect(input, 0, write, 0);
        graph.connect(read, 0, output, 0);

        let mut engine = Engine::new();
        engine.prepare(128, &[2]);
        load(&mut engine, &graph);

        // One impulse in the first block, then silence.
        let mut heard = Vec::new();
        for block in 0..3 {
            let daw_in = if block == 0 {
                impulse(128, 8)
            } else {
                vec![0.0; 2 * 128]
            };
            let mut daw_out = vec![0.0f32; 2 * 128];
            engine.run_audio(&audio_ctx(128), &daw_in, &mut daw_out, &mut Adders);
            heard.extend_from_slice(&daw_out[..128]);
        }

        let peak = heard
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .unwrap();
        assert_eq!(peak.0, 8 + 64, "the impulse comes back 64 samples later");
        assert!((peak.1 - 1.0).abs() < 1e-3, "and at its original height");
    }

    /// Verifies that delay times shorter than chunk length are clamped to minimum safe distance.
    #[test]
    fn a_delay_shorter_than_a_chunk_is_held_at_the_chunk() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let (write, read) = audio_delay(&mut graph, 1.0);
        graph.connect(input, 0, write, 0);
        graph.connect(read, 0, output, 0);

        let mut engine = Engine::new();
        engine.prepare(128, &[2]);
        load(&mut engine, &graph);

        let mut heard = Vec::new();
        for block in 0..2 {
            let daw_in = if block == 0 {
                impulse(128, 0)
            } else {
                vec![0.0; 2 * 128]
            };
            let mut daw_out = vec![0.0f32; 2 * 128];
            engine.run_audio(&audio_ctx(128), &daw_in, &mut daw_out, &mut Adders);
            heard.extend_from_slice(&daw_out[..128]);
        }
        let peak = heard
            .iter()
            .position(|v| v.abs() > 0.5)
            .expect("the impulse came back");
        // The quantum is 32, plus the two samples the interpolator needs ahead
        // of the read pointer. Asked for 1: a delay of 1 would have read this
        // chunk's own writes.
        assert_eq!(peak, 34);
    }

    /// Verifies consistent delay feedback behavior across different host block sizes.
    #[test]
    fn a_feedback_loop_sounds_the_same_at_any_block_size() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let (write, read) = audio_delay(&mut graph, 64.0);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 2,
                // Empty is unity: what a mix did before it had gains.
                gains: Vec::new(),
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, mix, 0);
        graph.connect(read, 0, mix, 2);
        graph.connect(mix, 0, output, 0);
        graph.connect(mix, 0, write, 0);

        let render = |block: usize| -> Vec<f32> {
            let mut engine = Engine::new();
            engine.prepare(512, &[2]);
            load(&mut engine, &graph);
            let mut heard = Vec::new();
            let mut at = 0;
            while at < 512 {
                let mut daw_in = vec![0.0f32; 2 * block];
                if at == 0 {
                    daw_in[0] = 1.0;
                    daw_in[block] = 1.0;
                }
                let mut daw_out = vec![0.0f32; 2 * block];
                engine.run_audio(&audio_ctx(block as u32), &daw_in, &mut daw_out, &mut Adders);
                heard.extend_from_slice(&daw_out[..block]);
                at += block;
            }
            heard
        };

        let big = render(512);
        let small = render(64);
        assert_eq!(big.len(), small.len());
        let worst = big
            .iter()
            .zip(&small)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 1e-6, "largest difference {worst}");
        assert!(
            big.iter().filter(|v| v.abs() > 0.5).count() >= 4,
            "the loop repeated"
        );
    }

    /// Verifies that modulating delay time does not alter sub-plugin processing chunk count.
    #[test]
    fn moving_the_delay_time_does_not_change_how_often_a_plugin_runs() {
        struct Counting(usize);
        impl AudioInstances for Counting {
            fn process(
                &mut self,
                _instance: u32,
                _notes: NoteStream,
                _input: &[f32],
                output: &mut [f32],
                chunk: AudioChunk,
            ) {
                self.0 += 1;
                for ch in 0..chunk.output_channels {
                    output[chunk.channel(ch)].fill(0.0);
                }
            }
        }

        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let plugin = audio_plugin(&mut graph, 0, 0);
        let (write, read) = audio_delay(&mut graph, 64.0);
        let time = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
        graph.connect(input, 0, plugin, 0);
        graph.connect(plugin, 0, write, 0);
        graph.connect(read, 0, output, 0);
        graph.connect(time, 0, read, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let lane = program
            .audio_ops
            .iter()
            .find_map(|op| match op {
                AudioOp::DelayRead { lane, .. } => *lane,
                _ => None,
            })
            .expect("the wired time control got a lane") as usize;

        let run = |seconds: f64| -> usize {
            let mut engine = Engine::new();
            engine.prepare(128, &[2]);
            load(&mut engine, &graph);
            let lanes_per_row = lane + 1;
            let lanes = vec![seconds; lanes_per_row * 4];
            let mut counting = Counting(0);
            engine.run_audio(
                &AudioContext {
                    frames: 128,
                    quantum: 32,
                    sample_rate: RATE,
                    lanes: &lanes,
                    lanes_per_row,
                },
                &vec![0.0; 2 * 128],
                &mut vec![0.0; 2 * 128],
                &mut counting,
            );
            counting.0
        };
        assert_eq!(run(seconds(64.0)), run(seconds(400.0)));
        assert_eq!(
            run(seconds(64.0)),
            4,
            "one call per sub-block, because of the loop"
        );
    }

    /// Verifies continuous tape-style pitch modulation during delay time sweeps without clicks.
    ///
    /// The signal written into the line is a ramp of one per sample, so what
    /// comes out is `t - d(t)` and the step between output samples is the
    /// playback speed. Holding the time still gives a step of exactly 1; a
    /// time shortening by a quarter of a sample per sample gives 1.25, which is
    /// the pitch moving up.
    #[test]
    fn sweeping_the_delay_time_moves_the_pitch_without_a_step() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let (write, read) = audio_delay(&mut graph, 200.0);
        let time = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
        graph.connect(input, 0, write, 0);
        graph.connect(read, 0, output, 0);
        graph.connect(time, 0, read, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let lane = program
            .audio_ops
            .iter()
            .find_map(|op| match op {
                AudioOp::DelayRead { lane, .. } => *lane,
                _ => None,
            })
            .expect("the wired time control got a lane") as usize;
        let lanes_per_row = lane + 1;

        let mut engine = Engine::new();
        engine.prepare(128, &[2]);
        load(&mut engine, &graph);

        let mut heard = Vec::new();
        let mut clock = 0.0f32;
        // Four blocks of ramp at a fixed 300 samples back to fill the line,
        // then four with the time sweeping from 300 to 268.
        for block in 0..8 {
            let mut daw_in = vec![0.0f32; 2 * 128];
            for i in 0..128 {
                daw_in[i] = clock;
                daw_in[128 + i] = clock;
                clock += 1.0;
            }
            let mut lanes = vec![0.0f64; lanes_per_row * 4];
            for row in 0..4 {
                let swept = (block - 4).max(0) as f64 * 4.0 + row as f64;
                lanes[row * lanes_per_row + lane] = seconds(300.0 - swept * 8.0);
            }
            let mut daw_out = vec![0.0f32; 2 * 128];
            engine.run_audio(
                &AudioContext {
                    frames: 128,
                    quantum: 32,
                    sample_rate: RATE,
                    lanes: &lanes,
                    lanes_per_row,
                },
                &daw_in,
                &mut daw_out,
                &mut Adders,
            );
            if block >= 4 {
                heard.extend_from_slice(&daw_out[..128]);
            }
        }

        // The first sub-block of the sweep is still coming up to speed: it
        // ramps from where the held pointer was, so it is the one chunk whose
        // step is 1. Everything after it is at the sweep's own rate.
        let steps: Vec<f32> = heard[32..].windows(2).map(|w| w[1] - w[0]).collect();
        // Eight samples of sweep every 32 of output: a quarter faster. Every
        // step, not just the average — one step out of line is what a click is.
        let worst = steps
            .iter()
            .map(|s| (s - 1.25).abs())
            .fold(0.0f32, f32::max);
        assert!(worst < 0.02, "largest departure from 1.25 was {worst}");
    }

    /// A mix of one input is a gain, which is the whole reason the gains live
    /// on `Mix` rather than on a node of their own.
    #[test]
    fn a_mix_of_one_is_a_gain() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let gain = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 1,
                gains: vec![linear_to_db(0.25)],
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, gain, 0);
        graph.connect(gain, 0, output, 0);

        let mut engine = Engine::new();
        engine.prepare(64, &[2]);
        load(&mut engine, &graph);

        let daw_in: Vec<f32> = (0..2 * 8).map(|i| i as f32).collect();
        let mut daw_out = vec![0.0f32; 2 * 8];
        engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
        let want: Vec<f32> = daw_in.iter().map(|v| v * 0.25).collect();
        assert_eq!(daw_out, want);
    }

    /// Each input has its own gain, and the sum is of the scaled ones. This is
    /// what turns a feedback loop's gain down below unity so it decays.
    #[test]
    fn each_mix_input_is_scaled_before_the_sum() {
        let mut graph = Graph::new();
        let a = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 2,
                gains: vec![linear_to_db(0.5), linear_to_db(0.25)],
            }),
            [0.0, 0.0],
        );
        // The same source into both inputs: 0.5 + 0.25 of it should come out.
        graph.connect(a, 0, mix, 0);
        graph.connect(a, 0, mix, 2);
        graph.connect(mix, 0, output, 0);

        let mut engine = Engine::new();
        engine.prepare(64, &[2]);
        load(&mut engine, &graph);

        let daw_in = vec![4.0f32; 2 * 8];
        let mut daw_out = vec![0.0f32; 2 * 8];
        engine.run_audio(&audio_ctx(8), &daw_in, &mut daw_out, &mut Adders);
        assert!(
            daw_out.iter().all(|&v| (v - 3.0).abs() < 1e-6),
            "{daw_out:?}"
        );
    }

    /// A gate is a `Mix` of one whose gain the parameter half switches, and
    /// this is the whole round trip: the control lands in a lane, the lane
    /// becomes a gain, the gain is unity or silence.
    #[test]
    fn a_gate_passes_or_silences_by_its_control() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
        let gate = graph.add(
            NodeKind::Gate(Gate {
                channels: 2,
                threshold: 0.5,
                invert: false,
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, gate, 0);
        graph.connect(control, 0, gate, 1);
        graph.connect(gate, 0, output, 0);

        let mut engine = Engine::new();
        engine.prepare(8, &[2]);
        load(&mut engine, &graph);

        let width = SLOTS + crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES;
        let mut render = |control: f64| -> Vec<f32> {
            let mut lanes = vec![0.0; width];
            lanes[0] = control;
            engine.run(&ctx(8), &mut lanes);
            let daw_in = vec![1.0f32; 2 * 8];
            let mut daw_out = vec![0.0f32; 2 * 8];
            engine.run_audio(
                &AudioContext {
                    frames: 8,
                    quantum: 32,
                    sample_rate: RATE,
                    lanes: &lanes,
                    lanes_per_row: width,
                },
                &daw_in,
                &mut daw_out,
                &mut Adders,
            );
            daw_out
        };

        assert!(
            render(1.0).iter().all(|&v| (v - 1.0).abs() < 1e-6),
            "an open gate is unity gain"
        );
        assert!(
            render(0.0).iter().all(|&v| v.abs() < 1e-6),
            "a shut gate is silence"
        );
    }

    /// When a gain socket is driven by a parameter source, the parameter value
    /// is interpreted as decibels and converted to a linear multiplier for audio.
    #[test]
    fn a_driven_gain_socket_interprets_its_value_as_decibels() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let gain_ctl = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 1,
                gains: vec![0.0],
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, mix, 0);
        // Sockets alternate: input 1 is socket 0, gain 1 is socket 1.
        graph.connect(gain_ctl, 0, mix, 1);
        graph.connect(mix, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let lane = program
            .audio_ops
            .iter()
            .find_map(|op| match op {
                AudioOp::Mix { inputs, .. } => inputs[0].lane,
                _ => None,
            })
            .expect("the wired gain control got a lane") as usize;
        let lanes_per_row = lane + 1;
        let lanes = vec![-6.0; lanes_per_row];

        let mut engine = Engine::new();
        engine.prepare(8, &[2]);
        load(&mut engine, &graph);

        let daw_in = vec![1.0f32; 2 * 8];
        let mut daw_out = vec![0.0f32; 2 * 8];
        engine.run_audio(
            &AudioContext {
                frames: 8,
                quantum: 8,
                sample_rate: RATE,
                lanes: &lanes,
                lanes_per_row,
            },
            &daw_in,
            &mut daw_out,
            &mut Adders,
        );

        let want_linear = db_to_linear(-6.0) as f32;
        assert!(
            daw_out.iter().all(|&v| (v - want_linear).abs() < 1e-6),
            "expected {want_linear}, got {daw_out:?}"
        );
    }

    /// Verifies that resizing max delay time allocates a larger ring buffer while preserving existing samples.
    #[test]
    fn a_longer_max_time_gets_a_longer_ring_and_keeps_what_was_in_it() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let (write, read) = audio_delay(&mut graph, 200.0);
        graph.connect(input, 0, write, 0);
        graph.connect(read, 0, output, 0);

        let mut program = compile(&graph, SLOTS).unwrap();
        let sized = program.size_rings(RATE, &[]);
        // 0.05 s at 48 kHz, plus the interpolator's headroom.
        assert_eq!(program.audio_ring_len, vec![2404]);
        assert_eq!(program.audio_rings[0].len(), MAX_CHANNELS * 2404);

        // Publishing again with nothing changed hands over no ring at all.
        let mut again = compile(&graph, SLOTS).unwrap();
        let sized_again = again.size_rings(RATE, &sized);
        assert!(
            again.audio_rings[0].is_empty(),
            "an unchanged line is left alone"
        );
        assert_eq!(sized_again, sized);

        let mut engine = Engine::new();
        engine.prepare(128, &[2]);
        let handoff = Handoff::new();
        handoff.send(Box::new(program));
        assert!(engine.adopt(&handoff));

        let daw_in = impulse(128, 8);
        let mut daw_out = vec![0.0f32; 2 * 128];
        engine.run_audio(&audio_ctx(128), &daw_in, &mut daw_out, &mut Adders);

        // Now ask for four times the range. The ring has to be reallocated on
        // the main thread, and the impulse still in it has to survive.
        if let Some(NodeKind::DelayRead(DelayRead { max_time, .. })) =
            graph.node_mut(read).map(|n| &mut n.kind)
        {
            *max_time = 0.2;
        }
        let mut wider = compile(&graph, SLOTS).unwrap();
        wider.size_rings(RATE, &sized);
        assert_eq!(wider.audio_ring_len, vec![9604]);
        assert!(
            !wider.audio_rings[0].is_empty(),
            "a changed line gets a new ring"
        );
        let handoff = Handoff::new();
        handoff.send(Box::new(wider));
        assert!(engine.adopt(&handoff));

        let mut daw_out = vec![0.0f32; 2 * 128];
        engine.run_audio(
            &audio_ctx(128),
            &vec![0.0; 2 * 128],
            &mut daw_out,
            &mut Adders,
        );
        assert_eq!(
            daw_out[..128]
                .iter()
                .position(|v| v.abs() > 0.9)
                .expect("the impulse came through the reallocation"),
            8 + 200 - 128
        );
    }

    /// Verifies that recompiling a patch preserves audio delay buffer contents.
    #[test]
    fn a_recompile_leaves_the_line_full() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        // Longer than a block, so the impulse is still inside the line when
        // the swap happens.
        let (write, read) = audio_delay(&mut graph, 200.0);
        graph.connect(input, 0, write, 0);
        graph.connect(read, 0, output, 0);

        let mut engine = Engine::new();
        engine.prepare(128, &[2]);
        load(&mut engine, &graph);

        let daw_in = impulse(128, 8);
        let mut daw_out = vec![0.0f32; 2 * 128];
        engine.run_audio(&audio_ctx(128), &daw_in, &mut daw_out, &mut Adders);

        // An edit somewhere else entirely, between the write and the read.
        let constant = graph.add(NodeKind::Constant(Constant { value: 0.5 }), [0.0, 0.0]);
        let slot = param_sink(&mut graph);
        graph.connect(constant, 0, slot, 0);
        load(&mut engine, &graph);

        let mut daw_out = vec![0.0f32; 2 * 128];
        engine.run_audio(
            &audio_ctx(128),
            &vec![0.0; 2 * 128],
            &mut daw_out,
            &mut Adders,
        );
        let peak = daw_out[..128]
            .iter()
            .fold(0.0f32, |best, v| best.max(v.abs()));
        assert!(peak > 0.9, "the impulse survived the swap, peak {peak}");
        assert_eq!(
            daw_out[..128]
                .iter()
                .position(|v| v.abs() > 0.9)
                .expect("and at the right moment"),
            8 + 200 - 128
        );
    }
}
