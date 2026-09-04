use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
pub use crate::ir::Follow;
use crate::ir::Op;
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, combo};
use crate::port::{Port, PortType};

/// Follows the notes on a stream: how hard, whether any are down, which key,
/// or how many keys.
///
/// The readings that still mean something when polyphony is flattened. The
/// per-note controllers — pressure, tuning, brightness and the rest —
/// deliberately have no place here: reducing them to "the newest note wins" is
/// not musical, giving one number for a chord with nothing on the canvas to
/// say which note it came from. They wait for a per-voice engine to be given
/// to; these are monophonic by nature and keep working without one.
///
/// Three of them read `0..=1` and `Held Keys` reads a count, which is the one
/// place this node's output changes units with its setting. A count is what
/// the reading is: two keys are two, and rescaling them here would be a guess
/// at what full means, made in the node that has the least idea — a `Param
/// Map` downstream is told, and a `Param Select`'s thresholds want the number
/// as it stands.
///
/// The stream is an input rather than an assumption, so a filter or a gate
/// upstream changes what this follows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteFollow {
    pub what: Follow,
}

impl Node for NoteFollow {
    fn title(&self) -> String {
        self.what.label().into()
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
            // Nothing wired: no notes, so nothing is held and nothing was
            // played. Key track reads the middle for the same reason a pan
            // does — the absence of a note is not the bottom of the keyboard.
            cx.emit(Op::Const {
                out,
                value: match self.what {
                    Follow::KeyTrack => 0.5,
                    Follow::Velocity | Follow::Gate | Follow::HeldKeys => 0.0,
                },
            });
            cx.bind_output(0, out);
            return Ok(());
        };
        let state = cx.latch()?;
        cx.emit(Op::NoteFollow {
            out,
            buf,
            state,
            what: self.what,
        });
        cx.bind_output(0, out);
        Ok(())
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        combo(ui, "reads", &mut self.what, &Follow::ALL, Follow::label)
    }
}

#[cfg(feature = "ui")]
impl NoteFollow {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, NoteFollow)> {
        Follow::ALL
            .iter()
            .map(|&what| (what.label(), NoteFollow { what }))
            .collect()
    }
}
