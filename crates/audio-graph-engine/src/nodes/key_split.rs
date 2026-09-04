use serde::{Deserialize, Serialize};

use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::{NodeUi, key_name};
use crate::port::{Port, PortType};

/// How many bands one node may cut a keyboard into.
///
/// The same reasoning as a `Mix`'s eight inputs: past this it is a wall of
/// sockets, and a second split fed from one of these bands reads better than a
/// taller node — the bands nest, so nothing is lost by splitting twice.
#[cfg(feature = "ui")]
const MAX_BANDS: usize = 8;

/// The highest key there is, and so the top of the first output.
const TOP_KEY: u8 = 127;

/// Cuts a note stream into key bands, one output per band.
///
/// Every output carries the same stream with the keys outside its own band
/// taken out, which is what makes a keyboard split one node rather than a key
/// mute per layer with the same numbers typed into each of them twice.
///
/// The bands meet and do not overlap. The first runs down from 127, the last
/// runs down to 0, and each of the others from its own key to one below the
/// next, so every key on the keyboard leaves exactly one output and no key
/// leaves two. The first output has no key of its own for that reason: 127 is
/// not a setting, it is where the keyboard ends.
///
/// One band is allowed, and is the whole keyboard on one socket. The count
/// falls out of the splits with no floor under it — `n` splits are `n + 1`
/// bands at every `n`, including none — and a node held at two bands would be
/// a rule to state and a case to check for the sake of forbidding a node that
/// costs nothing: with nothing to drop, the socket carries the buffer that
/// came in rather than a copy of it.
///
/// Only keys are divided. An event with no key of its own — a control change,
/// a pitch bend — carries the whole channel and belongs to no band, so it goes
/// out of every output. A player leaning on the sustain pedal means it for
/// whatever they are playing, and a pedal that reached only the band the last
/// note fell in would be a foot-switch for the bass and nothing else.
///
/// Both halves of a key outside a band go, note-on and note-off alike, so
/// nothing hangs there waiting for a release.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeySplit {
    /// The highest key of every output but the first, in socket order.
    ///
    /// `n` of them make `n + 1` outputs. Held descending by the control that
    /// edits them, because an entry above the one before it would name a band
    /// no key can fall in — an output wired to a synth that never speaks.
    pub splits: Vec<u8>,
}

impl KeySplit {
    /// The keys output `port` takes, `(low, high)` inclusive, or `None` for a
    /// socket this node does not have.
    ///
    /// `low` above `high` is a band nothing falls in, which is what a patch
    /// edited by hand into a list that does not descend asks for.
    pub fn band(&self, port: u8) -> Option<(u8, u8)> {
        let index = usize::from(port);
        if index > self.splits.len() {
            return None;
        }
        let high = if index == 0 {
            TOP_KEY
        } else {
            self.splits[index - 1]
        };
        let low = self
            .splits
            .get(index)
            .map_or(0, |&next| next.saturating_add(1));
        Some((low, high))
    }
}

impl Node for KeySplit {
    fn title(&self) -> String {
        "MIDI Split".into()
    }

    fn input_ports(&self) -> Vec<Port> {
        vec![Port::new("notes", PortType::Note)]
    }

    fn output_ports(&self) -> Vec<Port> {
        (0..=self.splits.len())
            .map(|i| {
                let port = Port::new(format!("out {}", i + 1), PortType::Note);
                // Held rather than dropped on the sole band, which has no
                // split under it to take away.
                #[cfg(feature = "ui")]
                let port = port.removable(!self.splits.is_empty());
                port
            })
            .collect()
    }

    /// Every output is the input with a different set of keys missing.
    fn note_passthrough(&self, port: u8) -> Option<u8> {
        (usize::from(port) <= self.splits.len()).then_some(0)
    }

    /// Everything outside this output's band. The mask is exactly as wide as
    /// the 128 keys, so the band's own bits inverted are the rest of them.
    fn note_mute(&self, port: u8) -> u128 {
        let Some((low, high)) = self.band(port) else {
            return 0;
        };
        if low > high {
            return u128::MAX;
        }
        let width = u32::from(high - low) + 1;
        let band = if width >= 128 {
            u128::MAX
        } else {
            ((1u128 << width) - 1) << low
        };
        !band
    }

    /// The key this output starts at, on the output's own row.
    ///
    /// The first row shows where the keyboard ends instead, greyed: a row with
    /// nothing on it would read as a control that failed to draw, and the
    /// number is worth seeing next to the ones under it.
    #[cfg(feature = "ui")]
    fn output_control(&mut self, ui: &mut egui::Ui, port: u8, _cx: &mut NodeUi<'_>) -> bool {
        let Some(index) = usize::from(port).checked_sub(1) else {
            ui.horizontal(|ui| {
                ui.weak(TOP_KEY.to_string());
                ui.weak(key_name(TOP_KEY));
            })
            .response
            .on_hover_text("the first band runs down from the top of the keyboard");
            return false;
        };
        // The neighbours are read out before the split itself is borrowed, and
        // they are what hold the list descending: a band may be dragged up to
        // one below the band above it and down to one above the band below.
        // A patch is a file and may hold a list no edit could produce, so the
        // floor is held at the ceiling rather than allowed past it: touching
        // the control on such a patch is what puts the band back in order.
        let ceiling = if index == 0 {
            TOP_KEY
        } else {
            self.splits[index - 1]
        }
        .saturating_sub(1);
        let floor = self
            .splits
            .get(index + 1)
            .map_or(0, |&next| next.saturating_add(1))
            .min(ceiling);
        let Some(key) = self.splits.get_mut(index) else {
            return false;
        };

        let mut changed = false;
        ui.horizontal(|ui| {
            let mut value = i32::from(*key);
            if ui
                .add(egui::DragValue::new(&mut value).range(i32::from(floor)..=i32::from(ceiling)))
                .on_hover_text("the highest key of this band; it runs down to one above the next")
                .changed()
            {
                *key = value.clamp(i32::from(floor), i32::from(ceiling)) as u8;
                changed = true;
            }
            // The number is what a DAW agrees with and the name is what the
            // split is chosen by ear as, so both are shown — see `key_name`.
            ui.weak(key_name(*key));
        });
        changed
    }

    /// Offered until the bands are as many as a node holds, and until the
    /// bottom band has no room left under it to cut one off.
    #[cfg(feature = "ui")]
    fn add_output_label(&self) -> Option<&'static str> {
        let room = self.splits.last().is_none_or(|&lowest| lowest > 0);
        (self.splits.len() + 1 < MAX_BANDS && room).then_some("another band")
    }

    #[cfg(feature = "ui")]
    fn add_output(&mut self) {
        match self.splits.last() {
            // An octave below the last split, which is where a hand lands more
            // often than anywhere else a default could pick.
            Some(&lowest) if lowest > 0 => self.splits.push(lowest.saturating_sub(12)),
            Some(_) => {}
            None => self.splits.push(60),
        }
    }

    /// Taking a band away merges it into the one above it — or, for the first
    /// band, into the one below. Either way one split goes, and the socket
    /// that keeps the merged band keeps whatever was wired to it. The last
    /// band merges into nothing and stays, since there is no split left to
    /// remove.
    #[cfg(feature = "ui")]
    fn remove_output(&mut self, port: u8) -> u8 {
        let index = usize::from(port).saturating_sub(1);
        if index >= self.splits.len() {
            return 0;
        }
        self.splits.remove(index);
        1
    }
}

#[cfg(feature = "ui")]
impl KeySplit {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, KeySplit)> {
        vec![(
            "MIDI Split",
            KeySplit {
                // Two bands parting at middle C, which is where a left hand and
                // a right hand part on a keyboard that is not being split by a
                // score.
                splits: vec![60],
            },
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bands cover the keyboard once each: every key leaves exactly one
    /// output. A gap would be a key that plays nothing and an overlap a key
    /// that plays twice as loud, and neither says so anywhere on screen.
    #[test]
    fn the_bands_meet_and_do_not_overlap() {
        let node = KeySplit {
            splits: vec![64, 32],
        };
        assert_eq!(node.band(0), Some((65, 127)));
        assert_eq!(node.band(1), Some((33, 64)));
        assert_eq!(node.band(2), Some((0, 32)));
        assert_eq!(node.band(3), None);

        for key in 0..128u8 {
            let taking = (0..3)
                .filter(|&port| node.note_mute(port) & (1u128 << key) == 0)
                .count();
            assert_eq!(taking, 1, "key {key} left {taking} outputs");
        }
    }

    /// A node with one band is a wire, and says so by muting nothing — the
    /// compiler then hands the socket the buffer that came in rather than
    /// spending a copy on a filter that drops nothing.
    #[test]
    fn a_single_band_passes_the_whole_keyboard() {
        let node = KeySplit { splits: Vec::new() };
        assert_eq!(node.band(0), Some((0, 127)));
        assert_eq!(node.note_mute(0), 0);
        assert_eq!(node.output_ports().len(), 1);
    }

    /// Bands come off one at a time down to the last, and the one that is left
    /// covers the keyboard: a split shrunk back to nothing is a node that
    /// passes what it is given, not one stuck two bands wide.
    #[test]
    fn the_bands_come_off_down_to_the_one_that_stays() {
        let mut node = KeySplit {
            splits: vec![64, 32],
        };
        assert_eq!(node.remove_output(2), 1, "the bottom band merges upwards");
        assert_eq!(node.splits, vec![64]);
        assert_eq!(node.remove_output(0), 1, "and the top one downwards");
        assert!(node.splits.is_empty());
        assert_eq!(node.remove_output(0), 0, "the last band stays");
        assert_eq!(node.note_mute(0), 0);
    }

    /// A list that does not descend names a band no key falls in, and that
    /// output is silent rather than carrying keys another output also has.
    #[test]
    fn a_band_that_no_key_falls_in_carries_nothing() {
        let node = KeySplit {
            splits: vec![32, 64],
        };
        assert_eq!(node.band(1), Some((65, 32)));
        assert_eq!(node.note_mute(1), u128::MAX);
    }
}
