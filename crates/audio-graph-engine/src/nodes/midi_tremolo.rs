use serde::{Deserialize, Serialize};

use crate::compile::{CompileError, ParamCx};
#[cfg(feature = "ui")]
use crate::nodes::widgets::NodeUi;
use crate::nodes::{Node, NoteTremolo, TremoloKeys};
use crate::port::{Port, PortType};

/// Cuts a note stream into repeated notes while a key switch says so: each
/// held note struck again on every step, and let go for the cut part of it.
///
/// What cutting a part up by hand in a piano roll would give, for release-cut
/// playing, without the cutting. The notes themselves are not moved: a note
/// sounds from where it is struck (or from the next step, if it was struck in
/// a cut) and ends where the player let it go, however far into a step that
/// is. See [`NoteOp::Tremolo`][crate::ir::NoteOp::Tremolo].
///
/// Every strike of a note carries the note's own id and velocity, so a plugin
/// hears one note played again rather than a stream of new ones, and the DAW
/// hears the note end once, after the last of them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MidiTremolo {
    #[serde(flatten)]
    pub keys: TremoloKeys,
    /// Whether the keys that steer are taken out of the stream on the way
    /// out, as a router's are, and for a router's reason.
    pub mute_keys: bool,
}

impl Node for MidiTremolo {
    fn title(&self) -> String {
        "MIDI Tremolo".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![Port::new("notes", PortType::Note)]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new("out", PortType::Note)]
    }

    fn compile(&self, cx: &mut ParamCx) -> Result<(), CompileError> {
        let _ = cx;
        Ok(())
    }

    fn note_tremolo(&self, port: u8) -> Option<NoteTremolo> {
        (port == 0).then(|| NoteTremolo {
            input: 0,
            spec: self.keys.spec(),
            mute: self.mute_keys,
        })
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        let mut changed = self.keys.controls(ui);
        changed |= ui
            .checkbox(&mut self.mute_keys, "mute switching keys")
            .on_hover_text(
                "The keys steer either way. Muted they stop here; unmuted they also go on \
                 downstream and sound.",
            )
            .changed();
        changed
    }
}

#[cfg(feature = "ui")]
impl MidiTremolo {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, MidiTremolo)> {
        vec![(
            "MIDI Tremolo",
            MidiTremolo {
                keys: TremoloKeys::defaults(),
                mute_keys: true,
            },
        )]
    }
}
