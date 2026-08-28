use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::ir::{Op, Operand};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, combo, fallback, key_control};
use crate::port::{Port, PortType};

/// How many values one key parameter may choose between. A `Mix`'s ceiling, for
/// a `Mix`'s reason: past it the node is a wall of sockets.
#[cfg(feature = "ui")]
const MAX_VALUES: usize = 8;

/// How a key-switched parameter reads its keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyParamMode {
    /// The last key struck picks its own value. A bank of switches.
    Select,
    /// One key — the first value's — moving on to the next value each time it
    /// is struck, and round to the first again at the end. With two values that
    /// is a plain toggle, and it costs one key instead of two.
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

/// A parameter set from the keyboard: keys played to choose a value rather than
/// to sound.
///
/// The same gesture [`KeySwitch`][crate::KeySwitch] uses for routing, pointed at
/// a value instead — which is what a player wants when the thing to change is a
/// sub-plugin's parameter rather than which sub-plugin hears the notes.
///
/// Each value has a socket of its own, the way a [`Switch`][crate::Switch]'s two
/// do, so a key can pick between two *signals* as easily as between two numbers;
/// the number on the row is what is used while that socket is empty. The key
/// that chooses it sits on the same row, because a key belongs to the value it
/// picks and a list of keys elsewhere on the node is a thing to match up by
/// counting.
///
/// The notes port is what the keys are read from. Nothing wired means no keys
/// are read at all and the output stays on its first value — the same rule the
/// gates follow, and it makes an unwired node predictable rather than secretly
/// live.
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
            // The last value is not offered a remove button: a switch with one
            // position is a constant, and the node would have nothing to say.
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
            // Nothing to choose between. Reading as zero is what every other
            // empty parameter socket does.
            let out = cx.zero()?;
            cx.bind_output(0, out);
            return Ok(());
        }
        let first = value(cx, 0);

        // No notes, no keys: the node rests on its first value. Emitting the key
        // ops anyway would make an unwired node quietly follow a keyboard it is
        // not connected to.
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

        // Fold the values into one register, each later one winning where the
        // latch has reached it.
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

    /// The value's own number and the key that picks it, on the row of the
    /// socket they belong to.
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
        // In `Toggle` only the first key does anything — one key is the point
        // of that mode — so the rest are greyed rather than hidden, which would
        // make switching modes look like it lost them.
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
        // A semitone up from the last: a bank of key switches is a run of
        // adjacent keys far more often than it is not.
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
