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

use crate::graph::{ExprSource, MathOp, NodeId, Waveform};

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

/// An index into the register file.
pub type Reg = u16;

/// A math operand: either another node's output or the node's own constant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Operand {
    Reg(Reg),
    Value(f64),
}

/// How fast an LFO runs, resolved into what the evaluator needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RateSpec {
    Hz(f64),
    /// Cycles per beat is what the evaluator wants; the editor thinks in beats
    /// per cycle, so the reciprocal is taken once, here, rather than every
    /// sub-block.
    CyclesPerBeat(f64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    Const {
        out: Reg,
        value: f64,
    },
    /// Read the DAW's automation for a slot.
    Slot {
        out: Reg,
        slot: u16,
    },
    Lfo {
        out: Reg,
        /// Index into the engine's phase array.
        state: u16,
        waveform: Waveform,
        rate: RateSpec,
        offset_phase: f64,
        depth: f64,
        centre: f64,
    },
    Expr {
        out: Reg,
        source: ExprSource,
    },
    Math {
        out: Reg,
        a: Reg,
        b: Operand,
        op: MathOp,
    },
    Range {
        out: Reg,
        a: Reg,
        in_lo: f64,
        in_span: f64,
        out_lo: f64,
        out_span: f64,
        clamp: bool,
    },
    /// Read from a delay line, `time` seconds back (§14.4).
    ///
    /// Clamped at run time to at least one sub-block, which is the floor of
    /// §14.4 expressed in the param domain. The compiler cannot do it: the
    /// floor depends on the sample rate and the quantum, and it knows neither.
    DelayRead {
        out: Reg,
        line: u16,
        time: f64,
    },
    /// Write this sub-block's value into a delay line.
    ///
    /// Emitted after everything feeding it, like any other op — but nothing
    /// reads its result, so no register is involved and no edge exists for the
    /// topological sort to follow back.
    DelayWrite {
        line: u16,
        a: Reg,
    },
}

/// An index into the audio buffer pool.
pub type Buf = u16;

/// How often the audio half of a program is evaluated (§14.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Chunking {
    /// Once per block the DAW hands us. Parameter changes still arrive at
    /// sub-block resolution, as events with an offset — there is no reason to
    /// call a plugin more often than the DAW does.
    #[default]
    WholeBlock,
    /// Once per sub-block. Required as soon as an audio feedback loop exists:
    /// §14.4's `D >= chunk length` binds the plugins in the loop too.
    SubBlock,
}

/// Where a plugin node's notes come from (§14.10).
///
/// An identity rather than a buffer: this crate does not know what a note is
/// (§7), so it routes the *name* of a source and lets the adapter turn that
/// into events. That is also what keeps a `Program` free of pointers (ADR-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NoteSource {
    /// Nothing wired to the notes port.
    ///
    /// The plugin gets no notes at all — not the DAW's, not anyone's. Before
    /// M8.3 every instance was handed every event the DAW sent, which is why
    /// two synths in one patch played in unison whatever the graph said.
    #[default]
    None,
    /// One of the wrapper's own note inputs from the DAW.
    Daw { bus: u16 },
}

/// One step of the audio half of a program.
///
/// Kept apart from [`Op`] because the two halves run at different rates
/// (§14.9). Buffers are indices into a pool the engine owns; nothing here is a
/// pointer, so a `Program` still crosses a process boundary unchanged (ADR-6).
#[derive(Debug, Clone, PartialEq)]
pub enum AudioOp {
    /// Copy one of the wrapper's own inputs in from the DAW.
    Input { out: Buf, bus: u16 },
    /// Copy a buffer out to one of the wrapper's own outputs.
    Output { a: Buf, bus: u16 },
    /// Run a sub-plugin from one buffer into another.
    ///
    /// `input` and `output` are always different buffers: a plugin that reads
    /// and writes the same memory is a question about the plugin's internals
    /// that the host has no business asking.
    Plugin {
        instance: u32,
        /// The whole input region: main bus first, then each aux bus, packed
        /// (§14.11). Assembled by a preceding [`AudioOp::Gather`] whenever it
        /// is more than one bus wide.
        input: Buf,
        /// Channel width of each input bus, main first. Empty for an
        /// instrument. The engine needs it to tell the adapter where the joins
        /// in `input` are.
        input_buses: Vec<u16>,
        output: Buf,
        /// Which note stream this instance hears (§14.10).
        notes: NoteSource,
    },
    /// Assemble a plugin's input region out of one buffer per bus (§14.11).
    ///
    /// Each entry names a source buffer and the width the plugin negotiated for
    /// that bus. Where they differ the copy adapts: a stereo source into a mono
    /// sidechain is summed, a mono source into a stereo bus is duplicated. That
    /// conversion is an op rather than a rule inside `Plugin` so it is visible
    /// in the compiled program and can be asserted on.
    Gather { out: Buf, buses: Vec<(Buf, u16)> },
    /// Sum several buffers into one.
    Mix { out: Buf, inputs: Vec<Buf> },
    /// Delay a buffer by a fixed number of samples.
    ///
    /// Inserted by the compiler to line up parallel paths (§14.6), never placed
    /// by the user — the delay the user places is a `DelayWrite`/`DelayRead`
    /// pair. `slot` indexes the engine's compensation rings.
    Compensate { buf: Buf, slot: u16, samples: u32 },
    /// Fill a buffer with silence. Emitted for an input nobody connected.
    Silence { out: Buf },
}

/// A graph, compiled.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    /// Topologically ordered: every `Op` reads only registers already written.
    pub ops: Vec<Op>,
    pub registers: usize,
    /// Which slot each output drives, and where its value ends up. Sorted by
    /// slot, and at most one entry per slot.
    pub outputs: Vec<(u16, Reg)>,
    /// Line index → the `DelayWrite` node it belongs to.
    ///
    /// Carried across a swap for the same reason as `lfo_nodes`: §14.5. A
    /// feedback loop that emptied itself every time the user nudged an
    /// unrelated control would not be usable.
    pub delay_nodes: Vec<NodeId>,
    /// The audio half, in order (§14.9).
    pub audio_ops: Vec<AudioOp>,
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
            instances: Vec::new(),
            buffers: Vec::new(),
            chunking: Chunking::WholeBlock,
            latency: 0,
            delay_nodes: Vec::new(),
            lfo_nodes: Vec::new(),
        }
    }

    /// Whether running this program would do nothing observable.
    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty() && self.audio_ops.is_empty()
    }

    /// Whether the graph drives this slot, and so overrides the DAW's
    /// automation for it.
    pub fn drives(&self, slot: usize) -> bool {
        u16::try_from(slot).is_ok_and(|s| self.outputs.iter().any(|&(o, _)| o == s))
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
    pub output_channels: u16,
}
