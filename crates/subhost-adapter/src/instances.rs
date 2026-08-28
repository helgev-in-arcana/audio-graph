//! Audio processing interfaces and types for sub-plugin instances.
//!
//! Defines the audio processing trait [`AudioInstances`], channel chunk
//! layouts, note routing sources, and direct parameter targets.
//!
//! A caller here owns a graph, a chain, a rack — something that decides *when*
//! each sub-plugin runs and *what* it hears — but has no idea what is at the
//! other end of one, and never learns whether it was a VST3 or a CLAP.
//!
//! Everything crossing this line is a flat slice or a `Copy` value. That is a
//! deliberate constraint, not a coincidence: it keeps the boundary workable if
//! a sub-plugin is ever moved into a separate process, where a pointer or a
//! borrow could not cross. It is also why notes cross as a *name* and a key
//! mask rather than as a buffer — the caller routes without knowing what a
//! note is, and this side turns the name into events.

use plugin_host::AuxBuses;

/// Specifies the MIDI note event source for an instance.
///
/// An identity rather than a buffer, so that whatever the caller schedules
/// never has to hold a pointer into an event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NoteSource {
    /// No note events are routed to this instance — not the DAW's, not
    /// anyone's. Handing every instance every event the DAW sent is the
    /// tempting default and the wrong one: two synths then play in unison
    /// whatever the caller intended.
    #[default]
    None,
    /// Route note events from the specified host DAW note bus.
    Daw { bus: u16 },
    /// Route only note-off / release events from the specified note bus,
    /// suppressing note-ons. What a shut gate leaves.
    ///
    /// Blocking everything would be simpler and wrong: a note already sounding
    /// when the gate closed would never get its note-off, and a hung note
    /// outlives whatever caused it. Letting the releases through costs nothing
    /// and means a gate can be thrown mid-phrase without leaving wreckage.
    DawReleases { bus: u16 },
}

impl NoteSource {
    /// Returns a variant of this source that delivers only note release
    /// events — see [`NoteSource::DawReleases`]. Nothing is already nothing.
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
/// `mute` is a 128-bit mask corresponding to MIDI note numbers 0..127. If bit
/// `k` is set, note-on and note-off alike are suppressed for key `k`. Dropping
/// both halves is what keeps it safe: the note-on went too, so no sounding
/// voice is left waiting for its release — the opposite of the situation
/// [`NoteSource::DawReleases`] exists for.
///
/// A mask rather than a list because it is copied per chunk and tested one bit
/// per event, and because sixteen bytes is cheaper than a pointer plus the
/// lifetime that would come with it.
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
/// Channels are stored contiguously in planar format packed by `frames` —
/// the same layout `AudioBuffers` uses. The caller's pool has room for the
/// longest block the host promised, but the channels inside a chunk sit at
/// `frames` rather than at that maximum, so a short sub-block is a smaller
/// buffer rather than a sparse one and the slice can be handed straight to a
/// sub-plugin without repacking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioChunk {
    /// Total number of input channels across main and aux buses.
    pub input_channels: u16,
    /// Total number of output channels across main and aux buses.
    pub output_channels: u16,
    /// Where the joins in the input region are. Empty for the usual one-bus
    /// plugin.
    pub aux_inputs: AuxBuses,
    /// Where the joins in the output region are. Empty in the same case.
    pub aux_outputs: AuxBuses,
    pub frames: u32,
    /// Sample offset of this chunk within the current host audio block.
    ///
    /// Zero when the caller runs a whole block at once. When it cuts the block
    /// into sub-blocks instead, this is what lets the implementation cut the
    /// block's events and automation down to the part this call covers, with
    /// offsets rebased — without it, every chunk would be handed every event
    /// in the block and a note would sound once per chunk.
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
///
/// This is the one line between scheduling audio and hosting a plugin: the
/// caller decides *when* each instance runs and *what* it hears, and nothing
/// about the plugin format crosses back.
pub trait AudioInstances {
    /// Processes audio and events for the specified sub-plugin instance.
    ///
    /// Reads planar audio channels from `input` and writes planar channels to
    /// `output`. The two slices never alias. `output` is written in full for
    /// the frames the chunk covers; anything the implementation does not write
    /// is whatever the pool held, so a plugin that produces nothing must clear
    /// it.
    ///
    /// `notes` says which note stream this instance hears. It is a name and a
    /// key mask, not a buffer: the caller routes notes without knowing what
    /// one is, and the implementation turns the name into events and drops the
    /// keys the mask names.
    fn process(
        &mut self,
        instance: u32,
        notes: NoteStream,
        input: &[f32],
        output: &mut [f32],
        chunk: AudioChunk,
    );
}

/// An [`AudioInstances`] implementation that clears output buffers to
/// silence, for a wrapper with nothing loaded.
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
///
/// A property of the arrangement rather than of the plugin: whether a
/// sidechain is switched on depends on whether the caller wired anything to
/// it, and the caller is what knows this. Changing it means the sub-plugin has
/// to be deactivated and activated again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceIo {
    pub instance: u32,
    /// Channel count of the main input bus. Zero for an instrument.
    pub input_channels: u16,
    /// Aux input buses, in order. Only the ones the caller wired.
    pub aux_inputs: Vec<u16>,
    /// Channel count of the main output bus.
    pub output_channels: u16,
    /// Aux output buses, in order, and only as far as the caller reads them:
    /// a plugin's third output is absent when only the second is wired.
    pub aux_outputs: Vec<u16>,
}

/// Target identifier for a sub-plugin parameter driven directly by the audio
/// graph.
///
/// The other way in. A [`Slot`][crate::Slot] is published to the DAW as an
/// automation lane, so there is a fixed number of those; this is not limited
/// the same way because nothing outside the caller has to name it. Both arrive
/// on the same schedule and the merge does not care which is which — see
/// [`SlotSchedule`][crate::SlotSchedule].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParamTarget {
    pub instance: u32,
    pub param: u32,
}
