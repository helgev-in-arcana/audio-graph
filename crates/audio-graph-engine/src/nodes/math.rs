use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
pub use crate::ir::MathOp;
use crate::ir::{Op, Operand};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, combo, fallback};
use crate::port::Port;

/// Performs a mathematical operation on two parameter inputs.
///
/// If the second input port `b` is unconnected, it falls back to the configured constant `b`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Math {
    pub op: MathOp,
    pub b: f64,
}

impl Node for Math {
    fn title(&self) -> String {
        self.op.label().into()
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![Port::param("a"), Port::param("b")]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("out")]
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
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

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        combo(ui, "op", &mut self.op, &MathOp::ALL, MathOp::label)
    }

    /// Input control for the fallback value `b` when port 1 is unconnected.
    #[cfg(feature = "ui")]
    fn input_control(
        &mut self,
        ui: &mut egui::Ui,
        port: u8,
        connected: bool,
        _cx: &mut NodeUi<'_>,
    ) -> bool {
        if port != 1 {
            return false;
        }
        let b = &mut self.b;
        fallback(ui, connected, |ui| {
            ui.add(egui::DragValue::new(b).speed(0.01)).changed()
        })
    }
}

#[cfg(feature = "ui")]
impl Math {
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
