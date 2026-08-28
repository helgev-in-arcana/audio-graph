use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::ir::{Op, Operand};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, combo, fallback, key_control};
use crate::port::{Port, PortType};

/// Maximum number of selectable values for a key parameter.
#[cfg(feature = "ui")]
const MAX_VALUES: usize = 8;

/// How a key-switched parameter reads its keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyParamMode {
    /// The most recently pressed key selects its corresponding value.
    Select,
    /// A single key toggles sequentially through available values on each press.
    Toggle,
}

impl KeyParamMode {
    pub const ALL: [KeyParamMode; 2] = [KeyParamMode::Select, KeyParamMode::Toggle];

    pub fn label(self) -> &'static str {
        match self {
            KeyParamMode::Select => "Select",
            KeyParamMode::Toggle => "Toggle",
        }
    }
}

/// Selects a parameter value or signal based on incoming MIDI note keys.
///
/// Each value has an associated trigger key and input socket. If a value socket
/// is unconnected, it falls back to its configured scalar value. When no note
/// input is connected, the node outputs the first value.
///
/// Which value is chosen survives a recompile but not a reload: the first
/// value is what the node reads until a key is struck, which is the honest
/// answer for a control the DAW knows nothing about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyParam {
    pub mode: KeyParamMode,
    /// One key per value, in socket order.
    pub keys: Vec<u8>,
    /// The number each value socket falls back to while it is unwired.
    /// Missing entries are 0.0.
    #[serde(default)]
    pub values: Vec<f64>,
}

impl KeyParam {
    /// The value sockets sit after the notes port.
    fn value_port(index: usize) -> u8 {
        (index + 1) as u8
    }
}

impl Node for KeyParam {
    fn title(&self) -> String {
        "Key Param Select".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        let mut out = vec![Port::new("notes", PortType::Note)];
        for i in 0..self.keys.len() {
            let port = Port::param(format!("value {}", i + 1));
            // Allow removal only if more than one value exists.
            #[cfg(feature = "ui")]
            let port = if self.keys.len() > 1 {
                port.removable()
            } else {
                port
            };
            out.push(port);
        }
        out
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("out")]
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        // Each value, as a register or as the number on its row.
        let value = |cx: &ParamCx, index: usize| match cx.input(KeyParam::value_port(index)) {
            Some(reg) => Operand::Reg(reg),
            None => Operand::Value(self.values.get(index).copied().unwrap_or(0.0)),
        };

        if self.keys.is_empty() {
            // If no keys are configured, output zero.
            let out = cx.zero()?;
            cx.bind_output(0, out);
            return Ok(());
        }
        let first = value(cx, 0);

        // When note input is connected, track key presses to update latch state;
        // otherwise default to the first value.
        let position = if cx.has_input(0) {
            let state = cx.latch()?;
            match self.mode {
                KeyParamMode::Toggle => cx.emit(Op::KeyStep {
                    state,
                    key: self.keys[0],
                    count: self.keys.len() as u16,
                }),
                KeyParamMode::Select => {
                    for (index, &key) in self.keys.iter().enumerate() {
                        cx.emit(Op::KeyLatch {
                            state,
                            key,
                            value: index as f64,
                        });
                    }
                }
            }
            let position = cx.alloc()?;
            cx.emit(Op::Latch {
                out: position,
                state,
                initial: 0.0,
            });
            Some(position)
        } else {
            None
        };

        // Combine values into a single register selected by the current latch state.
        // The latch holds a whole number, so a `>=` per value is an exact pick and
        // costs one instruction each.
        let mut chosen = match first {
            Operand::Reg(reg) => reg,
            Operand::Value(value) => {
                let out = cx.alloc()?;
                cx.emit(Op::Const { out, value });
                out
            }
        };
        if let Some(position) = position {
            for index in 1..self.keys.len() {
                let next = cx.alloc()?;
                cx.emit(Op::Select {
                    out: next,
                    control: position,
                    threshold: index as f64,
                    low: Operand::Reg(chosen),
                    high: value(cx, index),
                });
                chosen = next;
            }
        }
        cx.bind_output(0, chosen);
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        combo(
            ui,
            "mode",
            &mut self.mode,
            &KeyParamMode::ALL,
            KeyParamMode::label,
        )
    }

    /// Input controls for setting fallback value and trigger key on a value socket row.
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
        if index >= self.keys.len() {
            return false;
        }
        self.values.resize(self.keys.len(), 0.0);
        let mut changed = fallback(ui, connected, |ui| {
            ui.add(egui::DragValue::new(&mut self.values[index]).speed(0.01))
                .changed()
        });
        // In Toggle mode, only the first key triggers stepping.
        let live = self.mode != KeyParamMode::Toggle || index == 0;
        let out = ui.add_enabled_ui(live, |ui| key_control(ui, "", &mut self.keys[index]));
        if !live {
            out.response
                .on_hover_text("Toggle moves on from one key — the first value's");
        }
        changed |= out.inner;
        changed
    }

    #[cfg(feature = "ui")]
    fn add_input_label(&self) -> Option<&'static str> {
        (self.keys.len() < MAX_VALUES).then_some("another value")
    }

    #[cfg(feature = "ui")]
    fn add_input(&mut self) {
        // Default new key to one semitone above the last configured key.
        let next = self.keys.last().map_or(24, |k| k.saturating_add(1));
        self.keys.push(next);
        self.values.resize(self.keys.len(), 0.0);
    }

    #[cfg(feature = "ui")]
    fn remove_input(&mut self, port: u8) -> u8 {
        let Some(index) = usize::from(port).checked_sub(1) else {
            return 0;
        };
        if self.keys.len() <= 1 || index >= self.keys.len() {
            return 0;
        }
        self.keys.remove(index);
        self.values.resize(self.keys.len() + 1, 0.0);
        self.values.remove(index);
        1
    }
}

#[cfg(feature = "ui")]
impl KeyParam {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, KeyParam)> {
        vec![(
            "Key Param Select",
            KeyParam {
                mode: KeyParamMode::Select,
                keys: vec![24, 25],
                values: vec![0.0, 1.0],
            },
        )]
    }
}
