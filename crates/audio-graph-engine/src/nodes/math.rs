use serde::{Deserialize, Serialize};

use crate::compile::AudioCx;
use crate::compile::DeclareCx;
use crate::compile::{CompileError, ParamCx};
pub use crate::ir::MathOp;
use crate::ir::NoteSource;
use crate::ir::{Op, Operand};
use crate::port::Port;

/// Two inputs and an operator. Input 1 falls back to `b` when unconnected,
/// so a "multiply by 0.5" node needs no second node feeding it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Math {
    pub op: MathOp,
    pub b: f64,
}

impl Math {
    pub fn input_ports(&self) -> Vec<Port> {
        vec![Port::param("a"), Port::param("b")]
    }

    pub fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("out")]
    }

    pub fn title(&self) -> String {
        self.op.label().into()
    }

    pub(crate) fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let a = cx.input_or_zero(0)?;
        let b = match cx.input(1) {
            Some(reg) => Operand::Reg(reg),
            None => Operand::Value(self.b),
        };
        let out = cx.alloc()?;
        cx.emit(Op::Math {
            out,
            a,
            b,
            op: self.op,
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
use crate::nodes::widgets::{NodeUi, combo};

#[cfg(feature = "ui")]
impl Math {
    pub fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        let mut changed = combo(ui, "op", &mut self.op, &MathOp::ALL, MathOp::label);
        ui.horizontal(|ui| {
            ui.label("b");
            changed |= ui
                .add(egui::DragValue::new(&mut self.b).speed(0.01))
                .changed();
        });
        ui.weak("b is used only while its input is unconnected");
        changed
    }

    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, Math)> {
        vec![(
            "Math",
            Math {
                op: MathOp::Multiply,
                b: 1.0,
            },
        )]
    }
}
