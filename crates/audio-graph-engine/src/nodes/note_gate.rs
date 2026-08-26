use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::ir::{MathOp, Op, Operand};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::NodeUi;
use crate::port::{Port, PortType};

/// Pass notes on or hold them back, by where a control sits against a
/// threshold.
///
/// The note half's answer to [`Gate`][crate::Gate], and it works quite
/// differently underneath, because notes are not a buffer: this crate routes
/// the *name* of a note source and lets the wrapper turn it into events
/// (§14.10). So the gate is not something that happens to a stream — it is a
/// note on the route, carried to the plugin at the end of it and applied when
/// the events are handed over.
///
/// Shut means note-ons are held back and everything else still passes, so a
/// note that was already sounding gets its note-off and the gate can be thrown
/// mid-phrase without leaving a hung note behind. It also means a shut gate
/// does not cut a sounding note short; that is a release, not a mute, and
/// muting the audio is what [`Gate`][crate::Gate] is for.
///
/// An unwired control reads as zero, so a gate nobody has wired is shut — the
/// same way round as the audio gate, and for the same reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteGate {
    /// Notes pass at this value and above — or below it, with `invert`.
    pub threshold: f64,
    pub invert: bool,
}

impl Node for NoteGate {
    fn title(&self) -> String {
        "MIDI Gate".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![Port::new("notes", PortType::Note), Port::param("control")]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new("out", PortType::Note)]
    }

    fn note_passthrough(&self, port: u8) -> Option<u8> {
        (port == 0).then_some(0)
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let control = cx.input_or_zero(1)?;
        let (low, high) = if self.invert { (1.0, 0.0) } else { (0.0, 1.0) };
        let open = cx.alloc()?;
        cx.emit(Op::Select {
            out: open,
            control,
            threshold: self.threshold,
            low: Operand::Value(low),
            high: Operand::Value(high),
        });
        // Gates in series pass notes only when every one of them is open, and
        // multiplying the conditions is how that becomes one register — which
        // is what the audio half reads, so it never has to carry a list.
        let condition = match cx.upstream_note_gate(0) {
            Some(upstream) => {
                let both = cx.alloc()?;
                cx.emit(Op::Math {
                    out: both,
                    a: open,
                    b: Operand::Reg(upstream),
                    op: MathOp::Multiply,
                });
                both
            }
            None => open,
        };
        cx.bind_note_gate(0, condition)
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label(if self.invert { "shut at" } else { "open at" });
            changed |= ui
                .add(egui::DragValue::new(&mut self.threshold).speed(0.01))
                .changed();
            if ui
                .selectable_label(self.invert, "invert")
                .on_hover_text("pass while the control is below the threshold")
                .clicked()
            {
                self.invert = !self.invert;
                changed = true;
            }
        });
        changed
    }
}

#[cfg(feature = "ui")]
impl NoteGate {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, NoteGate)> {
        vec![(
            "MIDI Gate",
            NoteGate {
                threshold: 0.5,
                invert: false,
            },
        )]
    }
}
