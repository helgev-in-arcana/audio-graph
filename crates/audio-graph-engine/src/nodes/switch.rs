use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::ir::{Op, Operand};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, fallback};
use crate::port::Port;

/// One of two values, chosen by where a control sits against a threshold.
///
/// The parameter half's answer to a switch. `Math` can express a crossfade and
/// `RangeMap` a curve, but neither can say "this value below, that value
/// above" without a chain of three nodes that reads as arithmetic rather than
/// as a decision — and a decision is what the user is making.
///
/// Both values have a socket of their own, so a switch can pick between two
/// *signals* as easily as between two numbers; the numbers here are what is
/// used while a socket is empty, the same rule `Math`'s `b` follows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Switch {
    /// The control is "on" at this value and above. `>=` rather than `>` so
    /// that a gate wired from something that reaches exactly 1.0 switches.
    pub threshold: f64,
    pub off: f64,
    pub on: f64,
}

impl Node for Switch {
    fn title(&self) -> String {
        "Switch".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![
            Port::param("control"),
            Port::param("off"),
            Port::param("on"),
        ]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("out")]
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let control = cx.input_or_zero(0)?;
        let low = match cx.input(1) {
            Some(reg) => Operand::Reg(reg),
            None => Operand::Value(self.off),
        };
        let high = match cx.input(2) {
            Some(reg) => Operand::Reg(reg),
            None => Operand::Value(self.on),
        };
        let out = cx.alloc()?;
        cx.emit(Op::Select {
            out,
            control,
            threshold: self.threshold,
            low,
            high,
        });
        cx.bind_output(0, out);
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label("at");
            changed |= ui
                .add(egui::DragValue::new(&mut self.threshold).speed(0.01))
                .on_hover_text("the control switches at this value and above")
                .changed();
        });
        changed
    }

    /// Each value sits on the row of the socket it stands in for, and greys
    /// out when that socket is fed.
    #[cfg(feature = "ui")]
    fn input_control(
        &mut self,
        ui: &mut egui::Ui,
        port: u8,
        connected: bool,
        _cx: &mut NodeUi<'_>,
    ) -> bool {
        let value = match port {
            1 => &mut self.off,
            2 => &mut self.on,
            _ => return false,
        };
        fallback(ui, connected, |ui| {
            ui.add(egui::DragValue::new(value).speed(0.01)).changed()
        })
    }
}

#[cfg(feature = "ui")]
impl Switch {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, Switch)> {
        vec![(
            "Switch",
            Switch {
                threshold: 0.5,
                off: 0.0,
                on: 1.0,
            },
        )]
    }
}
