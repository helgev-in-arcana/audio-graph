use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError, DeclareCx, ParamCx};
use crate::ir::{NoteSource, Op};
use crate::port::Port;

/// A fixed number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Constant {
    pub value: f64,
}

impl Constant {
    pub fn input_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    pub fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("out")]
    }

    pub fn title(&self) -> String {
        "Constant".into()
    }

    pub(crate) fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let out = cx.alloc()?;
        cx.emit(Op::Const {
            out,
            value: self.value,
        });
        cx.bind_output(0, out);
        Ok(())
    }

    pub(crate) fn compile_audio(&self, _cx: &mut AudioCx) -> Result<(), CompileError> {
        Ok(())
    }

    pub(crate) fn declare(&self, _cx: &mut DeclareCx) -> Result<(), CompileError> {
        Ok(())
    }

    pub(crate) fn note_identity(&self) -> Option<NoteSource> {
        None
    }
}

#[cfg(feature = "ui")]
use crate::nodes::widgets::NodeUi;

#[cfg(feature = "ui")]
impl Constant {
    pub fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        ui.add(egui::Slider::new(&mut self.value, 0.0..=1.0))
            .changed()
    }

    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, Constant)> {
        vec![("Constant", Constant { value: 0.5 })]
    }
}
