use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::ir::Op;
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, combo, key_control};
use crate::port::Port;

/// One key and the value it stands for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyValue {
    pub key: u8,
    pub value: f64,
}

/// How a key-switched parameter reads its keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyParamMode {
    /// One key, flipping between the resting value and the first entry's.
    Toggle,
    /// A bank of keys, each standing for a value. The last one struck wins.
    Select,
}

impl KeyParamMode {
    pub const ALL: [KeyParamMode; 2] = [KeyParamMode::Toggle, KeyParamMode::Select];

    pub fn label(self) -> &'static str {
        match self {
            KeyParamMode::Toggle => "Toggle",
            KeyParamMode::Select => "Select",
        }
    }
}

/// A parameter set from the keyboard: keys played to change a value rather
/// than to sound.
///
/// The same gesture [`KeySwitch`][crate::KeySwitch] uses for routing, pointed
/// at a number instead — which is what a player wants when the thing to switch
/// is a sub-plugin's parameter rather than which sub-plugin hears the notes.
///
/// `Toggle` is one key flipping between two values. `Select` is a row of keys,
/// one value each, and the last one struck wins — which is what a bank of
/// switches does, and what makes a three-way or a five-way possible without
/// three or five nodes.
///
/// The value survives a recompile, so editing an unrelated node does not put
/// the parameter back where it started. It does *not* survive reopening the
/// patch: `resting` is what the node reads until a key is struck, and that is
/// the honest answer for a control the DAW knows nothing about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyParam {
    pub mode: KeyParamMode,
    /// `Toggle` uses the first entry and nothing else; `Select` uses them all.
    pub keys: Vec<KeyValue>,
    /// What the output reads before any key has been struck — and, in
    /// `Toggle`, the value it flips back to.
    pub resting: f64,
}

impl Node for KeyParam {
    fn title(&self) -> String {
        "Key parameter".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        Vec::new()
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("out")]
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let state = cx.latch()?;
        match self.mode {
            KeyParamMode::Toggle => {
                // Nothing to flip with no key named: the node then just reads
                // its resting value, which is a half-built patch behaving
                // predictably rather than refusing to compile.
                if let Some(entry) = self.keys.first() {
                    cx.emit(Op::KeyToggle {
                        state,
                        key: entry.key,
                        off: self.resting,
                        on: entry.value,
                    });
                }
            }
            KeyParamMode::Select => {
                for entry in &self.keys {
                    cx.emit(Op::KeyLatch {
                        state,
                        key: entry.key,
                        value: entry.value,
                    });
                }
            }
        }
        let out = cx.alloc()?;
        cx.emit(Op::Latch {
            out,
            state,
            initial: self.resting,
        });
        cx.bind_output(0, out);
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        let mut changed = combo(
            ui,
            "mode",
            &mut self.mode,
            &KeyParamMode::ALL,
            KeyParamMode::label,
        );
        ui.horizontal(|ui| {
            ui.label("resting");
            changed |= ui
                .add(egui::DragValue::new(&mut self.resting).speed(0.01))
                .on_hover_text("what the output reads until a key is struck")
                .changed();
        });

        // Toggle needs exactly one; Select takes as many as the user adds.
        // Rows past the first are drawn anyway in Toggle rather than hidden,
        // so switching modes does not look like it lost them.
        let removable = self.mode == KeyParamMode::Select && self.keys.len() > 1;
        let mut remove = None;
        for (index, entry) in self.keys.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                changed |= key_control(ui, "", &mut entry.key);
                changed |= ui
                    .add(egui::DragValue::new(&mut entry.value).speed(0.01))
                    .changed();
                if removable && ui.small_button("\u{00d7}").clicked() {
                    remove = Some(index);
                }
            });
        }
        if let Some(index) = remove {
            self.keys.remove(index);
            changed = true;
        }
        if self.mode == KeyParamMode::Select && self.keys.len() < 16 && ui.button("+ key").clicked()
        {
            let next = self.keys.last().map_or(24, |e| e.key.saturating_add(1));
            self.keys.push(KeyValue {
                key: next,
                value: 1.0,
            });
            changed = true;
        }
        changed
    }
}

#[cfg(feature = "ui")]
impl KeyParam {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, KeyParam)> {
        vec![(
            "Key parameter",
            KeyParam {
                mode: KeyParamMode::Toggle,
                keys: vec![KeyValue {
                    key: 24,
                    value: 1.0,
                }],
                resting: 0.0,
            },
        )]
    }
}
