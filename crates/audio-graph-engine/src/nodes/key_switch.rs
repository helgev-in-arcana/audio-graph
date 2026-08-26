use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::ir::{MathOp, Op, Operand, Reg};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, combo, key_control};
use crate::port::{Port, PortType};

/// How many destinations one key switch may have.
///
/// The same reasoning as a `Mix`'s eight inputs: past this it is a wall of
/// sockets, and a second key switch reads better than a taller one.
#[cfg(feature = "ui")]
const MAX_WAYS: usize = 8;

/// What a key switch does with the keys it watches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeySwitchMode {
    /// Each output speaks while its own key is held. Several at once is
    /// allowed and is how a layer is added by holding a key down.
    Hold,
    /// The last key struck leaves its output open and shuts the others. A
    /// bank of switches, latching.
    Select,
    /// One key — the first output's — moving the stream on to the next output
    /// each time it is struck, and round to the first again at the end.
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

/// Route notes by a key switch: keys played to steer the rest, rather than to
/// sound.
///
/// One output per destination, each with the key that opens it on its own row,
/// because a key belongs to the output it steers — a list of keys somewhere
/// else on the node is a thing to match up by counting.
///
/// The three modes are the three ways players use one. `Hold` is momentary:
/// the layer speaks while the key is down, which is what a foot-switch does
/// with hands instead. `Select` is a latching bank — tap a key, that
/// destination stays chosen. `Toggle` is the one-key version of `Select`, for
/// when there is no room on the keyboard for a key per way.
///
/// Where a `Select` or a `Toggle` stands survives a recompile, so editing an
/// unrelated node does not quietly move the routing back.
///
/// The switch keys are taken out of the stream by default, because a key
/// played to steer is not a key played to sound and a sampler handed one will
/// answer with whatever is mapped there. `pass_keys` puts them back for the
/// patch that wants the switch audible — a key that both selects a layer and
/// plays it is a real technique, just not the common one.
///
/// Both halves of a muted key go, note-on and note-off alike. There is no
/// sounding voice waiting for the release, so dropping it hangs nothing; that
/// is the difference between this and a shut gate, which must let releases
/// through.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeySwitch {
    pub mode: KeySwitchMode,
    /// One key per output, in socket order. Empty is a node the user has not
    /// finished building; it routes nothing.
    pub keys: Vec<u8>,
    /// Whether the switch keys go on downstream. Default off, and `default`
    /// rather than a required field so patches saved before it read as off —
    /// which is also the answer they would have wanted.
    #[serde(default)]
    pub pass_keys: bool,
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
                // The last way is not offered a remove button: a switch with
                // nowhere to send anything is not a switch.
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

    /// Every output carries the same stream; which of them are open is the
    /// whole of what this node decides.
    fn note_passthrough(&self, port: u8) -> Option<u8> {
        (usize::from(port) < self.keys.len()).then_some(0)
    }

    /// The keys this switch answers to, swallowed on the way out unless the
    /// user asked for them. Keys past 127 cannot be set from the UI and would
    /// not fit the mask, so they are simply not counted.
    fn note_mute(&self, port: u8) -> u128 {
        if self.pass_keys || usize::from(port) >= self.keys.len() {
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
                // One latch holding which way is chosen, which is what makes
                // the ways exclusive — and what survives a program swap.
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
                        // Untouched, the first way is the open one, so notes
                        // go where they were plugged in until a key says
                        // otherwise.
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
        changed |= ui
            .checkbox(&mut self.pass_keys, "pass keys on")
            .on_hover_text("Send the switch keys downstream as well as steering with them")
            .changed();
        changed
    }

    /// The key that opens this output, on the output's own row.
    ///
    /// In `Toggle` only the first row's key does anything — one key is the
    /// point of that mode — so the rest are drawn greyed rather than hidden,
    /// which would make switching modes look like it lost them.
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
        (self.keys.len() < MAX_WAYS).then_some("+ way")
    }

    #[cfg(feature = "ui")]
    fn add_output(&mut self) {
        // A semitone up from the last one: a bank of key switches is a run of
        // adjacent keys far more often than it is not.
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

/// Multiply a gate condition by whatever gate is already on the chain, so
/// gates in series pass notes only when every one of them is open.
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

#[cfg(feature = "ui")]
impl KeySwitch {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, KeySwitch)> {
        vec![(
            "Key MIDI Route",
            KeySwitch {
                mode: KeySwitchMode::Select,
                // Well below where most parts are played.
                keys: vec![24, 25],
                pass_keys: false,
            },
        )]
    }
}
