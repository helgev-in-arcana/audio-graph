use serde::{Deserialize, Serialize};

use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, key_control};
use crate::port::{Port, PortType};

/// How many keys one node may swallow. Past this the node is a wall of rows,
/// and a second mute reads better than a taller one.
#[cfg(feature = "ui")]
const MAX_KEYS: usize = 8;

/// Takes named keys out of a note stream and passes the rest on: for keys that
/// steer something this graph knows nothing about, such as a controller's
/// bottom octave of buttons.
///
/// Both halves of a muted key go, note-on and note-off alike, so nothing hangs
/// waiting for a release.
///
/// The keys are a list on the node rather than a socket each: they name events
/// rather than carry anything.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteMute {
    /// The keys taken out. Empty passes everything.
    pub keys: Vec<u8>,
}

impl Node for NoteMute {
    fn title(&self) -> String {
        "MIDI Key Mute".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![Port::new("notes", PortType::Note)]
    }

    fn output_ports(&self) -> Vec<Port> {
        vec![Port::new("out", PortType::Note)]
    }

    fn note_passthrough(&self, port: u8) -> Option<u8> {
        (port == 0).then_some(0)
    }

    /// Keys past 127 do not fit the mask and are not counted.
    fn note_mute(&self, port: u8) -> u128 {
        if port != 0 {
            return 0;
        }
        self.keys
            .iter()
            .filter(|&&key| key < 128)
            .fold(0u128, |mask, &key| mask | (1u128 << key))
    }

    /// One row per key, each with the button that takes it off the list.
    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        let mut changed = false;
        let mut remove = None;
        for (index, key) in self.keys.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                changed |= key_control(ui, "", key);
                if ui
                    .small_button("−")
                    .on_hover_text("stop muting this key")
                    .clicked()
                {
                    remove = Some(index);
                }
            });
        }
        if let Some(index) = remove {
            self.keys.remove(index);
            changed = true;
        }
        if self.keys.len() < MAX_KEYS && ui.button("another key").clicked() {
            // A semitone up from the last: steering keys are usually adjacent.
            let next = self
                .keys
                .last()
                .map_or(24, |k| k.saturating_add(1).min(127));
            self.keys.push(next);
            changed = true;
        }
        if self.keys.is_empty() {
            ui.weak("no keys muted");
        }
        changed
    }
}

#[cfg(feature = "ui")]
impl NoteMute {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, NoteMute)> {
        vec![(
            "MIDI Key Mute",
            NoteMute {
                // Well below where most parts are played.
                keys: vec![24],
            },
        )]
    }
}
