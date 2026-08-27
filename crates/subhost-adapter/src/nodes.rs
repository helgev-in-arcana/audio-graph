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
    /// The same stream with note-ons held back (§14.10).
    ///
    /// What a shut gate leaves. Blocking everything would be simpler and
    /// wrong: a note that was already sounding when the gate closed would
    /// never get its note-off, and a hung note outlives the patch that caused
    /// it. Letting the releases through costs nothing and means a gate can be
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
/// A mask rather than a list because the audio half copies this per chunk and
/// tests one bit per event, and because sixteen bytes is cheaper than a
/// pointer plus the lifetime that would come with it (ADR-6).
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
/// Planar and packed at `frames`, which is the same layout `AudioBuffers` uses
/// (§4.3). The pool has room for the longest block the host promised, but the
/// channels inside a chunk sit at `frames` rather than at that maximum — so a
/// short sub-block is a smaller buffer rather than a sparse one, and the slice
/// can be handed straight to a sub-plugin without repacking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioChunk {
    /// Channels in the input region: the main bus plus every aux bus (§14.11).
    pub input_channels: u16,
    /// Channels in the output region, counted the same way (§14.2).
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

/// How the engine runs a sub-plugin.
///
/// The engine schedules audio but has no idea what is at the other end of a
/// plugin node — this crate does not know what a VST3 is, and after M6 it will
/// not know what a CLAP is either (§7). Everything crossing this boundary is a
/// flat slice or a `Copy` value, for the same reason as §4.1: it has to still
/// work when the plugin is in another process (ADR-6).
pub trait AudioNodes {
    /// Run instance `instance` from `input` into `output`.
    ///
    /// The two slices never alias. `output` is written in full for the frames
    /// the chunk covers; anything the implementation does not write is whatever
    /// the pool held, so a plugin that produces nothing should clear it.
    ///
    /// `notes` says which note stream this instance hears (§14.10). It is a
    /// name and a key mask, not a buffer: the engine routes notes without
    /// knowing what one is, and the implementation is what turns the name into
    /// events and drops the keys the mask names.
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
pub struct NoNodes;

impl AudioNodes for NoNodes {
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
