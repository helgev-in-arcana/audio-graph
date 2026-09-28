//! Audio instruction set and audio buffer operations.
//!
//! Kept apart from [`Op`][crate::Op] because the two halves run at different
//! rates. Buffers are indices into a pool the engine owns; nothing here is a
//! pointer, so a `Program` stays a value that could cross a process boundary
//! unchanged.

use serde::{Deserialize, Serialize};

use crate::ir::{NoteBuf, RateSpec, Waveform};

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

/// Supported sub-block sizes in samples: how long a chunk is when a stage runs
/// at [`Chunking::SubBlock`], and so the shortest an audio delay can be. Powers
/// of two, so a block cut into chunks and into parameter rows lines up either
/// way round.
///
/// Nothing to do with how often a parameter moves; that is the parameter
/// resolution, a separate setting. A sub-block smaller than 16 would call
/// every sub-plugin in a loop more than 250 times for a 4096-sample block.
pub const QUANTUM_CHOICES: [u32; 4] = [16, 32, 64, 128];

/// Default sub-block size in samples: about 0.67 ms at 48 kHz.
pub const DEFAULT_QUANTUM: u32 = 32;

/// Evaluation granularity for audio processing operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Chunking {
    /// Once per block the DAW hands us. Parameter changes still arrive at
    /// the parameter resolution, as events with an offset — there is no
    /// reason to call a plugin more often than the DAW does.
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
    /// The lanes its ops' registers drive, in `Program::outputs`.
    ///
    /// A stage writes these and no others. The consumer of a value may sit in
    /// a later stage than the op that made it — a plugin with one socket fed
    /// by an LFO and another by an envelope follower — and a later stage
    /// writing every lane would copy the earlier stage's last row, which is
    /// what its register holds by then, over every row of that lane.
    pub outputs: Span,
    /// Which note buffers those note ops write, one bit each.
    ///
    /// The engine records where a buffer stood before each row so the
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
    /// Scale a buffer by a gain that slides towards its target instead of
    /// stepping to it.
    ///
    /// What a [`AudioOp::Mix`] of one cannot do: a mix holds its gain for a
    /// whole row, so a gate switching a loud signal steps the waveform at a
    /// row boundary and clicks. Here the gain moves sample by sample towards
    /// a target re-read every row.
    ///
    /// `out` may be `a`, which makes it a scaling in place that costs no
    /// buffer. Nothing about the ramp is fixed at compile time except its
    /// slope: where a fade starts is wherever the last one left off, and it
    /// may reverse mid-travel, so `state` names the latch the gain lives in
    /// between blocks. A latch is NaN until it has run, and the first block
    /// takes its target as it stands — a patch loaded with the gate open must
    /// not fade in.
    Fade {
        out: Buf,
        a: Buf,
        state: u16,
        /// The lane carrying the target gain in decibels. Without one, `gain`
        /// is the whole story.
        lane: Option<u16>,
        gain: f64,
        /// Seconds to travel the whole range, rising and falling. The slope is
        /// what is constant, so a gain a third of the way up falls back in a
        /// third of `fall`.
        rise: f64,
        fall: f64,
    },
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
        /// The latch holding where this read's pointer stood at the end of the
        /// last chunk, in samples, so the next chunk sweeps from there.
        ///
        /// A latch because it is keyed by node and carried across a swap: the
        /// time is baked into the op, every edit to it recompiles, and a
        /// position that started over at each swap would jump to the new time
        /// in one sample. Numbering reads as they run instead would give reads
        /// in different stages the same number and each other's positions.
        state: u16,
        lane: Option<u16>,
        /// Static delay time in seconds (used if `lane` is `None`).
        time: f64,
        /// Seconds. The line never reads further back than this, whatever the
        /// automation says.
        max_time: f64,
    },
    /// Write buffer audio into an audio delay line ring buffer.
    DelayWrite { line: u16, a: Buf },
    /// A sample-by-sample operation on a buffer, or on two.
    ///
    /// `out` may be `a`: every op reads a sample before writing it. `b` is the
    /// second operand where the op takes one, and `None` when nothing is wired
    /// to it — see [`AudioMathOp`] for what each op does then. `state` is the
    /// op's history between blocks, booked by the node whether or not this op
    /// needs one, so switching ops does not change what the node owns.
    Math {
        out: Buf,
        a: Buf,
        b: Option<Buf>,
        op: AudioMathOp,
        state: u16,
    },
    /// Scale a buffer by a gain that swings between 1 and `1 - depth`, one
    /// cycle of `waveform` at `rate`: a tremolo.
    ///
    /// The oscillator runs at the sample rate rather than being an LFO node
    /// driving a gain, because a parameter changes only at a row
    /// boundary and a gain that steps every 32 samples at a tremolo's speed is
    /// audible as a buzz. `state` holds the phase, so it runs on through a
    /// recompile, and the depth it last applied, which is ramped rather than
    /// stepped when the lane moves it. `lane` carries the depth when its
    /// socket is wired; without one, `depth` is the whole story.
    Tremolo {
        out: Buf,
        a: Buf,
        state: u16,
        lane: Option<u16>,
        depth: f64,
        waveform: Waveform,
        rate: RateSpec,
    },
    /// Advance an audio delay line's write head over silence.
    ///
    /// Emitted for a line nothing writes. The read position is measured back
    /// from the head, so a head that stopped would replay the ring forever.
    DelaySilence { line: u16 },
}

/// What an [`AudioOp::Math`] does to each sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AudioMathOp {
    /// Take out any constant offset: a first-order high-pass at
    /// [`DC_CUTOFF_HZ`]. What a chain that rectifies, waveshapes or sums an
    /// offset in leaves behind, and what eats headroom and thumps on the next
    /// gate.
    #[default]
    RemoveDc,
    /// Flip the polarity.
    Invert,
    /// The magnitude of each sample: full-wave rectification.
    Rectify,
    /// `a × b`, sample by sample: ring modulation. With nothing wired to `b`,
    /// `a` passes unchanged rather than going silent — an unwired modulator
    /// is not a modulator of zero.
    Multiply,
}

/// Where [`AudioMathOp::RemoveDc`] crosses over, in hertz. Low enough to leave
/// the lowest note of a piano (27.5 Hz) alone, high enough to settle an offset
/// within a fraction of a second.
pub const DC_CUTOFF_HZ: f64 = 5.0;

impl AudioMathOp {
    pub const ALL: [AudioMathOp; 4] = [
        AudioMathOp::RemoveDc,
        AudioMathOp::Invert,
        AudioMathOp::Rectify,
        AudioMathOp::Multiply,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AudioMathOp::RemoveDc => "Remove DC",
            AudioMathOp::Invert => "Invert",
            AudioMathOp::Rectify => "Rectify",
            AudioMathOp::Multiply => "Multiply",
        }
    }

    /// Whether the op reads a second signal.
    pub fn takes_b(self) -> bool {
        matches!(self, AudioMathOp::Multiply)
    }
}
