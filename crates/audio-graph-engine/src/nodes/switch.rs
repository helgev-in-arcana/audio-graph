use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::ir::{Op, Operand};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, fallback};
use crate::port::Port;

/// How many values one switch may choose between. A `Mix`'s ceiling, for a
/// `Mix`'s reason: past it the node is a wall of sockets.
#[cfg(feature = "ui")]
const MAX_VALUES: usize = 8;

/// One of several values, chosen by where a control sits against a ladder of
/// thresholds.
///
/// The parameter half's answer to a switch. `Math` can express a crossfade and
/// `RangeMap` a curve, but neither can say "this value below, that value above"
/// without a chain of three nodes that reads as arithmetic rather than as a
/// decision — and a decision is what the user is making.
///
/// Every value has a socket of its own, so a switch can pick between *signals*
/// as easily as between numbers; the number on the row is what is used while
/// that socket is empty, the same rule `Math`'s `b` follows.
///
/// The first value has no threshold. It is what the control reads below every
/// other one, so a threshold on it would be a threshold to nothing — and the
/// node has to have an answer for a control that has not reached anything yet.
/// Each later value carries the threshold at which it takes over, `>=` rather
/// than `>` so that a control reaching exactly 1.0 switches.
///
/// The thresholds are not sorted or checked. Out of order they still mean
/// something exact — the last one the control has reached wins — and a switch
/// being edited passes through half-finished ladders on the way to a finished
/// one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "SwitchWire")]
pub struct Switch {
    /// The number each value socket falls back to while it is unwired. Missing
    /// entries are 0.0.
    pub values: Vec<f64>,
    /// The threshold at which each value *after the first* takes over, so
    /// `thresholds[i]` belongs to `values[i + 1]`. Missing entries are 0.0.
    pub thresholds: Vec<f64>,
}

/// What a `Switch` may be read from, old shape or new.
///
/// Before it grew a ladder, a switch was two values and one threshold. A patch
/// saved then still says `off`, `on` and `threshold`, and it means the two-value
/// ladder that says the same thing — so it is read as one rather than as a node
/// with no values at all.
#[derive(Deserialize)]
struct SwitchWire {
    #[serde(default)]
    values: Vec<f64>,
    #[serde(default)]
    thresholds: Vec<f64>,
    threshold: Option<f64>,
    off: Option<f64>,
    on: Option<f64>,
}

impl From<SwitchWire> for Switch {
    fn from(wire: SwitchWire) -> Switch {
        if !wire.values.is_empty() {
            return Switch {
                values: wire.values,
                thresholds: wire.thresholds,
            };
        }
        match (wire.off, wire.on) {
            (Some(off), Some(on)) => Switch {
                values: vec![off, on],
                thresholds: vec![wire.threshold.unwrap_or(0.5)],
            },
            // Neither shape. A node with one value and nothing to switch to is
            // what an empty file deserves, and the editor's `+` is right there.
            _ => Switch {
                values: vec![0.0],
                thresholds: Vec::new(),
            },
        }
    }
}

impl Switch {
    /// The value sockets sit after the control.
    fn value_port(index: usize) -> u8 {
        (index + 1) as u8
    }

    fn value(&self, index: usize) -> f64 {
        self.values.get(index).copied().unwrap_or(0.0)
    }

    /// The threshold at which `values[index]` takes over. The first value has
    /// none — it is what is read until something else has been reached.
    fn threshold(&self, index: usize) -> f64 {
        index
            .checked_sub(1)
            .and_then(|i| self.thresholds.get(i).copied())
            .unwrap_or(0.0)
    }
}

impl Node for Switch {
    fn title(&self) -> String {
        "Param Select".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        let mut out = vec![Port::param("control")];
        for i in 0..self.values.len() {
            let port = Port::param(format!("{}", i + 1));
            // The first value is not offered a remove button: it is the one
            // with no threshold, and a switch that had lost it would have
            // nothing to read below its own ladder.
            #[cfg(feature = "ui")]
            let port = if i > 0 { port.removable() } else { port };
            out.push(port);
        }
        out
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("out")]
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let control = cx.input_or_zero(0)?;
        let operand = |cx: &ParamCx, index: usize| match cx.input(Switch::value_port(index)) {
            Some(reg) => Operand::Reg(reg),
            None => Operand::Value(self.value(index)),
        };
        if self.values.is_empty() {
            let out = cx.zero()?;
            cx.bind_output(0, out);
            return Ok(());
        }

        // The first value, in a register so the ladder has something to build
        // on. It is what the output is when the control has reached nothing.
        let mut chosen = match operand(cx, 0) {
            Operand::Reg(reg) => reg,
            Operand::Value(value) => {
                let out = cx.alloc()?;
                cx.emit(Op::Const { out, value });
                out
            }
        };
        // One `Select` per rung, each overriding the last where the control has
        // reached it. Written this way round — later rungs on the outside —
        // the last threshold the control has passed is the one that wins, which
        // is what a ladder reads as.
        for index in 1..self.values.len() {
            let next = cx.alloc()?;
            cx.emit(Op::Select {
                out: next,
                control,
                threshold: self.threshold(index),
                low: Operand::Reg(chosen),
                high: operand(cx, index),
            });
            chosen = next;
        }
        cx.bind_output(0, chosen);
        Ok(())
    }

    /// The value each socket stands in for, and the threshold that picks it, on
    /// the row of the socket they belong to.
    #[cfg(feature = "ui")]
    fn input_control(
        &mut self,
        ui: &mut egui::Ui,
        port: u8,
        connected: bool,
        _cx: &mut NodeUi<'_>,
    ) -> bool {
        let Some(index) = usize::from(port).checked_sub(1) else {
            return false;
        };
        if index >= self.values.len() {
            return false;
        }
        self.thresholds.resize(self.values.len().max(1) - 1, 0.0);
        let mut changed = false;
        // The row runs right to left, so the first thing added is the
        // right-hand one: threshold, then value, reading back as
        // "socket, value, threshold".
        if let Some(rung) = index.checked_sub(1) {
            changed |= ui
                .add(egui::DragValue::new(&mut self.thresholds[rung]).speed(0.01))
                .on_hover_text("the control picks this value at this threshold and above")
                .changed();
        }
        changed |= fallback(ui, connected, |ui| {
            ui.add(egui::DragValue::new(&mut self.values[index]).speed(0.01))
                .changed()
        });
        changed
    }

    #[cfg(feature = "ui")]
    fn add_input_label(&self) -> Option<&'static str> {
        (self.values.len() < MAX_VALUES).then_some("another value")
    }

    #[cfg(feature = "ui")]
    fn add_input(&mut self) {
        self.values.push(0.0);
        // A rung above the last, so a second press gives a ladder rather than
        // two rungs at the same height that can never both be reached.
        let next = self.thresholds.last().map_or(0.5, |t| t + 0.5);
        self.thresholds.push(next);
    }

    #[cfg(feature = "ui")]
    fn remove_input(&mut self, port: u8) -> u8 {
        let Some(index) = usize::from(port).checked_sub(1) else {
            return 0;
        };
        if index == 0 || index >= self.values.len() {
            return 0;
        }
        self.values.remove(index);
        self.thresholds.resize(self.values.len(), 0.0);
        self.thresholds.remove(index - 1);
        1
    }
}

#[cfg(feature = "ui")]
impl Switch {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, Switch)> {
        vec![(
            "Param Select",
            Switch {
                values: vec![0.0, 1.0],
                thresholds: vec![0.5],
            },
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A patch saved when a switch was two values and one threshold still means
    /// what it meant. Reading it as a node with no values would empty the socket
    /// its links point at.
    #[test]
    fn the_old_two_value_shape_reads_as_a_two_rung_ladder() {
        let old = r#"{"threshold": 0.6, "off": 0.2, "on": 0.9}"#;
        let switch: Switch = serde_json::from_str(old).unwrap();
        assert_eq!(switch.values, vec![0.2, 0.9]);
        assert_eq!(switch.thresholds, vec![0.6]);
    }

    /// And the ports are where the links expect them: control, then a socket per
    /// value.
    #[test]
    fn the_old_shape_keeps_its_socket_order() {
        let old = r#"{"threshold": 0.5, "off": 0.0, "on": 1.0}"#;
        let switch: Switch = serde_json::from_str(old).unwrap();
        let names: Vec<String> = switch
            .input_ports()
            .iter()
            .map(|p| p.name.to_string())
            .collect();
        assert_eq!(names, vec!["control", "1", "2"]);
    }
}
