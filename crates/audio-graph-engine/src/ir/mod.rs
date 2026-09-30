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
mod granular;
mod keys;
pub use granular::{
    GranularParam, GranularSpec, MAX_GRAINS, MAX_GRANULAR_SECONDS, MAX_GRANULAR_SLICES,
    MAX_GRANULARS, MIN_GRANULAR_BLOCK_SECONDS,
};
pub use keys::KeyTrigger;
mod note_op;
mod op;
mod tremolo;

pub(crate) use note_op::{NoteStream, NoteStreamKind};

pub use audio_op::{
    AudioMathOp, AudioOp, Buf, Chunking, DC_CUTOFF_HZ, DEFAULT_QUANTUM, MixIn, QUANTUM_CHOICES,
    Span, Stage,
};
pub use note_op::{
    ALL_CHANNELS, ALL_CONTROLLERS, CC_INTERVAL, MAX_MERGE_INPUTS, MAX_NOTE_BUFS, MAX_NOTE_DELAYS,
    MAX_NOTE_EMITS, NOTE_BUF_CAPACITY, NOTE_DELAY_CAPACITY, NoteBuf, NoteOp,
};

pub use tremolo::{MAX_TREMOLO_ROWS, MAX_TREMOLOS, TREMOLO_NOTES, TremoloSpec};

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

/// How many audio nodes one program may have that keep state between blocks
/// of their own (a filter's history) — see [`AudioOp::Math`]. A ceiling for
/// the same reason as the latches.
pub const MAX_DSP_STATES: usize = 64;

/// Values in one node's DSP state: two per channel for a first-order filter,
/// with room for twice that.
pub const DSP_VALUES: usize = 4 * MAX_CHANNELS;
pub const MAX_DELAY_LINES: usize = 16;

/// How many *audio* delay lines one program may have.
///
/// Counted apart from [`MAX_DELAY_LINES`] because an audio line's ring is a
/// channel of `f32` per channel of its signal, and the audio half keeps its
/// own numbering for them.
pub const MAX_AUDIO_DELAY_LINES: usize = 8;
/// How far back any delay line may be *asked* to read, in seconds.
///
/// Not what it costs: each ring is allocated from its node's `max_time`, so a
/// 250 ms delay costs 250 ms. This is the ceiling because something has to bound
/// `max_time`, and a delay longer than it is a looper rather than a delay.
pub const MAX_DELAY_SECONDS: f64 = 10.0;

/// The rings of one kind of delay line, as a program carries them to the audio
/// thread.
///
/// A ring holds a sample per sample of delay — per channel for audio, one
/// value for a parameter — so how far back a line reaches is its node's
/// `max_time` and nothing else. It is sized here, on the main thread, from
/// that and the sample rate, because the audio thread may not allocate and
/// only this side knows both.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Rings<T> {
    /// Line index → the `DelayWrite` node it belongs to, which is what the
    /// ring's contents follow across a program swap: a feedback loop that
    /// emptied itself every time the user nudged an unrelated control would
    /// not be usable.
    pub(crate) nodes: Vec<NodeId>,
    /// Line index → the furthest any read of it asked to reach, in seconds.
    pub(crate) seconds: Vec<f64>,
    /// Line index → how many samples per channel its ring holds, or zero for
    /// none. Filled by [`Rings::size`].
    pub(crate) len: Vec<usize>,
    /// Rings for the lines whose length has changed, allocated on the main
    /// thread and handed over with the program.
    ///
    /// Empty — the usual case — means "keep the one you have". A recompile
    /// happens on every drag of every control, and reallocating 700 kB each
    /// time to hand back something the same size would be silly.
    pub(crate) rings: Vec<Vec<T>>,
}

impl<T: Copy + Default> Rings<T> {
    /// Gives each line a ring of `len_of(seconds)` samples per channel, where
    /// `previous` — what the last call returned — does not already say it has
    /// one that long.
    ///
    /// Returns what it decided, for the next call to compare against.
    fn size(
        &mut self,
        channels: usize,
        len_of: impl Fn(f64) -> usize,
        previous: &[(NodeId, usize)],
    ) -> Vec<(NodeId, usize)> {
        self.len = self
            .seconds
            .iter()
            .map(|&seconds| len_of(seconds))
            .collect();
        let want: Vec<(NodeId, usize)> = self
            .nodes
            .iter()
            .copied()
            .zip(self.len.iter().copied())
            .collect();
        self.rings = want
            .iter()
            .map(|entry| {
                if entry.1 == 0 || previous.contains(entry) {
                    Vec::new()
                } else {
                    vec![T::default(); channels * entry.1]
                }
            })
            .collect();
        want
    }

    /// Takes over the rings a superseded, never-adopted program was carrying
    /// for the same nodes at the same lengths, so replacing it does not throw
    /// them away.
    fn carry(&mut self, pending: &mut Rings<T>) {
        for line in 0..self.nodes.len() {
            let (node, len) = (self.nodes[line], self.len[line]);
            let Some(old_line) = pending
                .nodes
                .iter()
                .zip(&pending.len)
                .position(|(&old_node, &old_len)| old_node == node && old_len == len)
            else {
                continue;
            };
            if self.rings[line].is_empty() && !pending.rings[old_line].is_empty() {
                std::mem::swap(&mut self.rings[line], &mut pending.rings[old_line]);
            }
        }
    }
}

/// What [`Program::size_rings`] decided, per kind of line.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct RingSizes {
    granular: Vec<(NodeId, usize)>,
    audio: Vec<(NodeId, usize)>,
    params: Vec<(NodeId, usize)>,
}

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
    /// The audio delay lines, numbered among themselves.
    pub(crate) audio_lines: Rings<f32>,
    pub(crate) granular_lines: Rings<f32>,
    /// The parameter delay lines, numbered as the compiler numbered every
    /// line; an audio line's entry asks for no ring.
    pub(crate) param_lines: Rings<f64>,
    /// Audio processing operations in topological execution order.
    pub(crate) audio_ops: Vec<AudioOp>,
    /// The note half, run once per row ahead of the audio ops.
    pub(crate) note_ops: Vec<NoteOp>,
    /// How many note buffers this program uses.
    pub(crate) note_bufs: u16,
    pub(crate) note_streams: Vec<NoteStream>,
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
    /// DSP state index → the audio node it belongs to.
    ///
    /// Carried across a swap like the LFOs: a filter whose history emptied on
    /// every recompile would click on every drag of every control.
    pub(crate) dsp_nodes: Vec<NodeId>,
    /// Note delay index → the node whose queue it is, carried across a swap
    /// so the notes in flight are not lost to a recompile.
    pub(crate) note_delay_nodes: Vec<NodeId>,
    /// Tremolo index → the node whose clock and notes it is, the MIDI
    /// tremolos first and then the audio ones. Carried across a swap so a
    /// recompile neither moves the grid nor drops a note being cut.
    pub(crate) tremolo_nodes: Vec<NodeId>,
    /// Param output socket → the register holding its value, in the order the
    /// outputs were bound.
    ///
    /// The audio thread never reads it. It is kept for whoever has to say what
    /// a socket is carrying: every register is written once per row and
    /// never reused, so after a block has run, the register named here still
    /// holds that socket's value from the last row.
    pub(crate) output_registers: Vec<((NodeId, u8), Reg)>,
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
    pub(crate) fallback_destinations: Vec<crate::notes::FallbackDestination>,
    pub(crate) program: Program,
    pub(crate) publication: u64,
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
        previous: &RingSizes,
    ) -> (Self, RingSizes) {
        let sizes = program.size_rings(sample_rate, previous);
        (
            Self {
                fallback_destinations: program
                    .instances
                    .iter()
                    .map(|io| crate::notes::FallbackDestination::new(io.instance))
                    .collect(),
                program,
                publication: 0,
            },
            sizes,
        )
    }

    pub(crate) fn program_mut(&mut self) -> &mut Program {
        &mut self.program
    }

    pub(crate) fn carry_pending_rings(&mut self, pending: &mut Self) {
        self.program
            .granular_lines
            .carry(&mut pending.program.granular_lines);
        self.program
            .audio_lines
            .carry(&mut pending.program.audio_lines);
        self.program
            .param_lines
            .carry(&mut pending.program.param_lines);
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
    history: std::sync::Mutex<PublicationHistory>,
    handoff: crate::Handoff<PreparedProgram>,
}

#[derive(Default)]
struct PublicationHistory {
    rings: RingSizes,
    publication: u64,
}

impl Default for ProgramPublisher {
    fn default() -> Self {
        Self {
            history: std::sync::Mutex::new(PublicationHistory::default()),
            handoff: crate::Handoff::new(),
        }
    }
}

impl ProgramPublisher {
    /// Forces the next publication to supply every delay ring for a new activation.
    pub fn reset(&self) {
        self.history.lock().unwrap().rings = RingSizes::default();
    }

    pub(crate) fn handoff(&self) -> &crate::Handoff<PreparedProgram> {
        &self.handoff
    }

    pub fn reclaim(&self) {
        self.handoff.reclaim();
    }

    /// Returns a monotonically increasing identifier within this publisher.
    /// A consumer can distinguish publication from actual adoption through
    /// [`Engine::publication`][crate::Engine::publication].
    pub fn publish(&self, program: Program, sample_rate: f64) -> u64 {
        let mut history = self.history.lock().unwrap();
        let (mut prepared, sizes) = PreparedProgram::prepare(program, sample_rate, &history.rings);
        history.rings = sizes;
        history.publication = history
            .publication
            .checked_add(1)
            .expect("publication identifiers exhausted");
        prepared.publication = history.publication;
        self.handoff.send_with(Box::new(prepared), |next, pending| {
            next.carry_pending_rings(pending);
        });
        history.publication
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
            note_streams: Vec::new(),
            param_targets: Vec::new(),
            audio_lane_base: 0,
            instances: Vec::new(),
            buffers: Vec::new(),
            stages: Vec::new(),
            latency: 0,
            audio_lines: Rings::default(),
            granular_lines: Rings::default(),
            param_lines: Rings::default(),
            lfo_nodes: Vec::new(),
            latch_nodes: Vec::new(),
            dsp_nodes: Vec::new(),
            note_delay_nodes: Vec::new(),
            tremolo_nodes: Vec::new(),
            output_registers: Vec::new(),
        }
    }

    /// Gives each delay line a ring as long as its node asked for.
    ///
    /// Main thread only — it allocates, and that is the point: the audio thread
    /// must never do it, and only this side knows both the graph's `max_time`
    /// and the DAW's sample rate. The rings ride over inside the program, so
    /// they arrive at exactly the moment the line numbering they belong to
    /// does.
    ///
    /// `previous` is what the last call returned. A line already holding a ring
    /// of the right length gets an empty entry, which the engine reads as "keep
    /// the one you have".
    pub(crate) fn size_rings(&mut self, sample_rate: f64, previous: &RingSizes) -> RingSizes {
        let ceiling = (MAX_DELAY_SECONDS * sample_rate.max(1.0)) as usize;
        let samples = |seconds: f64| (seconds.max(0.0) * sample_rate).ceil() as usize;
        RingSizes {
            granular: self.granular_lines.size(
                MAX_CHANNELS,
                |seconds| (seconds * sample_rate.max(1.0)).ceil().max(4.0) as usize,
                &previous.granular,
            ),
            // Four samples over what was asked for: the read pointer is
            // fractional and the interpolator looks two samples past it.
            audio: self.audio_lines.size(
                MAX_CHANNELS,
                |seconds| (samples(seconds) + 4).clamp(64, ceiling),
                &previous.audio,
            ),
            // One over, because a read reaches back from the sample the row's
            // write is about to fill. A line no read asked anything of — an
            // audio line's entry among them — gets no ring at all.
            params: self.param_lines.size(
                1,
                |seconds| {
                    if seconds > 0.0 {
                        (samples(seconds) + 1).min(ceiling)
                    } else {
                        0
                    }
                },
                &previous.params,
            ),
        }
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

    /// Every param output socket this program computes, with the register
    /// holding its value. See [`Engine::registers`][crate::Engine::registers]
    /// for where to read it.
    pub fn output_registers(&self) -> &[((NodeId, u8), Reg)] {
        &self.output_registers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pending replacements preserve rings by node identity, even when compile order changes.
    #[test]
    fn pending_ring_transfer_follows_nodes_not_line_numbers() {
        let mut old = Program::empty();
        old.audio_lines.nodes = vec![11, 22];
        old.audio_lines.len = vec![4, 8];
        old.audio_lines.rings = vec![vec![1.0; 8], Vec::new()];
        let mut next = Program::empty();
        next.audio_lines.nodes = vec![22, 11];
        next.audio_lines.len = vec![8, 4];
        next.audio_lines.rings = vec![Vec::new(), Vec::new()];

        let mut next = PreparedProgram {
            fallback_destinations: Vec::new(),
            program: next,
            publication: 0,
        };
        let mut old = PreparedProgram {
            fallback_destinations: Vec::new(),
            program: old,
            publication: 0,
        };
        next.carry_pending_rings(&mut old);

        assert_eq!(next.program.audio_lines.rings[1], vec![1.0; 8]);
        assert!(next.program.audio_lines.rings[0].is_empty());
    }
}
