use serde::{Deserialize, Serialize};

use crate::compile::{AudioCx, CompileError, DeclareCx, ParamCx};
use crate::ir::{NoteSource, Op};
use crate::port::Port;

/// Rescale one range onto another. The 0..1 → plain-units half of §9.3 is
/// the slot table's job (`ResolvedTarget::to_plain`); this is the shaping
/// that happens before it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RangeMap {
    pub in_lo: f64,
    pub in_hi: f64,
    pub out_lo: f64,
    pub out_hi: f64,
    pub clamp: bool,
}

impl RangeMap {
    pub fn input_ports(&self) -> Vec<Port> {
        vec![Port::param("in")]
    }

    pub fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("out")]
    }

    pub fn title(&self) -> String {
        "Range map".into()
    }

    pub(crate) fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let a = cx.input_or_zero(0)?;
        let out = cx.alloc()?;
        cx.emit(Op::Range {
            out,
            a,
            in_lo: self.in_lo,
            in_span: self.in_hi - self.in_lo,
            out_lo: self.out_lo,
            out_span: self.out_hi - self.out_lo,
            clamp: self.clamp,
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
impl RangeMap {
    pub fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label("in");
            changed |= ui
                .add(egui::DragValue::new(&mut self.in_lo).speed(0.01))
                .changed();
            changed |= ui
                .add(egui::DragValue::new(&mut self.in_hi).speed(0.01))
                .changed();
        });
        ui.horizontal(|ui| {
            ui.label("out");
            changed |= ui
                .add(egui::DragValue::new(&mut self.out_lo).speed(0.01))
                .changed();
            changed |= ui
                .add(egui::DragValue::new(&mut self.out_hi).speed(0.01))
                .changed();
        });
        changed |= ui.checkbox(&mut self.clamp, "clamp").changed();
        changed
    }

    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, RangeMap)> {
        vec![(
            "Range map",
            RangeMap {
                in_lo: 0.0,
                in_hi: 1.0,
                out_lo: 0.0,
                out_hi: 1.0,
                clamp: true,
            },
        )]
    }
}
