//! Compiled intermediate representation: a flat sequence of instructions over register files and audio buffers.
//!
//! A [`Program`] is what crosses to the audio thread. It holds no `Rc`, no
//! `Box<dyn>`, no map lookups and no pointers back into the edit graph —
//! running it is a straight walk down a `Vec` writing `f64`s into a slice.
//! Everything that could have been a decision has already been made by the
//! compiler: the audio thread does not think, it executes.
//!
//! A `Program` is immutable once built. The per-instance state that changes as
//! it runs — LFO phases, delay ring contents, latches — lives in
//! [`Engine`][crate::Engine] instead, so swapping a program does not reset an
//! oscillator mid-note.
//!
//! **Nothing in this module may reach back into the edit side**: no `use` of
//! `graph`, `nodes` or `port` appears here or in its children. That is what
//! keeps a `Program` a value rather than a view onto a graph, and it is what
//! would let an out-of-process backend be a substitution rather than a
//! rewrite.

mod audio_op;
mod note_op;
mod op;

pub use audio_op::{AudioOp, Buf, Chunking, MixIn, Span, Stage};
pub use note_op::{
    ALL_CHANNELS, ALL_CONTROLLERS, MAX_NOTE_BUFS, MAX_NOTE_EMITS, NOTE_BUF_CAPACITY, NoteBuf,
    NoteOp,
};

pub use op::{Detect, Follow, MathOp, Op, Operand, RateSpec, Reg, Waveform};
use subhost_adapter::{InstanceIo, ParamTarget};

/// Unique identifier for a node, persistent across graph recompilations.
///
/// Defined here rather than with the graph because a `Program` carries a few of
/// them: an LFO's phase, a delay line's ring and a latch are matched to their
/// node across a swap, so that recompiling — which happens on every drag of
/// every control — does not restart an oscillator, empty a delay or forget which
/// way a switch was thrown.
pub type NodeId = u32;

/// How many sub-plugin parameters one graph may drive directly.
///
/// A ceiling for the same reason the register count is one: the schedule that
/// carries these to the audio thread is allocated at activate, and a graph that
/// wants more is refused with a message rather than served with an allocation
/// inside `process`.
pub const MAX_GRAPH_PARAMS: usize = 64;

/// Ceilings, so the audio thread can preallocate and never resize.
///
/// A graph that would exceed one is refused at compile time with an error the
/// user can read, which is a much better failure than an allocation inside
/// `process`.
pub const MAX_REGISTERS: usize = 256;
pub const MAX_LFOS: usize = 64;

/// How many latches one program may have — one per key-switch node. A ceiling
/// because the table is allocated once and never resized.
pub const MAX_LATCHES: usize = 64;
pub const MAX_DELAY_LINES: usize = 16;

/// How far back a param delay line can read, in sub-blocks.
///
/// A param line stores one value per sub-block, so this is a time only once the
/// sample rate and the quantum are known: 4096 sub-blocks is 2.7 s at 48 kHz
/// with the default quantum of 32, and 1.4 s at the finest quantum of 16. The
/// ring is preallocated for it, because the audio thread may not allocate and
/// the alternative — sizing from the longest delay in the graph — would mean a
/// reallocation every time the user drags the time control.
pub const MAX_DELAY_TAPS: usize = 4096;

/// How many *audio* delay lines one program may have.
///
/// Counted apart from [`MAX_DELAY_LINES`] because an audio line costs a ring of
/// samples rather than a ring of sub-block values.
pub const MAX_AUDIO_DELAY_LINES: usize = 8;
/// How far back an audio delay line may be *asked* to read, in seconds.
///
/// Not what it costs: each ring is allocated from its node's `max_time`, so a
/// 250 ms delay costs 250 ms. This is the ceiling because something has to bound
/// `max_time`, and a delay longer than it is a looper rather than a delay.
pub const MAX_AUDIO_DELAY_SECONDS: f64 = 10.0;

/// Lanes past the slot table that carry something the *audio* half reads: a
/// delay time or a gain.
///
/// Same mechanism as the parameter lanes and a disjoint range of lane numbers,
/// so the evaluator writes one exactly the way it writes a slot and the adapter,
/// which only knows about parameters, never sees one.
pub const MAX_AUDIO_LANES: usize = 16;

/// How many parallel paths one program may compensate, and by how much.
///
/// Both are preallocated, so both are ceilings rather than guidance. The length
/// is about 680 ms at 48 kHz, which covers the linear-phase and look-ahead
/// plugins that make compensation necessary in the first place; the count is the
/// number of *compensated* branches, not of buffers, and a merge of two paths
/// needs one.
pub const MAX_COMPENSATORS: usize = 8;
pub const MAX_COMPENSATION: usize = 32_768;

/// Ceiling on the audio buffer pool, so `activate` can size it once and never
/// grow.
pub const MAX_BUFFERS: usize = 64;

/// Widest single bus the engine moves around. Stereo throughout.
pub const MAX_CHANNELS: usize = 2;

/// Widest *buffer*, which is not the same thing.
///
/// A plugin's input region holds its main bus and then each aux bus packed into
/// one run, so it is as wide as all of them together. Every buffer in the pool
/// is this wide because the pool is uniform; at 8 channels, 64 buffers and a
/// 512-frame block that is a megabyte, which is worth it for not having two
/// kinds of buffer to keep straight.
pub const MAX_BUFFER_CHANNELS: usize = MAX_CHANNELS * (1 + MAX_AUX_BUSES);

pub use plugin_host::MAX_AUX_BUSES;

/// A compiled execution program representing an audio and control graph.
///
/// The instruction storage is owned by the compiler. External callers receive
/// metadata through read-only accessors and must publish through
/// [`ProgramPublisher`].
///
/// ```compile_fail
/// let mut program = audio_graph_engine::Program::empty();
/// program.ops.clear();
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    /// Topologically ordered scalar operations. Every `Op` reads only registers already written.
    pub(crate) ops: Vec<Op>,
    pub(crate) registers: usize,
    /// Which lane each output drives, and where its value ends up. Sorted by
    /// lane, and at most one entry per lane.
    ///
    /// Lanes below `slot_count` are the DAW's own automation and the graph never
    /// writes them; what lands here is a parameter lane or an audio lane.
    pub(crate) outputs: Vec<(u16, Reg)>,
    /// Audio line index → how many samples per channel its ring holds.
    ///
    /// From the node's `max_time` and the sample rate, so a line costs what it
    /// was asked for. The compiler cannot fill it in — it does not know the
    /// sample rate — so the main thread does, in `size_rings`.
    pub(crate) audio_ring_len: Vec<usize>,
    /// Rings for the lines whose length has changed, allocated on the main
    /// thread and handed over with the program.
    ///
    /// Empty — the usual case — means "keep the ones you have". A recompile
    /// happens on every drag of every control, and reallocating 700 kB each time
    /// to hand back something the same size would be silly.
    pub(crate) audio_rings: Vec<Vec<f32>>,
    /// Maximum delay duration in seconds per audio delay line.
    pub(crate) audio_ring_seconds: Vec<f64>,
    /// Audio line index → the `DelayWrite` node it belongs to.
    ///
    /// Separate from `delay_nodes`: audio lines are numbered among themselves,
    /// because their rings are a scarcer resource than a param line's. Carried
    /// across a swap so the ring contents survive.
    pub(crate) audio_delay_nodes: Vec<NodeId>,
    /// Line index → the `DelayWrite` node it belongs to.
    ///
    /// Carried across a swap for the same reason as `lfo_nodes`: a feedback loop
    /// that emptied itself every time the user nudged an unrelated control would
    /// not be usable.
    pub(crate) delay_nodes: Vec<NodeId>,
    /// Audio processing operations in topological execution order.
    pub(crate) audio_ops: Vec<AudioOp>,
    /// The note half, run once per sub-block ahead of the audio ops.
    pub(crate) note_ops: Vec<NoteOp>,
    /// How many note buffers this program uses.
    pub(crate) note_bufs: u16,
    /// Which sub-plugin parameter each graph-driven lane drives.
    ///
    /// Entry `k` is the lane `slot_count + k` in [`Program::outputs`], so the
    /// evaluator writes it exactly the way it writes a slot and needs to know
    /// nothing about parameters. Sorted by instance, then by parameter.
    pub(crate) param_targets: Vec<ParamTarget>,
    /// The first lane number that carries something the *audio* half reads:
    /// `slot_count + MAX_GRAPH_PARAMS`.
    ///
    /// The evaluator needs it to know which of its outputs are 0..1 parameters
    /// and which are not. A gain is decibels and a delay time is seconds;
    /// clamping either of those to 0..1 turns a -100 dB mute into unity gain.
    pub(crate) audio_lane_base: u16,
    /// How each plugin instance has to be activated.
    ///
    /// Derived from the graph, not from the plugin: whether a sidechain bus is
    /// switched on depends on whether anything is wired to it. Sorted by
    /// instance.
    pub(crate) instances: Vec<InstanceIo>,
    /// Channel width of each buffer in the audio pool.
    pub(crate) buffers: Vec<u16>,
    /// How the three op lists are cut into runs that execute together, in
    /// the order they run. See [`Stage`].
    pub(crate) stages: Vec<Stage>,
    /// What the wrapper should report to the DAW as its own latency: the longest
    /// path from an input to an output, after compensation.
    pub(crate) latency: u32,
    /// Latch index → the node it belongs to.
    ///
    /// Carried across a swap: a key switch that forgot which way it was thrown
    /// every time the user nudged an unrelated control would be unusable.
    pub(crate) latch_nodes: Vec<NodeId>,
    /// State index → the LFO node it belongs to.
    ///
    /// Carried across a swap so that recompiling — which happens on every drag
    /// of every knob — does not restart the oscillators. Without it, editing an
    /// unrelated node would put a click in the middle of a slow LFO sweep.
    pub(crate) lfo_nodes: Vec<NodeId>,
}

/// A compiled program prepared for its publisher's receiving engine and sample
/// rate. Unchanged delay rings remain in the engine instead of being duplicated.
///
/// Construction belongs to [`ProgramPublisher`], which keeps the preparation
/// history and pending resources together.
///
/// ```compile_fail
/// use audio_graph_engine::{PreparedProgram, Program};
/// let prepared = PreparedProgram { program: Program::empty() };
/// ```
#[derive(Debug, PartialEq)]
pub struct PreparedProgram {
    pub(crate) program: Program,
}

impl std::ops::Deref for PreparedProgram {
    type Target = Program;

    fn deref(&self) -> &Self::Target {
        &self.program
    }
}

impl PreparedProgram {
    pub(crate) fn prepare(
        mut program: Program,
        sample_rate: f64,
        previous: &[(NodeId, usize)],
    ) -> (Self, Vec<(NodeId, usize)>) {
        let sizes = program.size_rings(sample_rate, previous);
        (Self { program }, sizes)
    }

    pub(crate) fn program_mut(&mut self) -> &mut Program {
        &mut self.program
    }

    pub(crate) fn carry_pending_rings(&mut self, pending: &mut Self) {
        for line in 0..self.program.audio_delay_nodes.len() {
            let node = self.program.audio_delay_nodes[line];
            let len = self.program.audio_ring_len[line];
            let Some(old_line) = pending
                .program
                .audio_delay_nodes
                .iter()
                .zip(&pending.program.audio_ring_len)
                .position(|(&old_node, &old_len)| old_node == node && old_len == len)
            else {
                continue;
            };
            if self.program.audio_rings[line].is_empty()
                && !pending.program.audio_rings[old_line].is_empty()
            {
                std::mem::swap(
                    &mut self.program.audio_rings[line],
                    &mut pending.program.audio_rings[old_line],
                );
            }
        }
    }
}

/// Main-thread owner of ring sizing history and the prepared handoff.
///
/// One publisher serves one receiving [`Engine`][crate::Engine]. Publishing and
/// reclamation run off the audio thread; adoption takes no lock. Call
/// [`reset`][Self::reset] before publishing for a new activation so its rings
/// do not depend on a program from the previous activation.
///
/// The engine accepts this preparation boundary instead of an unprepared queue.
///
/// ```compile_fail
/// use audio_graph_engine::{Engine, Handoff, Program};
/// let handoff = Handoff::new();
/// handoff.send(Box::new(Program::empty()));
/// Engine::new().adopt(&handoff);
/// ```
pub struct ProgramPublisher {
    previous: std::sync::Mutex<Vec<(NodeId, usize)>>,
    handoff: crate::Handoff<PreparedProgram>,
}

impl Default for ProgramPublisher {
    fn default() -> Self {
        Self {
            previous: std::sync::Mutex::new(Vec::new()),
            handoff: crate::Handoff::new(),
        }
    }
}

impl ProgramPublisher {
    /// Forces the next publication to supply every delay ring for a new activation.
    pub fn reset(&self) {
        self.previous.lock().unwrap().clear();
    }

    pub(crate) fn handoff(&self) -> &crate::Handoff<PreparedProgram> {
        &self.handoff
    }

    pub fn reclaim(&self) {
        self.handoff.reclaim();
    }

    pub fn publish(&self, program: Program, sample_rate: f64) {
        let mut previous = self.previous.lock().unwrap();
        let (prepared, sizes) = PreparedProgram::prepare(program, sample_rate, &previous);
        *previous = sizes;
        self.handoff.send_with(Box::new(prepared), |next, pending| {
            next.carry_pending_rings(pending);
        });
    }
}

impl Program {
    /// The program that does nothing: no graph, or a graph with no outputs.
    pub fn empty() -> Program {
        Program {
            ops: Vec::new(),
            registers: 0,
            outputs: Vec::new(),
            audio_ops: Vec::new(),
            note_ops: Vec::new(),
            note_bufs: 0,
            param_targets: Vec::new(),
            audio_lane_base: 0,
            instances: Vec::new(),
            buffers: Vec::new(),
            stages: Vec::new(),
            latency: 0,
            delay_nodes: Vec::new(),
            audio_delay_nodes: Vec::new(),
            audio_ring_len: Vec::new(),
            audio_rings: Vec::new(),
            audio_ring_seconds: Vec::new(),
            lfo_nodes: Vec::new(),
            latch_nodes: Vec::new(),
        }
    }

    /// Gives each audio delay line a ring as long as its node asked for.
    ///
    /// Main thread only — it allocates, and that is the point: the audio thread
    /// must never do it, and only this side knows both the graph's `max_time`
    /// and the DAW's sample rate. The rings ride over inside the program, so
    /// they arrive at exactly the moment the line numbering they belong to
    /// does.
    ///
    /// `previous` is what the last call returned. A line already holding a ring
    /// of the right length gets an empty entry, which the engine reads as "keep
    /// the one you have" — otherwise every drag of every control would hand over
    /// a fresh 700 kB to replace something identical.
    ///
    /// Returns what it decided, for the next call to compare against.
    pub(crate) fn size_rings(
        &mut self,
        sample_rate: f64,
        previous: &[(NodeId, usize)],
    ) -> Vec<(NodeId, usize)> {
        let ceiling = (MAX_AUDIO_DELAY_SECONDS * sample_rate.max(1.0)) as usize;
        self.audio_ring_len = self
            .audio_ring_seconds
            .iter()
            // Four samples over what was asked for: the read pointer is
            // fractional and the interpolator looks two samples past it.
            .map(|&s| ((s.max(0.0) * sample_rate).ceil() as usize + 4).clamp(64, ceiling))
            .collect();
        let want: Vec<(NodeId, usize)> = self
            .audio_delay_nodes
            .iter()
            .copied()
            .zip(self.audio_ring_len.iter().copied())
            .collect();
        self.audio_rings = want
            .iter()
            .map(|entry| {
                if previous.contains(entry) {
                    Vec::new()
                } else {
                    vec![0.0; MAX_CHANNELS * entry.1]
                }
            })
            .collect();
        want
    }

    /// Whether the graph drives `lane` — a parameter lane or an audio lane,
    /// since the slot lanes below them are the DAW's.
    pub fn drives_lane(&self, lane: usize) -> bool {
        u16::try_from(lane).is_ok_and(|l| self.outputs.iter().any(|&(o, _)| o == l))
    }

    /// Returns true if running this program produces no observable outputs or audio operations.
    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty() && self.audio_ops.is_empty()
    }

    pub fn instances(&self) -> &[InstanceIo] {
        &self.instances
    }

    pub fn param_targets(&self) -> &[ParamTarget] {
        &self.param_targets
    }

    pub fn latency(&self) -> u32 {
        self.latency
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pending replacements preserve rings by node identity, even when compile order changes.
    #[test]
    fn pending_ring_transfer_follows_nodes_not_line_numbers() {
        let mut old = Program::empty();
        old.audio_delay_nodes = vec![11, 22];
        old.audio_ring_len = vec![4, 8];
        old.audio_rings = vec![vec![1.0; 8], Vec::new()];
        let mut next = Program::empty();
        next.audio_delay_nodes = vec![22, 11];
        next.audio_ring_len = vec![8, 4];
        next.audio_rings = vec![Vec::new(), Vec::new()];

        let mut next = PreparedProgram { program: next };
        let mut old = PreparedProgram { program: old };
        next.carry_pending_rings(&mut old);

        assert_eq!(next.program.audio_rings[1], vec![1.0; 8]);
        assert!(next.program.audio_rings[0].is_empty());
    }
}
