use serde::{Deserialize, Serialize};

use crate::ir::{MAX_TREMOLO_ROWS, TremoloSpec};
#[cfg(feature = "ui")]
use crate::nodes::widgets::{beats_control, combo, key_control, ratio_control};
use crate::nodes::{Beats, KeySwitchMode, Ratio};

/// The modes a tremolo offers: held, or latched. `Toggle` walks through the
/// destinations of a router, and a tremolo has none to walk through.
#[cfg(feature = "ui")]
const MODES: [KeySwitchMode; 2] = [KeySwitchMode::Hold, KeySwitchMode::Select];

/// One key that starts a tremolo, and how long each of its steps is.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TremoloRow {
    pub key: u8,
    pub step: Beats,
}

/// What the MIDI and the audio tremolo share: which keys start them, how fast
/// each cuts, and how much of a step sounds.
///
/// Shared so the two can be set alike and cut alike. Both run the same clock
/// over the same keys; see [`TremoloSpec`].
///
/// The grid starts at the moment a key is struck, not on the host's bar
/// lines. That makes the phase something the player sets by where the key
/// goes, and keeps the tremolo meaningful with the transport stopped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TremoloKeys {
    /// `Hold` cuts while a key is down; `Select` starts at a key and runs
    /// until `stop_key` or another row's key. `Toggle` reads as `Select`.
    pub mode: KeySwitchMode,
    /// One per key. Empty is a node the user has not finished building; no
    /// key starts it.
    pub rows: Vec<TremoloRow>,
    /// The key that stops a latched tremolo, and does nothing while held.
    ///
    /// A key of its own rather than the running key struck a second time:
    /// players put a key switch at the head of every phrase as a safeguard,
    /// and a second strike that stopped the tremolo would undo exactly that.
    pub stop_key: u8,
    /// How a step is shared between sounding and cut, in that order.
    pub share: Ratio,
}

impl TremoloKeys {
    pub(crate) fn spec(&self) -> TremoloSpec {
        let rows = self.rows.len().min(MAX_TREMOLO_ROWS);
        let mut keys = [0; MAX_TREMOLO_ROWS];
        let mut steps = [0.0; MAX_TREMOLO_ROWS];
        for (i, row) in self.rows[..rows].iter().enumerate() {
            keys[i] = row.key;
            steps[i] = row.step.value();
        }
        let latch = self.mode != KeySwitchMode::Hold;
        TremoloSpec {
            latch,
            keys,
            steps,
            rows: rows as u8,
            stop: latch.then_some(self.stop_key),
            share: self.share.share(),
        }
    }

    #[cfg(feature = "ui")]
    pub(crate) fn defaults() -> TremoloKeys {
        TremoloKeys {
            mode: KeySwitchMode::Hold,
            // Below where most parts are played, like a router's; a 16th
            // and a 32nd, the two tremolos most often written.
            rows: vec![
                TremoloRow {
                    key: 24,
                    step: Beats::new(1, 4),
                },
                TremoloRow {
                    key: 25,
                    step: Beats::new(1, 8),
                },
            ],
            stop_key: 23,
            share: Ratio::EVEN,
        }
    }

    /// The mode, the share, one row per key, and the stop key where it
    /// applies.
    #[cfg(feature = "ui")]
    pub(crate) fn controls(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = combo(ui, "mode", &mut self.mode, &MODES, KeySwitchMode::label);
        ui.horizontal(|ui| {
            ui.label("sound : cut");
            changed |= ratio_control(ui, &mut self.share);
        });
        let removable = self.rows.len() > 1;
        let mut remove = None;
        for (index, row) in self.rows.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                changed |= key_control(ui, "", &mut row.key);
                changed |= beats_control(ui, ("step", index), &mut row.step);
                // The last row keeps its button greyed rather than losing
                // it, as a router's last way does.
                if ui
                    .add_enabled(removable, egui::Button::new("x").small())
                    .on_hover_text("remove this key")
                    .clicked()
                {
                    remove = Some(index);
                }
            });
        }
        if let Some(index) = remove {
            self.rows.remove(index);
            changed = true;
        }
        if self.rows.len() < MAX_TREMOLO_ROWS
            && ui.small_button("+").on_hover_text("another key").clicked()
        {
            // A semitone up from the last one, with the same step: a bank of
            // key switches is a run of adjacent keys more often than not.
            let next = self.rows.last().map_or(
                TremoloRow {
                    key: 24,
                    step: Beats::new(1, 4),
                },
                |last| TremoloRow {
                    key: last.key.saturating_add(1).min(127),
                    step: last.step,
                },
            );
            self.rows.push(next);
            changed = true;
        }
        // Greyed rather than hidden while held, so switching modes does not
        // look like it lost the key.
        let latched = self.mode != KeySwitchMode::Hold;
        let stop = ui.add_enabled_ui(latched, |ui| key_control(ui, "stop", &mut self.stop_key));
        if !latched {
            stop.response
                .on_hover_text("a held tremolo stops when its key is let go");
        }
        changed | stop.inner
    }
}

#[cfg(all(test, feature = "ui"))]
mod tests {
    use super::*;
    use crate::nodes::widgets::NODE_WIDTH;

    /// A row at its widest — key, name, a triplet 64th over 64 and its
    /// remove button — still fits across a node.
    #[test]
    fn a_tremolo_row_fits_a_node() {
        let mut keys = TremoloKeys::defaults();
        keys.mode = KeySwitchMode::Select;
        keys.rows[0] = TremoloRow {
            key: 127,
            step: Beats {
                num: Beats::MAX_NUM,
                den: 64,
                triplet: true,
            },
        };
        let ctx = egui::Context::default();
        let mut width = f32::INFINITY;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1000.0, 1000.0),
            )),
            ..Default::default()
        };
        let output = ctx.run_ui(input, |ui| {
            // The dropdown takes whatever width it is given, so the question
            // is whether anything spills past a node-wide `Ui`.
            width = ui
                .vertical(|ui| {
                    ui.set_max_width(NODE_WIDTH);
                    keys.controls(ui);
                    ui.min_rect().width()
                })
                .inner;
        });
        output.drop_without_applying_deltas();
        assert!(width <= NODE_WIDTH + 0.5, "{width} wide");
    }
}
