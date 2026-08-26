//! The compiled form: a flat list of instructions over a register file.
//!
//! This is what crosses to the audio thread. It holds no `Rc`, no `Box<dyn>`,
//! no map lookups and no pointers back into the edit graph — running it is a
//! straight walk down a `Vec` writing `f64`s into a slice. Everything that
//! could have been a decision has already been made by the compiler, which is
//! the whole point of §9.1: the audio thread does not think, it executes.
//!
//! A `Program` is immutable once built. The per-instance state that changes as
//! it runs — LFO phases — lives in [`Engine`][crate::Engine] instead, so
//! swapping a program does not reset an oscillator mid-note.
//!
//! Nothing in this module may reach back into the edit side: no `use` of
//! `graph`, `nodes` or `port` appears here or in its children. That is what
//! keeps a `Program` a value rather than a view onto a graph, and it is what
//! ADR-6 relies on to make an out-of-process backend a substitution.

mod audio_op;
mod op;

pub use audio_op::{AudioOp, Buf, Chunking, MixIn, NoteSource};
pub use op::{ExprSource, MathOp, Op, Operand, RateSpec, Reg, Waveform};

/// Identifies one node, for the whole life of a patch.
///
/// Defined here rather than with the graph because a `Program` carries a few
/// of them: an LFO's phase and a delay line's ring are matched to their node
/// across a swap, so that recompiling — which happens on every drag of every
/// control — does not restart an oscillator or empty a delay (§14.5).
pub type NodeId = u32;

/// How many sub-plugin parameters one graph may drive directly (§14.12).
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
pub const MAX_DELAY_LINES: usize = 16;

/// How far back a param delay line can read, in sub-blocks.
///
/// A param line stores one value per sub-block (§9.2), so this is a time only
/// once the sample rate and the quantum are known: 4096 sub-blocks is 2.7 s at
/// 48 kHz with the default quantum of 32, and 1.4 s at the finest quantum of
/// 16. The ring is preallocated for it, because §9.1 forbids allocating in
/// `process` and the alternative — sizing from the longest delay in the graph —
/// would mean a reallocation every time the user drags the time control.
pub const MAX_DELAY_TAPS: usize = 4096;

/// How many *audio* delay lines one program may have, and how far back one can
/// read.
///
/// Counted apart from [`MAX_DELAY_LINES`] because an audio line costs a ring of
/// samples rather than a ring of sub-block values. The length is what a line
/// may be *asked* for, not what it costs: each ring is allocated from its
/// node's `max_time` (§14.5), so a 250 ms delay costs 250 ms. 10 s is the
/// ceiling because something has to bound `max_time`, and a delay longer than
/// that is a looper rather than a delay.
pub const MAX_AUDIO_DELAY_LINES: usize = 8;
pub const MAX_AUDIO_DELAY_SECONDS: f64 = 10.0;

/// Lanes past the slot table that carry something the *audio* half reads: a
/// delay time (§14.5) or a gain.
///
/// Same mechanism as §14.12 and a disjoint range of lane numbers, so the
/// evaluator writes one exactly the way it writes a slot and the adapter,
/// which only knows about parameters, never sees one.
pub const MAX_AUDIO_LANES: usize = 16;

/// How many parallel paths one program may compensate, and by how much.
///
/// Both are preallocated (§9.1), so both are ceilings rather than guidance. A
/// graph that wants more is refused with a message rather than served with an
/// allocation inside `process`. The length is about 680 ms at 48 kHz, which
/// covers the linear-phase and look-ahead plugins that make compensation
/// necessary in the first place; the count is the number of *compensated*
/// branches, not of buffers, and a merge of two paths needs one.
pub const MAX_COMPENSATORS: usize = 8;
pub const MAX_COMPENSATION: usize = 32_768;

/// Ceiling on the audio buffer pool, so `activate` can size it once and never
/// grow.
pub const MAX_BUFFERS: usize = 64;

/// Widest single bus the engine moves around. Stereo throughout (§14.8).
pub const MAX_CHANNELS: usize = 2;

/// Widest *buffer*, which is not the same thing (§14.11).
///
/// A plugin's input region holds its main bus and then each aux bus packed into
/// one run, so it is as wide as all of them together. Every buffer in the pool
/// is this wide because the pool is uniform; at 8 channels, 64 buffers and a
/// 512-frame block that is a megabyte, which is worth it for not having two
/// kinds of buffer to keep straight.
pub const MAX_BUFFER_CHANNELS: usize = MAX_CHANNELS * (1 + MAX_AUX_BUSES);

pub use plugin_host_api::MAX_AUX_BUSES;

/// A graph, compiled.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    /// Topologically ordered: every `Op` reads only registers already written.
    pub ops: Vec<Op>,
    pub registers: usize,
    /// Which lane each output drives, and where its value ends up. Sorted by
    /// lane, and at most one entry per lane.
    ///
    /// Lanes below `slot_count` are the DAW's own automation and the graph
    /// never writes them (§8); what lands here is a parameter lane (§14.12) or
    /// an audio lane (§14.5).
    pub outputs: Vec<(u16, Reg)>,
    /// Audio line index → how many samples per channel its ring holds.
    ///
    /// From the node's `max_time` and the sample rate, so a line costs what it
    /// was asked for (§14.5). The compiler cannot fill it in — it does not know
    /// the sample rate — so the main thread does, in `size_rings`.
    pub audio_ring_len: Vec<usize>,
    /// Rings for the lines whose length has changed, allocated on the main
    /// thread and handed over with the program (§9.1, §9.4).
    ///
    /// Empty — the usual case — means "keep the ones you have". A recompile
    /// happens on every drag of every control, and reallocating 700 kB each
    /// time to hand back something the same size would be silly.
    pub audio_rings: Vec<Vec<f32>>,
    /// Audio line index → the longest a read on it asks for, in seconds.
    pub audio_ring_seconds: Vec<f64>,
    /// Audio line index → the `DelayWrite` node it belongs to.
    ///
    /// Separate from `delay_nodes`: audio lines are numbered among themselves,
    /// because their rings are a scarcer resource than a param line's
    /// (`MAX_AUDIO_DELAY_LINES`). Carried across a swap for the same reason.
    pub audio_delay_nodes: Vec<NodeId>,
    /// Line index → the `DelayWrite` node it belongs to.
    ///
    /// Carried across a swap for the same reason as `lfo_nodes`: §14.5. A
    /// feedback loop that emptied itself every time the user nudged an
    /// unrelated control would not be usable.
    pub delay_nodes: Vec<NodeId>,
    /// The audio half, in order (§14.9).
    pub audio_ops: Vec<AudioOp>,
    /// Which sub-plugin parameter each graph-driven lane drives (§14.12).
    ///
    /// Entry `k` is the lane `slot_count + k` in [`Program::outputs`], so the
    /// evaluator writes it exactly the way it writes a slot and needs to know
    /// nothing about parameters. Sorted by instance, then by parameter.
    pub param_targets: Vec<ParamTarget>,
    /// The first lane number that carries something the *audio* half reads
    /// (§14.5): `slot_count + MAX_GRAPH_PARAMS`.
    ///
    /// The evaluator needs it to know which of its outputs are 0..1 parameters
    /// and which are not. A gain is decibels and a delay time is seconds;
    /// clamping either of those to 0..1 turns a -100 dB mute into unity gain,
    /// which is exactly what it used to do.
    pub audio_lane_base: u16,
    /// How each plugin instance has to be activated (§14.11).
    ///
    /// Derived from the graph, not from the plugin: whether a sidechain bus is
    /// switched on depends on whether anything is wired to it. Sorted by
    /// instance.
    pub instances: Vec<InstanceIo>,
    /// Channel width of each buffer in the pool, by index.
    pub buffers: Vec<u16>,
    /// How often `audio_ops` runs (§14.9).
    pub chunking: Chunking,
    /// What the wrapper should report to the DAW as its own latency: the
    /// longest path from an input to an output, after compensation (§14.6).
    pub latency: u32,
    /// State index → the LFO node it belongs to.
    ///
    /// Carried across a swap so that recompiling — which happens on every drag
    /// of every knob — does not restart the oscillators. Without it, editing
    /// an unrelated node would put a click in the middle of a slow LFO sweep.
    pub lfo_nodes: Vec<NodeId>,
}

impl Program {
    /// The program that does nothing: no graph, or a graph with no outputs.
    pub fn empty() -> Program {
        Program {
            ops: Vec::new(),
            registers: 0,
            outputs: Vec::new(),
            audio_ops: Vec::new(),
            param_targets: Vec::new(),
            audio_lane_base: 0,
            instances: Vec::new(),
            buffers: Vec::new(),
            chunking: Chunking::WholeBlock,
            latency: 0,
            delay_nodes: Vec::new(),
            audio_delay_nodes: Vec::new(),
            audio_ring_len: Vec::new(),
            audio_rings: Vec::new(),
            audio_ring_seconds: Vec::new(),
            lfo_nodes: Vec::new(),
        }
    }

    /// Give each audio delay line a ring as long as its node asked for
    /// (§14.5).
    ///
    /// Main thread only — it allocates, and that is the point: the audio
    /// thread must never do it (§9.1), and only this side knows both the
    /// graph's `max_time` and the DAW's sample rate. The rings ride over
    /// inside the program, so they arrive at exactly the moment the line
    /// numbering they belong to does.
    ///
    /// `previous` is what the last call returned. A line already holding a ring
    /// of the right length gets an empty entry, which the engine reads as "keep
    /// the one you have" — otherwise every drag of every control would hand
    /// over a fresh 700 kB to replace something identical.
    ///
    /// Returns what it decided, for the next call to compare against.
    pub fn size_rings(
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

    /// Whether the graph drives `lane` — a parameter lane (§14.12) or an
    /// audio lane (§14.5), since the slot lanes below them are the DAW's.
    pub fn drives_lane(&self, lane: usize) -> bool {
        u16::try_from(lane).is_ok_and(|l| self.outputs.iter().any(|&(o, _)| o == l))
    }

    /// Whether running this program would do nothing observable.
    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty() && self.audio_ops.is_empty()
    }
}

/// The activation shape of one plugin instance (§14.11).
///
/// A sub-plugin has to be activated with the buses the graph will actually
/// feed it, and that is a property of the patch rather than of the plugin. It
/// lives in the `Program` because the compiler is what knows it, and because
/// changing it means the sub-plugin has to be deactivated and activated again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceIo {
    pub instance: u32,
    /// Main input bus width. Zero for an instrument.
    pub input_channels: u16,
    /// Aux input buses, in order. Only the ones the graph wired.
    pub aux_inputs: Vec<u16>,
    /// Main output bus width.
    pub output_channels: u16,
    /// Aux output buses, in order. Only as far as the graph reads them, so a
    /// plugin's third output is absent when only the second is wired.
    pub aux_outputs: Vec<u16>,
}

/// One sub-plugin parameter the graph drives directly (§14.12).
///
/// The wrapper's slots are the DAW's automation lanes and there are 32 of them;
/// this is the other way in, and it is not limited that way because nothing
/// outside the patch has to name it. A `SlotIn` node wired to a parameter
/// socket is how a DAW lane reaches a parameter now — the slot table is no
/// longer the only route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParamTarget {
    pub instance: u32,
    pub param: u32,
}
