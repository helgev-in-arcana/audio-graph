use crate::compile::{AudioCx, CompileError, DeclareCx, ParamCx};
use crate::ir::NoteSource;
use crate::port::{Port, PortType};

/// Notes arriving from the DAW.
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

    pub(crate) fn compile(&self, _cx: &mut ParamCx) -> Result<(), CompileError> {
        Ok(())
    }

    pub(crate) fn compile_audio(&self, _cx: &mut AudioCx) -> Result<(), CompileError> {
        Ok(())
    }

    /// The note stream a plugin wired to this node plays from (§14.10).
    ///
    /// An identity rather than a buffer: the engine does not know what a note
    /// is, so it routes the *name* of a source and lets the adapter turn that
    /// into events.
    pub(crate) fn note_identity(&self) -> Option<NoteSource> {
        Some(NoteSource::Daw { bus: 0 })
    }

    pub(crate) fn declare(&self, _cx: &mut DeclareCx) -> Result<(), CompileError> {
        Ok(())
    }
}

#[cfg(feature = "ui")]
use crate::nodes::widgets::NodeUi;

#[cfg(feature = "ui")]
impl NoteIn {
    /// A source with nothing to set.
    pub fn controls(&mut self, _ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        false
    }

    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, NoteIn)> {
        vec![("Note in", NoteIn)]
    }
}
