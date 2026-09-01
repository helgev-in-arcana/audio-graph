use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
use crate::ir::Op;
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::NodeUi;
use crate::port::{Port, PortType};

/// Reads a controller off a note stream as a parameter signal.
///
/// The way into the graph for a mod wheel, an expression pedal, a breath
/// controller — anything the player moves that is not a note.
///
/// It reads the stream *as routed*, so a `MIDI Filter` or a `MIDI Gate`
/// upstream changes what this sees. That is the point of it taking a note
/// input rather than reading whatever the DAW happened to send: the path is on
/// the canvas.
///
/// The value follows one sub-block behind the events, which is what a
/// parameter signal's resolution means — it carries the value in effect at the
/// sub-block boundary, being the last message before it. Events reaching a
/// sub-plugin keep their own sample offsets and are not delayed by this.
///
/// A controller keeps its position between messages, so a block with no CC in
/// it holds the last value rather than falling back to `initial`. `initial` is
/// only what the controller reads as before it has ever been moved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CcIn {
    /// Controller number, `0..128`.
    pub cc: u8,
    /// MIDI channel, or `None` for any.
    ///
    /// Any is the default because a controller keyboard sends on whichever
    /// channel it is set to, and a patch that has not thought about channels
    /// should still work.
    pub channel: Option<u8>,
    /// What the value reads as until the controller is first moved.
    pub initial: f64,
}

impl Default for CcIn {
    fn default() -> Self {
        // CC1 is the modulation wheel, which is the one everybody has.
        CcIn {
            cc: 1,
            channel: None,
            initial: 0.0,
        }
    }
}

impl Node for CcIn {
    fn title(&self) -> String {
        "CC In".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![Port::new("notes", PortType::Note)]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::param("out")]
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let out = cx.alloc()?;
        let Some(buf) = cx.note_source_of(0) else {
            // Nothing wired to the notes port. The controller has never moved,
            // so it reads as its starting value — the same answer it gives
            // before the first message on a stream that is connected.
            cx.emit(Op::Const {
                out,
                value: self.initial,
            });
            cx.bind_output(0, out);
            return Ok(());
        };
        // A latch, so the value survives a recompile: the wheel is where the
        // player left it, and a program swap happens on every drag of every
        // unrelated control.
        let state = cx.latch()?;
        cx.emit(Op::NoteCc {
            out,
            buf,
            state,
            channel: self.channel.map_or(-1, i16::from),
            cc: self.cc & 0x7f,
            initial: self.initial,
        });
        cx.bind_output(0, out);
        Ok(())
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
                    2 => "breath",
                    7 => "channel volume",
                    11 => "expression",
                    64 => "sustain pedal",
                    _ => "controller number",
                })
                .changed();

            let mut any = self.channel.is_none();
            if ui
                .selectable_label(any, "any ch")
                .on_hover_text("read this controller on every channel")
                .clicked()
            {
                any = !any;
                self.channel = if any { None } else { Some(0) };
                changed = true;
            }
            if let Some(channel) = &mut self.channel {
                changed |= ui
                    .add(egui::DragValue::new(channel).range(0..=15))
                    .changed();
            }
        });
        changed
    }
}

#[cfg(feature = "ui")]
impl CcIn {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, CcIn)> {
        vec![("CC In", CcIn::default())]
    }
}
