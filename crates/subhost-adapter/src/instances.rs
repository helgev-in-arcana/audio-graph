//! Audio processing interfaces and types for sub-plugin instances.
//!
//! Defines the audio processing trait [`AudioInstances`], channel chunk layouts,
//! note routing sources, and direct parameter targets using flat slices and copyable types.

use plugin_host::AuxBuses;

/// Specifies the MIDI note event source for an instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NoteSource {
    /// No note events are routed to this instance.
    #[default]
    None,
    /// Route note events from the specified host DAW note bus.
    Daw { bus: u16 },
    /// Route only note-off / release events from the specified note bus, suppressing note-ons.
    DawReleases { bus: u16 },
}

impl NoteSource {
    /// Returns a variant of this source that delivers only note release events.
    pub fn releases_only(self) -> NoteSource {
        match self {
            NoteSource::None => NoteSource::None,
            NoteSource::Daw { bus } | NoteSource::DawReleases { bus } => {
                NoteSource::DawReleases { bus }
            }
        }
    }
}

/// Note source configuration and key-filtering mask for a sub-plugin instance.
///
/// `mute` is a 128-bit mask corresponding to MIDI note numbers 0..127. If bit `k`
/// is set, all note events for key `k` are suppressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NoteStream {
    pub source: NoteSource,
    pub mute: u128,
}

impl NoteStream {
    pub fn from_source(source: NoteSource) -> NoteStream {
        NoteStream { source, mute: 0 }
    }
}

/// Buffer layout and timing geometry for an audio processing chunk.
///
/// Channels are stored contiguously in planar format packed by `frames`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioChunk {
    /// Total number of input channels across main and aux buses.
    pub input_channels: u16,
    /// Total number of output channels across main and aux buses.
    pub output_channels: u16,
    /// Auxiliary input bus channel layout.
    pub aux_inputs: AuxBuses,
    /// Auxiliary output bus channel layout.
    pub aux_outputs: AuxBuses,
    pub frames: u32,
    /// Sample offset of this chunk within the current host audio block.
    pub offset: u32,
}

impl AudioChunk {
    /// Returns the slice index range for the given output channel.
    pub fn channel(&self, channel: u16) -> std::ops::Range<usize> {
        let start = channel as usize * self.frames as usize;
        start..start + self.frames as usize
    }
}

/// Audio processing interface for indexed sub-plugin instances.
pub trait AudioInstances {
    /// Processes audio and events for the specified sub-plugin instance.
    ///
    /// Reads planar audio channels from `input`, writes planar channels to `output`,
    /// and dispatches note events filtered according to `notes`.
    fn process(
        &mut self,
        instance: u32,
        notes: NoteStream,
        input: &[f32],
        output: &mut [f32],
        chunk: AudioChunk,
    );
}

/// An [`AudioInstances`] implementation that clears output buffers to silence.
pub struct NoInstances;

impl AudioInstances for NoInstances {
    fn process(
        &mut self,
        _instance: u32,
        _notes: NoteStream,
        _input: &[f32],
        output: &mut [f32],
        chunk: AudioChunk,
    ) {
        for ch in 0..chunk.output_channels {
            output[chunk.channel(ch)].fill(0.0);
        }
    }
}

/// Audio bus activation configuration for a specific sub-plugin instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceIo {
    pub instance: u32,
    /// Channel count of the main input bus (0 for instrument plugins without audio inputs).
    pub input_channels: u16,
    /// Channel counts for active auxiliary input buses.
    pub aux_inputs: Vec<u16>,
    /// Channel count of the main output bus.
    pub output_channels: u16,
    /// Channel counts for active auxiliary output buses.
    pub aux_outputs: Vec<u16>,
}

/// Target identifier for a sub-plugin parameter driven directly by the audio graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParamTarget {
    pub instance: u32,
    pub param: u32,
}
