use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::ir::{Op, Operand};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::NodeUi;
use crate::port::{Port, PortType};

/// Gates a MIDI note stream based on whether a parameter control value meets a threshold.
///
/// When closed, note-on events are blocked while note-off and other MIDI events
/// are allowed through to avoid stuck notes. Unconnected control inputs default to zero (closed).
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

    fn note_gated(&self, port: u8) -> bool {
        port == 0
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
        // No folding of the gates upstream: each one is its own filter on its
        // own copy of the stream, so two in series already pass only what both
        // let through.
        cx.bind_note_gate(0, open)
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
