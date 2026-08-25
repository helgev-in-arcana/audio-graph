use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
pub use crate::ir::MathOp;
use crate::ir::{Op, Operand};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, combo, fallback};
use crate::port::Port;

/// Two inputs and an operator. Input 1 falls back to `b` when unconnected,
/// so a "multiply by 0.5" node needs no second node feeding it.
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

    /// `b` sits on the row of the socket it stands in for, and greys out when
    /// that socket is fed — which is the whole of what the line of prose under
    /// this node used to say.
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
