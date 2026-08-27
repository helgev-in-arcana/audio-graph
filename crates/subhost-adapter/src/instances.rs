//! The contract between a host that schedules audio and the sub-plugins it runs.
//!
//! A caller here owns a graph, a chain, a rack — something that decides *when*
//! each sub-plugin runs and *what* it hears — but has no idea what is at the
//! other end of one. Everything crossing this line is a flat slice or a `Copy`
//! value, so it still works when the sub-plugin lives in another process
//! (ADR-6). That is why notes cross as a *name* and a key mask rather than as a
//! buffer: the caller routes without knowing what a note is, and this side is
//! what turns the name into events.

use plugin_host::AuxBuses;

/// Where one instance's notes come from.
///
/// An identity rather than a buffer. The caller decides which instance hears
/// which stream without knowing what a note is, and this side turns the name
/// into events — which is also what keeps whatever the caller schedules from
/// holding a pointer (ADR-6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NoteSource {
    /// Nothing routed to this instance.
    ///
    /// It gets no notes at all — not the DAW's, not anyone's. Handing every
    /// instance every event the DAW sent is the tempting default and the wrong
    /// one: two synths then play in unison whatever the caller intended.
    #[default]
    None,
    /// One of the wrapper's own note inputs from the DAW.
    Daw { bus: u16 },
    /// The same stream with note-ons held back.
    ///
    /// What a shut gate leaves. Blocking everything would be simpler and
    /// wrong: a note that was already sounding when the gate closed would
    /// never get its note-off, and a hung note outlives whatever caused it.
    /// Letting the releases through costs nothing and means a gate can be
    /// thrown mid-phrase without leaving wreckage.
    DawReleases { bus: u16 },
}

impl NoteSource {
    /// This source with its note-ons held back — see
    /// [`NoteSource::DawReleases`]. Nothing is already nothing.
    pub fn releases_only(self) -> NoteSource {
        match self {
            NoteSource::None => NoteSource::None,
            NoteSource::Daw { bus } | NoteSource::DawReleases { bus } => {
                NoteSource::DawReleases { bus }
            }
        }
    }
}

/// What one instance hears for a chunk: a stream, and the keys taken out of it.
///
/// `mute` is a bitmask over MIDI keys 0..128 — bit `k` set means key `k` never
/// reaches the plugin, note-on and note-off alike. Dropping both halves is
/// what keeps it safe: the note-on was dropped too, so there is no sounding
/// voice left waiting for its release, which is the opposite of the situation
/// [`NoteSource::DawReleases`] exists for.
///
/// A mask rather than a list because it is copied per chunk and tested one bit
/// per event, and because sixteen bytes is cheaper than a pointer plus the
/// lifetime that would come with it (ADR-6).
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

/// The shape of one chunk handed to a sub-plugin.
///
/// Planar and packed at `frames`, which is the same layout `AudioBuffers` uses.
/// The caller's pool has room for the longest block the host promised, but the
/// channels inside a chunk sit at `frames` rather than at that maximum — so a
/// short sub-block is a smaller buffer rather than a sparse one, and the slice
/// can be handed straight to a sub-plugin without repacking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioChunk {
    /// Channels in the input region: the main bus plus every aux bus.
    pub input_channels: u16,
    /// Channels in the output region, counted the same way.
    pub output_channels: u16,
    /// Where the joins in the input region are. Empty for the usual one-bus
    /// plugin.
    pub aux_inputs: AuxBuses,
    /// Where the joins in the output region are. Empty in the same case.
    pub aux_outputs: AuxBuses,
    pub frames: u32,
    /// Where this chunk starts inside the block the DAW handed us.
    ///
    /// Zero when the caller runs a whole block at once. When it cuts the block
    /// into sub-blocks instead, this is what lets the implementation cut the
    /// block's events and automation down to the part this call covers, with
    /// offsets rebased — without it, every chunk would be handed every event in
    /// the block and a note would sound once per chunk.
    pub offset: u32,
}

impl AudioChunk {
    /// One output channel of a chunk, as a range into the flat buffer.
    pub fn channel(&self, channel: u16) -> std::ops::Range<usize> {
        let start = channel as usize * self.frames as usize;
        start..start + self.frames as usize
    }
}

/// Every sub-plugin a caller can run, addressed by index.
///
/// This is the one line between scheduling audio and hosting a plugin. The
/// caller decides *when* each instance runs and *what* it hears; it has no idea
/// what is at the other end, and does not learn whether it was a VST3 or a
/// CLAP. Everything crossing back is a flat slice or a `Copy` value, so the
/// arrangement still works when the sub-plugin is in another process (ADR-6).
pub trait AudioInstances {
    /// Run instance `instance` from `input` into `output`.
    ///
    /// The two slices never alias. `output` is written in full for the frames
    /// the chunk covers; anything the implementation does not write is whatever
    /// the pool held, so a plugin that produces nothing should clear it.
    ///
    /// `notes` says which note stream this instance hears. It is a name and a
    /// key mask, not a buffer: the caller routes notes without knowing what one
    /// is, and the implementation is what turns the name into events and drops
    /// the keys the mask names.
    fn process(
        &mut self,
        instance: u32,
        notes: NoteStream,
        input: &[f32],
        output: &mut [f32],
        chunk: AudioChunk,
    );
}

/// An implementation that produces silence, for a wrapper with nothing loaded.
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

/// How one instance has to be activated: the buses that will actually be fed to
/// it.
///
/// A property of the arrangement rather than of the plugin — whether a
/// sidechain is switched on depends on whether the caller wired anything to it.
/// The caller is what knows this, which is why it comes in from that side; and
/// changing it means the sub-plugin has to be deactivated and activated again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceIo {
    pub instance: u32,
    /// Main input bus width. Zero for an instrument.
    pub input_channels: u16,
    /// Aux input buses, in order. Only the ones the caller wired.
    pub aux_inputs: Vec<u16>,
    /// Main output bus width.
    pub output_channels: u16,
    /// Aux output buses, in order. Only as far as the caller reads them, so a
    /// plugin's third output is absent when only the second is wired.
    pub aux_outputs: Vec<u16>,
}

/// One sub-plugin parameter the caller drives directly.
///
/// The other way in. A [`Slot`][crate::Slot] is the DAW's lane and is published
/// to it, so there is a fixed number of those; this is not limited the same way
/// because nothing outside the caller has to name it. Both arrive on the same
/// schedule and the merge does not care which is which — see [`SlotSchedule`][crate::SlotSchedule].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParamTarget {
    pub instance: u32,
    pub param: u32,
}
