use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::NodeUi;
use crate::port::{Port, PortType};

/// Turns a parameter signal into control change events on a note stream.
///
/// Not a precision route, and not meant as one. Both plugin formats carry
/// parameters as `double`, and a graph that wants to move a sub-plugin's
/// parameter should wire its parameter port directly — that path is exact and
/// this one quantizes to seven bits at the far end.
///
/// What this is for is *meaning*. Sending CC64 is pressing the sustain pedal,
/// which is not the same act as turning a knob: it reaches behaviour a plugin
/// never exposed as a parameter, and every synth understands it without being
/// told. That is worth a lossy path.
///
/// The value is taken as `0..=1` and clamped. Scaling belongs upstream, in a
/// Range Map, rather than in two more fields here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamToCc {
    /// MIDI channel, `0..16`.
    pub channel: u8,
    /// Controller number, `0..128`.
    pub cc: u8,
}

impl Default for ParamToCc {
    fn default() -> Self {
        // CC64 is the sustain pedal, which is the case this node exists for.
        ParamToCc { channel: 0, cc: 64 }
    }
}

/// The input socket carrying the value, and the one carrying the stream it is
/// added to.
pub(crate) const VALUE_PORT: u8 = 0;
pub(crate) const NOTES_PORT: u8 = 1;

impl Node for ParamToCc {
    fn title(&self) -> String {
        "Param → CC".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![Port::param("value"), Port::new("notes", PortType::Note)]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new("out", PortType::Note)]
    }

    /// The stream this node adds to passes through it, so a chain of these
    /// builds up one stream rather than needing a merge.
    fn note_passthrough(&self, port: u8) -> Option<u8> {
        (port == 0).then_some(NOTES_PORT)
    }

    fn note_emits(&self, port: u8) -> Option<(u8, u8)> {
        (port == 0).then_some((self.channel & 0x0f, self.cc & 0x7f))
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        // The note half reads the value off a lane, the way a mix gain does:
        // the two halves run at different rates and a register is the param
        // half's alone.
        let value = cx.input_or_zero(VALUE_PORT)?;
        cx.drive_audio(VALUE_PORT, value)
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label("CC");
            changed |= ui
                .add(egui::DragValue::new(&mut self.cc).range(0..=127))
                .on_hover_text(match self.cc {
                    1 => "modulation wheel",
                    7 => "channel volume",
                    11 => "expression",
                    64 => "sustain pedal",
                    _ => "controller number",
                })
                .changed();
            ui.label("ch");
            changed |= ui
                .add(egui::DragValue::new(&mut self.channel).range(0..=15))
                .changed();
        });
        changed
    }
}

#[cfg(feature = "ui")]
impl ParamToCc {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, ParamToCc)> {
        vec![("Param → CC", ParamToCc::default())]
    }
}
