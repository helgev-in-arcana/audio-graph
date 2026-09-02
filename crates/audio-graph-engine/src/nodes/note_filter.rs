use serde::{Deserialize, Serialize};

use crate::ir::{ALL_CHANNELS, ALL_CONTROLLERS};
use crate::nodes::Node;
#[cfg(feature = "ui")]
use crate::nodes::widgets::NodeUi;
use crate::port::{Port, PortType};

/// Which way a list reads: the things named, or everything but them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FilterMode {
    /// Only what is named gets through.
    #[default]
    Keep,
    /// Everything except what is named.
    Drop,
}

/// Narrows a note stream to particular MIDI channels and controllers.
///
/// The counterpart of [`NoteMute`][crate::NoteMute], which names keys. A
/// controller is not a key and cannot be selected the same way, so it gets its
/// own node rather than more rows on that one: a patch that splits a
/// controller keyboard by channel and a patch that keeps the sustain pedal out
/// of a layer are different jobs.
///
/// An empty list is not a filter that blocks everything — that node would be a
/// disconnected wire, and there is already a way to draw one. `Keep` with an
/// empty list passes everything, so a freshly placed node is inert until the
/// user says what they meant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NoteFilter {
    /// MIDI channels named, `0..16`.
    pub channels: Vec<u8>,
    pub channel_mode: FilterMode,
    /// Controller numbers named, `0..128`.
    pub controllers: Vec<u8>,
    pub controller_mode: FilterMode,
}

impl Default for NoteFilter {
    fn default() -> Self {
        NoteFilter {
            channels: Vec::new(),
            channel_mode: FilterMode::Keep,
            controllers: Vec::new(),
            controller_mode: FilterMode::Keep,
        }
    }
}

/// Turns a list of numbers into the mask of what passes.
///
/// `Keep` with nothing named passes everything, so an untouched node is inert.
fn mask<const N: u32>(named: &[u8], mode: FilterMode) -> u128 {
    let all = if N >= 128 {
        u128::MAX
    } else {
        (1u128 << N) - 1
    };
    let listed = named
        .iter()
        .filter(|&&n| u32::from(n) < N)
        .fold(0u128, |m, &n| m | (1u128 << n));
    match mode {
        FilterMode::Keep if listed == 0 => all,
        FilterMode::Keep => listed,
        FilterMode::Drop => all & !listed,
    }
}

impl Node for NoteFilter {
    fn title(&self) -> String {
        "MIDI Filter".into()
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

    fn note_channels(&self, port: u8) -> u16 {
        if port != 0 {
            return ALL_CHANNELS;
        }
        mask::<16>(&self.channels, self.channel_mode) as u16
    }

    fn note_controllers(&self, port: u8) -> u128 {
        if port != 0 {
            return ALL_CONTROLLERS;
        }
        mask::<128>(&self.controllers, self.controller_mode)
    }

    #[cfg(feature = "ui")]
    fn controls(&mut self, ui: &mut egui::Ui, _cx: &mut NodeUi<'_>) -> bool {
        let mut changed = false;
        changed |= list(
            ui,
            "channels",
            &mut self.channels,
            &mut self.channel_mode,
            15,
        );
        ui.separator();
        changed |= list(
            ui,
            "CCs",
            &mut self.controllers,
            &mut self.controller_mode,
            127,
        );
        changed
    }
}

/// One editable list of numbers with its keep/drop toggle.
#[cfg(feature = "ui")]
fn list(
    ui: &mut egui::Ui,
    label: &str,
    values: &mut Vec<u8>,
    mode: &mut FilterMode,
    max: u8,
) -> bool {
    /// Past this the node is a wall of rows and a second filter reads better.
    const MAX_ROWS: usize = 8;

    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        let keep = *mode == FilterMode::Keep;
        if ui
            .selectable_label(keep, if keep { "keep" } else { "drop" })
            .on_hover_text(if keep {
                "only these get through"
            } else {
                "everything except these"
            })
            .clicked()
        {
            *mode = if keep {
                FilterMode::Drop
            } else {
                FilterMode::Keep
            };
            changed = true;
        }
    });

    let mut remove = None;
    for (index, value) in values.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            changed |= ui.add(egui::DragValue::new(value).range(0..=max)).changed();
            if ui.small_button("−").clicked() {
                remove = Some(index);
            }
        });
    }
    if let Some(index) = remove {
        values.remove(index);
        changed = true;
    }
    if values.len() < MAX_ROWS && ui.button("another").clicked() {
        let next = values.last().map_or(0, |v| v.saturating_add(1).min(max));
        values.push(next);
        changed = true;
    }
    if values.is_empty() {
        ui.weak(match mode {
            FilterMode::Keep => "everything passes",
            FilterMode::Drop => "nothing is dropped",
        });
    }
    changed
}

#[cfg(feature = "ui")]
impl NoteFilter {
    pub(crate) fn catalogue_defaults() -> Vec<(&'static str, NoteFilter)> {
        vec![("MIDI Filter", NoteFilter::default())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An untouched node has to be inert. `Keep` with an empty list meaning
    /// "nothing passes" would make a freshly placed node silently cut the
    /// stream, which reads as a bug in the graph rather than an empty list.
    #[test]
    fn an_empty_keep_list_passes_everything() {
        let node = NoteFilter::default();
        assert_eq!(node.note_channels(0), ALL_CHANNELS);
        assert_eq!(node.note_controllers(0), ALL_CONTROLLERS);
    }

    #[test]
    fn keep_names_what_survives_and_drop_names_what_does_not() {
        let keep = NoteFilter {
            channels: vec![0, 9],
            channel_mode: FilterMode::Keep,
            ..NoteFilter::default()
        };
        assert_eq!(keep.note_channels(0), 0b10_0000_0001);

        let drop = NoteFilter {
            channels: vec![0, 9],
            channel_mode: FilterMode::Drop,
            ..NoteFilter::default()
        };
        assert_eq!(drop.note_channels(0), !0b10_0000_0001u16);
    }

    /// The sustain pedal on its own, which is the case the node exists for.
    #[test]
    fn one_controller_can_be_kept_alone() {
        let node = NoteFilter {
            controllers: vec![64],
            controller_mode: FilterMode::Keep,
            ..NoteFilter::default()
        };
        assert_eq!(node.note_controllers(0), 1u128 << 64);
    }
}
