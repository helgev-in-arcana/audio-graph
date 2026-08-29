//! Audio instruction set and audio buffer operations.
//!
//! Kept apart from [`Op`][crate::Op] because the two halves run at different
//! rates. Buffers are indices into a pool the engine owns; nothing here is a
//! pointer, so a `Program` stays a value that could cross a process boundary
//! unchanged.

use subhost_adapter::{NoteSource, NoteStream};

/// An index into the audio buffer pool.
pub type Buf = u16;

/// One input of an [`AudioOp::Mix`]: where it comes from, and how loud.
///
/// `lane` names the schedule lane carrying the gain when the user has wired its
/// socket; without one, `gain` is the whole story. Same arrangement as
/// [`AudioOp::DelayRead`]'s time, and the same range of lane numbers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MixIn {
    pub buf: Buf,
    pub lane: Option<u16>,
    pub gain: f64,
}

/// Evaluation granularity for audio processing operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Chunking {
    /// Once per block the DAW hands us. Parameter changes still arrive at
    /// sub-block resolution, as events with an offset — there is no reason to
    /// call a plugin more often than the DAW does.
    #[default]
    WholeBlock,
    /// Once per sub-block. Required as soon as an audio feedback loop exists:
    /// the rule that a delay must be at least one chunk long binds the plugins
    /// in the loop too.
    SubBlock,
}

/// How a plugin's notes reach it: where they come from, what may stop them on
/// the way, and which keys are swallowed before they arrive.
///
/// The gate is a lane number rather than a decision because the decision is the
/// parameter half's and is remade every sub-block, while the audio half runs on
/// its own grain — the same arrangement a `Mix`'s gain and a delay's time use.
/// Below 0.5 the stream is shut.
///
/// `mute` is settled at compile time instead: which keys a key switch answers to
/// is an edit, not a signal, so there is nothing for a lane to carry.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct NoteRoute {
    pub source: NoteSource,
    pub gate: Option<u16>,
    pub mute: u128,
}

impl NoteRoute {
    pub fn from_source(source: NoteSource) -> NoteRoute {
        NoteRoute {
            source,
            gate: None,
            mute: 0,
        }
    }

    /// Resolves the effective [`NoteStream`] based on the dynamic gate lane value.
    ///
    /// When the gate lane value is below 0.5, only release/note-off events are
    /// passed. `None` for the lane value means the lane is not there at all,
    /// which is a program the engine should not have been handed; shutting the
    /// stream is the quiet failure rather than the loud one.
    pub fn resolve(self, lane_value: Option<f64>) -> NoteStream {
        let source = match self.gate {
            None => self.source,
            Some(_) if lane_value.is_some_and(|v| v >= 0.5) => self.source,
            Some(_) => self.source.releases_only(),
        };
        NoteStream {
            source,
            mute: self.mute,
        }
    }
}

/// One step of the audio half of a program.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioOp {
    /// Copy host audio input bus into a buffer.
    Input { out: Buf, bus: u16 },
    /// Copy buffer contents to a host audio output bus.
    Output { a: Buf, bus: u16 },
    /// Run a sub-plugin from one buffer into another.
    ///
    /// `input` and `output` are always different buffers: a plugin that reads
    /// and writes the same memory is a question about the plugin's internals
    /// that the host has no business asking.
    Plugin {
        instance: u32,
        /// The whole input region: main bus first, then each aux bus, packed.
        /// Assembled by a preceding [`AudioOp::Gather`] whenever it is more
        /// than one bus wide.
        input: Buf,
        /// Channel width of each input bus, main first. Empty for an
        /// instrument. The engine needs it to tell the adapter where the joins
        /// in `input` are.
        input_buses: Vec<u16>,
        /// The whole output region: main bus first, then each aux bus the
        /// graph reads, packed — the mirror of `input`. Taken apart by
        /// [`AudioOp::Split`] when there is more than one.
        output: Buf,
        /// Channel width of each output bus, main first. One entry in the
        /// common case; more only when a patch reads a plugin's extra
        /// outputs.
        output_buses: Vec<u16>,
        /// Note stream routing and gating configuration for this plugin instance.
        notes: NoteRoute,
    },
    /// Copy one bus out of a plugin's output region.
    ///
    /// The mirror of [`AudioOp::Gather`], and simpler: the widths are the
    /// plugin's own on both sides, so nothing is converted. Emitted once per
    /// output bus something reads, and not at all for the one-bus case — where
    /// the plugin writes straight into the buffer the next node reads.
    Split {
        from: Buf,
        out: Buf,
        /// Starting channel offset of the bus within `from`.
        channel: u16,
        width: u16,
    },
    /// Assemble a plugin's input region out of one buffer per bus.
    ///
    /// Each entry names a source buffer and the width the plugin negotiated for
    /// that bus. Where they differ the copy adapts: a stereo source into a mono
    /// sidechain is summed, a mono source into a stereo bus is duplicated. That
    /// conversion is an op rather than a rule inside `Plugin` so it is visible
    /// in the compiled program and can be asserted on.
    Gather { out: Buf, buses: Vec<(Buf, u16)> },
    /// Sum several buffers into one, each scaled first.
    ///
    /// `out` may be the first input's buffer — that is what makes the mix an
    /// accumulate rather than a copy, and with one input it makes a gain a
    /// scaling in place that costs no buffer at all.
    Mix { out: Buf, inputs: Vec<MixIn> },
    /// Delay a buffer by a fixed number of samples.
    ///
    /// Inserted by the compiler to line up parallel paths, never placed by the
    /// user — the delay the user places is a `DelayWrite`/`DelayRead` pair.
    /// `slot` indexes the engine's compensation rings.
    Compensate { buf: Buf, slot: u16, samples: u32 },
    /// Fill a buffer with silence. Emitted for an input nobody connected.
    Silence { out: Buf },
    /// Read an audio delay line into a buffer.
    ///
    /// The read pointer is fractional and interpolated, so moving the time moves
    /// the pitch — the tape behaviour, which is what falls out of writing this
    /// the obvious way. `lane` names the schedule lane carrying the time when it
    /// is automated; without one, `time` is the whole story.
    DelayRead {
        out: Buf,
        line: u16,
        lane: Option<u16>,
        /// Static delay time in seconds (used if `lane` is `None`).
        time: f64,
        /// Seconds. The line never reads further back than this, whatever the
        /// automation says.
        max_time: f64,
    },
    /// Write buffer audio into an audio delay line ring buffer.
    DelayWrite { line: u16, a: Buf },
    /// Advance an audio delay line's write head over silence.
    ///
    /// Emitted for a line nothing writes. The read position is measured back
    /// from the head, so a head that stopped would replay the ring forever.
    DelaySilence { line: u16 },
}
