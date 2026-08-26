use crate::ir::NoteSource;
use crate::nodes::Node;
use crate::port::{Port, PortType};

/// Notes arriving from the DAW.
///
/// Carries nothing, and stays a unit variant of [`NodeKind`][crate::NodeKind]
/// for that reason: a newtype around an empty struct would spell itself
/// `{"NoteIn": null}` on disk instead of `"NoteIn"`, and patches already
/// saved say the latter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NoteIn;

impl Node for NoteIn {
    fn title(&self) -> String {
        "MIDI In".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new("out", PortType::Note)]
    }

    /// The note stream a plugin wired to this node plays from (§14.10).
    ///
    /// An identity rather than a buffer: the engine does not know what a note
    /// is, so it routes the *name* of a source and lets the adapter turn that
    /// into events.
    fn note_identity(&self) -> Option<NoteSource> {
        Some(NoteSource::Daw { bus: 0 })
    }
}

#[cfg(feature = "ui")]
impl NoteIn {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, NoteIn)> {
        vec![("MIDI In", NoteIn)]
    }
}
