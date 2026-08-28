use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::ir::{Op, Operand};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, fallback};
use crate::port::Port;

/// Maximum number of selectable values supported by the Switch node.
#[cfg(feature = "ui")]
const MAX_VALUES: usize = 8;

/// Selects among multiple input values or signals based on threshold rungs.
///
/// Output begins at the base value (`values[0]`) when below all thresholds.
/// Each subsequent threshold in `thresholds` activates its corresponding value
/// in `values[1..]` when the control signal meets or exceeds (`>=`) that threshold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(from = "SwitchWire")]
pub struct Switch {
    /// Fallback scalar values for unwired value sockets. Defaults to 0.0.
    pub values: Vec<f64>,
    /// Thresholds for activating values after the first (`thresholds[i]` corresponds to `values[i + 1]`).
    pub thresholds: Vec<f64>,
}

/// Deserialization helper supporting legacy 2-state format (`off`/`on`/`threshold`) and multi-value ladders.
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
            // Fallback for unrecognized payload: single 0.0 value with no thresholds.
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

    /// Threshold for `values[index]`. Returns 0.0 for index 0 (which has no threshold).
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
            // First value cannot be removed as it represents the baseline output.
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

        // Initialize base register with the first value.
        let mut chosen = match operand(cx, 0) {
            Operand::Reg(reg) => reg,
            Operand::Value(value) => {
                let out = cx.alloc()?;
                cx.emit(Op::Const { out, value });
                out
            }
        };
        // Evaluate ladder rungs sequentially, overriding output as thresholds are met.
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

    /// Renders threshold and fallback value controls for a value socket row.
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
        // Draw threshold input for rungs after the base value.
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
        // Set next threshold 0.5 above the previous rung.
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

    // Verify backward compatibility with legacy 2-state format.
    #[test]
    fn the_old_two_value_shape_reads_as_a_two_rung_ladder() {
        let old = r#"{"threshold": 0.6, "off": 0.2, "on": 0.9}"#;
        let switch: Switch = serde_json::from_str(old).unwrap();
        assert_eq!(switch.values, vec![0.2, 0.9]);
        assert_eq!(switch.thresholds, vec![0.6]);
    }

    // Verify port ordering matches expected layout (control followed by value ports).
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
