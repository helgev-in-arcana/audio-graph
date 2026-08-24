use crate::compile::AudioCx;
use crate::compile::{CompileError, DeclareCx, ParamCx};
use crate::ir::NoteSource;
/// Notes arriving from the DAW.
use crate::port::{Port, PortType};
///
/// Carries nothing, and stays a unit variant of [`NodeKind`][crate::NodeKind]
/// for that reason: a newtype around an empty struct would spell itself
/// `{"NoteIn": null}` on disk instead of `"NoteIn"`, and patches already
/// saved say the latter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NoteIn;

impl NoteIn {
    pub fn input_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    pub fn output_ports(&self) -> Vec<Port> {
        vec![Port::new("out", PortType::Note)]
    }

    pub fn title(&self) -> String {
        "Note in".into()
    }
}

impl NoteIn {
    pub(crate) fn compile(&self, _cx: &mut ParamCx) -> Result<(), CompileError> {
        Ok(())
    }
}

impl NoteIn {
    pub(crate) fn compile_audio(&self, _cx: &mut AudioCx) -> Result<(), CompileError> {
        Ok(())
    }
}

impl NoteIn {
    /// The note stream a plugin wired to this node plays from (§14.10).
    ///
    /// An identity rather than a buffer: the engine does not know what a note
    /// is, so it routes the *name* of a source and lets the adapter turn that
    /// into events.
    pub(crate) fn note_identity(&self) -> Option<NoteSource> {
        Some(NoteSource::Daw { bus: 0 })
    }
}

impl NoteIn {
    pub(crate) fn declare(&self, _cx: &mut DeclareCx) -> Result<(), CompileError> {
        Ok(())
    }
}
