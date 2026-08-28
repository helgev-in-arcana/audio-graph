//! Audio instruction set and audio buffer operations.
//!
//! Defines instructions that process audio buffers ([`AudioOp`]), mixing inputs,
//! sub-block/whole-block evaluation chunking, and note event routing.

use subhost_adapter::{NoteSource, NoteStream};

/// An index into the audio buffer pool.
pub type Buf = u16;

/// One input of an [`AudioOp::Mix`]: source buffer, optional dynamic gain lane, and static gain.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MixIn {
    pub buf: Buf,
    pub lane: Option<u16>,
    pub gain: f64,
}

/// Evaluation granularity for audio processing operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Chunking {
    /// Evaluated once per host audio block.
    #[default]
    WholeBlock,
    /// Evaluated in sub-block increments (required when feedback delay loops are present).
    SubBlock,
}

/// Note stream routing and gating configuration for a plugin instance.
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
    /// When the gate lane value is below 0.5 (or missing), only release/note-off events are passed.
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

/// Audio processing instruction executed over audio buffer indices.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioOp {
    /// Copy host audio input bus into a buffer.
    Input { out: Buf, bus: u16 },
    /// Copy buffer contents to a host audio output bus.
    Output { a: Buf, bus: u16 },
    /// Execute a sub-plugin instance reading from `input` and writing to `output`.
    Plugin {
        instance: u32,
        /// Packed input buffer containing all active input buses (main followed by aux buses).
        input: Buf,
        /// Channel count for each input bus in `input`.
        input_buses: Vec<u16>,
        /// Packed output buffer for the plugin's output buses.
        output: Buf,
        /// Channel count for each output bus in `output`.
        output_buses: Vec<u16>,
        /// Note stream routing and gating configuration for this plugin instance.
        notes: NoteRoute,
    },
    /// Extract a single bus from a multi-bus packed plugin output buffer.
    Split {
        from: Buf,
        out: Buf,
        /// Starting channel offset of the bus within `from`.
        channel: u16,
        width: u16,
    },
    /// Assemble a packed plugin input buffer from individual source buffers, performing mono/stereo channel adaptation as needed.
    Gather { out: Buf, buses: Vec<(Buf, u16)> },
    /// Sum multiple input buffers with individual gains into an output buffer.
    Mix { out: Buf, inputs: Vec<MixIn> },
    /// Apply a fixed delay of `samples` to line up parallel paths with differing latencies.
    Compensate { buf: Buf, slot: u16, samples: u32 },
    /// Fill an audio buffer with silence.
    Silence { out: Buf },
    /// Read from an audio delay line with fractional Hermite interpolation.
    DelayRead {
        out: Buf,
        line: u16,
        lane: Option<u16>,
        /// Static delay time in seconds (used if `lane` is `None`).
        time: f64,
        /// Maximum delay limit in seconds.
        max_time: f64,
    },
    /// Write buffer audio into an audio delay line ring buffer.
    DelayWrite { line: u16, a: Buf },
}
