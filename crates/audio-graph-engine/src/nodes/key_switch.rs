use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::ir::{MathOp, Op, Operand, Reg};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, combo, key_control};
use crate::port::{Port, PortType};

/// What a key switch does with the key it watches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeySwitchMode {
    /// Notes pass while the key is held, and stop when it is let go.
    Hold,
    /// Each press of the key moves the stream to the other output.
    Toggle,
}

impl KeySwitchMode {
    pub const ALL: [KeySwitchMode; 2] = [KeySwitchMode::Hold, KeySwitchMode::Toggle];

    pub fn label(self) -> &'static str {
        match self {
            KeySwitchMode::Hold => "Hold",
            KeySwitchMode::Toggle => "Toggle",
        }
    }
}

/// Route notes by a key switch: a key played to steer the rest, rather than to
/// sound.
///
/// The two modes are the two ways players use one. `Hold` is momentary — the
/// layer speaks while the key is down, which is what a foot-switch does with
/// hands instead. `Toggle` is latching: the key is tapped once and the stream
/// moves to the other output, and stays there until it is tapped again.
///
/// Which way a `Toggle` is thrown survives a recompile, so editing an
/// unrelated node does not quietly move the routing back.
///
/// The switch key's own note still reaches whatever is downstream. Taking it
/// out of the stream would mean the route carrying a key to filter as well as
/// a gate, and a key switch on a range the patch does not play is the ordinary
/// case; the exception can be handled when someone hits it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeySwitch {
    pub key: u8,
    pub mode: KeySwitchMode,
}

impl Node for KeySwitch {
    fn title(&self) -> String {
        "Key switch".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![Port::new("notes", PortType::Note)]
    }

    /// One output while holding, two while toggling — the second is where the
    /// notes go when the switch is thrown.
    fn output_ports(&self) -> Vec<Port> {
        match self.mode {
            KeySwitchMode::Hold => vec![Port::new("out", PortType::Note)],
            KeySwitchMode::Toggle => vec![
                Port::new("a", PortType::Note),
                Port::new("b", PortType::Note),
            ],
        }
    }

    /// Both outputs carry the same stream; which of them is open is the whole
    /// of what this node decides.
    fn note_passthrough(&self, port: u8) -> Option<u8> {
        (port < self.output_ports().len() as u8).then_some(0)
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        match self.mode {
            KeySwitchMode::Hold => {
                let held = cx.alloc()?;
                cx.emit(Op::KeyHeld {
                    out: held,
                    key: self.key,
                });
                let condition = fold_upstream(cx, 0, held)?;
                cx.bind_note_gate(0, condition)
            }
            KeySwitchMode::Toggle => {
                let state = cx.latch()?;
                cx.emit(Op::KeyToggle {
                    state,
                    key: self.key,
                    off: 0.0,
                    on: 1.0,
                });
                let thrown = cx.alloc()?;
                cx.emit(Op::Latch {
                    out: thrown,
                    state,
                    initial: 0.0,
                });
                // `a` is open while the switch is *not* thrown, so an untouched
                // key switch leaves the notes where they were plugged in.
                let on_a = cx.alloc()?;
                cx.emit(Op::Select {
                    out: on_a,
                    control: thrown,
                    threshold: 0.5,
                    low: Operand::Value(1.0),
                    high: Operand::Value(0.0),
                });
                let a = fold_upstream(cx, 0, on_a)?;
                cx.bind_note_gate(0, a)?;
                let b = fold_upstream(cx, 0, thrown)?;
                cx.bind_note_gate(1, b)
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
        changed |= key_control(ui, "key", &mut self.key);
        changed
    }
}

/// Multiply a gate condition by whatever gate is already on the chain, so
/// gates in series pass notes only when every one of them is open.
fn fold_upstream(cx: &mut ParamCx, port: u8, condition: Reg) -> Result<Reg, CompileError> {
    let Some(upstream) = cx.upstream_note_gate(port) else {
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
            "Key switch",
            KeySwitch {
                // Well below where most parts are played.
                key: 24,
                mode: KeySwitchMode::Hold,
            },
        )]
    }
}
