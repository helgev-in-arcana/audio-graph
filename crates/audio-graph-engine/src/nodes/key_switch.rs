use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::ir::{MathOp, Op, Operand, Reg};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, combo, key_control};
use crate::port::{Port, PortType};

/// Maximum number of output destinations for a key switch.
#[cfg(feature = "ui")]
const MAX_WAYS: usize = 8;

/// What a key switch does with the keys it watches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeySwitchMode {
    /// Each output routes notes while its assigned key is held down.
    Hold,
    /// The most recently pressed key activates its output and closes others.
    Select,
    /// A single key toggles sequentially through outputs on each press.
    Toggle,
}

impl KeySwitchMode {
    pub const ALL: [KeySwitchMode; 3] = [
        KeySwitchMode::Hold,
        KeySwitchMode::Select,
        KeySwitchMode::Toggle,
    ];

    pub fn label(self) -> &'static str {
        match self {
            KeySwitchMode::Hold => "Hold",
            KeySwitchMode::Select => "Select",
            KeySwitchMode::Toggle => "Toggle",
        }
    }
}

/// Routes MIDI note streams to different output destinations based on trigger keys.
///
/// Supports momentary (`Hold`), latching (`Select`), and sequential (`Toggle`) modes.
/// Trigger keys can optionally be filtered out (`mute_keys`) so they do not produce sound
/// downstream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeySwitch {
    pub mode: KeySwitchMode,
    /// Trigger keys assigned to each output in socket order.
    pub keys: Vec<u8>,
    /// Whether trigger keys are removed from downstream note output streams.
    #[serde(default = "muted")]
    pub mute_keys: bool,
}

impl Node for KeySwitch {
    fn title(&self) -> String {
        "Key MIDI Route".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![Port::new("notes", PortType::Note)]
    }

    fn output_ports(&self) -> Vec<Port> {
        (0..self.keys.len())
            .map(|i| {
                let port = Port::new(format!("out {}", i + 1), PortType::Note);
                // Allow removal only when more than one output destination exists.
                #[cfg(feature = "ui")]
                let port = if self.keys.len() > 1 {
                    port.removable()
                } else {
                    port
                };
                port
            })
            .collect()
    }

    /// All outputs forward notes from the primary input when their gate condition is met.
    fn note_passthrough(&self, port: u8) -> Option<u8> {
        (usize::from(port) < self.keys.len()).then_some(0)
    }

    /// Bitmask of MIDI keys to suppress from outgoing note streams when `mute_keys` is active.
    fn note_mute(&self, port: u8) -> u128 {
        if !self.mute_keys || usize::from(port) >= self.keys.len() {
            return 0;
        }
        self.keys
            .iter()
            .filter(|&&key| key < 128)
            .fold(0u128, |mask, &key| mask | (1u128 << key))
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        match self.mode {
            KeySwitchMode::Hold => {
                for (port, &key) in self.keys.iter().enumerate() {
                    let held = cx.alloc()?;
                    cx.emit(Op::KeyHeld { out: held, key });
                    let condition = fold_upstream(cx, held)?;
                    cx.bind_note_gate(port as u8, condition)?;
                }
                Ok(())
            }
            KeySwitchMode::Select | KeySwitchMode::Toggle => {
                if self.keys.is_empty() {
                    return Ok(());
                }
                // Maintain active destination state in a latch register.
                let state = cx.latch()?;
                match self.mode {
                    KeySwitchMode::Toggle => cx.emit(Op::KeyStep {
                        state,
                        key: self.keys[0],
                        count: self.keys.len() as u16,
                    }),
                    _ => {
                        for (port, &key) in self.keys.iter().enumerate() {
                            cx.emit(Op::KeyLatch {
                                state,
                                key,
                                value: port as f64,
                            });
                        }
                    }
                }
                for port in 0..self.keys.len() {
                    let chosen = cx.alloc()?;
                    cx.emit(Op::LatchIs {
                        out: chosen,
                        state,
                        value: port as f64,
                        // Default to the first output destination initially.
                        initial: 0.0,
                    });
                    let condition = fold_upstream(cx, chosen)?;
                    cx.bind_note_gate(port as u8, condition)?;
                }
                Ok(())
            }
        }
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        let mut changed = combo(
            ui,
            "mode",
            &mut self.mode,
            &KeySwitchMode::ALL,
            KeySwitchMode::label,
        );
        // Toggle whether trigger keys are stripped from downstream note output.
        changed |= ui
            .checkbox(&mut self.mute_keys, "mute switching keys")
            .on_hover_text(
                "The keys steer either way. Muted they stop here; unmuted they also go on                  downstream and sound.",
            )
            .changed();
        changed
    }

    /// Displays the trigger key configuration for each output socket row.
    #[cfg(feature = "ui")]
    fn output_control(&mut self, ui: &mut egui::Ui, port: u8, _cx: &mut NodeUi<'_>) -> bool {
        let toggling = self.mode == KeySwitchMode::Toggle;
        let Some(key) = self.keys.get_mut(usize::from(port)) else {
            return false;
        };
        let live = !toggling || port == 0;
        let out = ui.add_enabled_ui(live, |ui| key_control(ui, "", key));
        if !live {
            out.response
                .on_hover_text("Toggle moves on from one key — the first output's");
        }
        out.inner
    }

    #[cfg(feature = "ui")]
    fn add_output_label(&self) -> Option<&'static str> {
        (self.keys.len() < MAX_WAYS).then_some("another destination")
    }

    #[cfg(feature = "ui")]
    fn add_output(&mut self) {
        // Default new key to one semitone above the last configured key.
        let next = self.keys.last().map_or(24, |k| k.saturating_add(1));
        self.keys.push(next);
    }

    #[cfg(feature = "ui")]
    fn remove_output(&mut self, port: u8) -> u8 {
        let index = usize::from(port);
        if self.keys.len() <= 1 || index >= self.keys.len() {
            return 0;
        }
        self.keys.remove(index);
        1
    }
}

/// Combines gate condition with any upstream note gate via multiplication.
fn fold_upstream(cx: &mut ParamCx, condition: Reg) -> Result<Reg, CompileError> {
    let Some(upstream) = cx.upstream_note_gate(0) else {
        return Ok(condition);
    };
    let both = cx.alloc()?;
    cx.emit(Op::Math {
        out: both,
        a: condition,
        b: Operand::Reg(upstream),
        op: MathOp::Multiply,
    });
    Ok(both)
}

/// Default value (`true`) for `mute_keys` during deserialization.
fn muted() -> bool {
    true
}

#[cfg(feature = "ui")]
impl KeySwitch {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, KeySwitch)> {
        vec![(
            "Key MIDI Route",
            KeySwitch {
                mode: KeySwitchMode::Select,
                // Default keys at lower octave (C1, C#1).
                keys: vec![24, 25],
                mute_keys: true,
            },
        )]
    }
}
