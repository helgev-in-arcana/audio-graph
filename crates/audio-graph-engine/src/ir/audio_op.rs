//! Audio instruction set and audio buffer operations.
//!
//! Kept apart from [`Op`][crate::Op] because the two halves run at different
//! rates. Buffers are indices into a pool the engine owns; nothing here is a
//! pointer, so a `Program` stays a value that could cross a process boundary
//! unchanged.

use crate::ir::NoteBuf;

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
    /// Once per sub-block. What the two ends of a delay line need, because a
    /// delay is at least one chunk long and a whole-block chunk would put the
    /// floor at ten milliseconds.
    SubBlock,
}

/// A run of one op list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn range(&self) -> std::ops::Range<usize> {
        self.start as usize..self.end as usize
    }

    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// One slice of a program: the parameter, note and audio ops that run
/// together, at one granularity.
///
/// A program's three lists are one topological order between them, cut into
/// stages. A stage covers the whole DAW block before the next one starts, and
/// inside it the order is parameters, then notes, then audio — the order a
/// signal changes rate in. What differs from one stage to the next is whether
/// its audio ops are called once for the block or once per sub-block.
///
/// Two things ask for a cut. A delay line's two ends have to run at the
/// quantum, because a delay is at least one chunk long. And a parameter read
/// off audio cannot be worked out until that audio exists, which is what makes
/// an envelope follower expressible at all: the stage holding it runs after
/// the stage that made the sound it is measuring.
///
/// Granularity is per stage rather than per program. One answer for the whole
/// program would mean a delay line anywhere in a patch calling every plugin in
/// it once per sub-block. How often a sub-plugin is called is a cost; how short
/// a delay the graph can express is not the same question, and should not be
/// paid for by everything that asked neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stage {
    /// Where this stage's ops sit in `Program::ops`.
    pub params: Span,
    /// Its ops in `Program::note_ops`.
    pub notes: Span,
    /// Its ops in `Program::audio_ops`.
    pub audio: Span,
    /// Which note buffers those note ops write, one bit each.
    ///
    /// The engine records where a buffer stood before each sub-block so the
    /// audio half can find its own rows again. Only the stage that fills a
    /// buffer may write that mark: a later stage passing over the same rows
    /// would overwrite every one of them with the length the buffer finished
    /// at, and the audio half would read the whole block as one row.
    pub note_bufs: u16,
    pub chunking: Chunking,
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
        /// The note buffer this instance hears, or `None` when nothing is
        /// wired to its notes port.
        ///
        /// `None` rather than an empty buffer because an unwired instrument
        /// has to hear nothing: handing every instance whatever the DAW sent
        /// is the tempting default and the wrong one — two synths then play in
        /// unison whatever the patch says.
        notes: Option<NoteBuf>,
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
    /// sidechain is averaged, a mono source into a stereo bus is duplicated.
    /// The two are inverses, so a round trip keeps its level. That
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
