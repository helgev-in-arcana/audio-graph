//! What a node needs to draw itself, and the controls more than one node uses.
//!
//! Only compiled with the `ui` feature, which only the wrapper turns on. The
//! CLI and the adapter link the same crate without egui in the tree at all —
//! `cargo tree -p host-cli` is the check.
//!
//! The feature exists so that a node's controls can sit in the node's own file
//! rather than in a second `match` in the editor crate. That was the last
//! place a node's implementation was still split in two, and putting the
//! canvas on one side of the line and the node on the other is what settles
//! where a new node's code goes: all of it here.
//!
//! What stays with the canvas is everything *about the canvas* — panning,
//! zooming, drawing links, the add-node menu, loading plugins. A node never
//! learns any of that; it is handed a `Ui` the right size and a [`NodeUi`] of
//! facts about the world outside the graph.

use crate::nodes::Rate;

/// Width of a node's body, in canvas units.
///
/// Here rather than in the editor because a node's controls are laid out
/// against it — a combo box that is wider than the node it sits in is the kind
/// of thing that only shows up once somebody adds a node.
pub const NODE_WIDTH: f32 = 232.0;

/// One sub-plugin instance, as the node holding it needs to draw it.
///
/// Filled in by the wrapper: this crate has no idea what is loaded, and does
/// not gain one by drawing it.
#[derive(Default, Clone)]
pub struct InstanceView {
    pub loaded: bool,
    pub name: String,
    pub editor_open: bool,
    /// `(id, name)` for every parameter, to fill a socket's dropdown.
    pub params: Vec<(u32, String)>,
}

/// Something a node's controls asked for that only the wrapper can do.
///
/// Opening a window may not happen inside a draw callback (see the wrapper's
/// `editor` module), so the request is recorded and carried out afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeAction {
    OpenSubEditor(usize),
    CloseSubEditor(usize),
}

/// What a node's controls know about the world outside the graph.
///
/// Deliberately narrow. The editor's own context carries the scanned plugin
/// list, the free instance number and the canvas's error banner as well; none
/// of that is a node's business, and leaving it out is what keeps this crate
/// from needing to know what a plugin format is.
pub struct NodeUi<'a> {
    /// How many slots the wrapper has, so a slot picker cannot point past the
    /// table.
    pub slot_count: usize,
    /// Slot index → the sub-plugin parameter it drives and whether that
    /// binding resolved. Shown on slot nodes so the graph reads as "drive the
    /// filter cutoff" rather than as "drive slot 12".
    pub bindings: &'a [(usize, String, bool)],
    /// The value each slot currently has after the graph has had its say.
    pub live: &'a [f32],
    /// What the sub-plugin can accept (§3.3).
    pub poly_modulation: bool,
    /// The sub-block size and the sample rate, which together are the floor a
    /// delay time cannot go below (§14.4). The editor shows it and holds the
    /// control at it; the audio thread applies it again regardless, because
    /// these two can change while a patch is loaded.
    pub quantum: u32,
    pub sample_rate: f64,
    /// Indexed by instance number, so a plugin node can look itself up.
    pub instances: &'a [InstanceView],
    /// What the controls asked the wrapper to do, in the order they asked.
    pub actions: Vec<NodeAction>,
}

impl NodeUi<'_> {
    pub fn act(&mut self, action: NodeAction) {
        self.actions.push(action);
    }
}

/// Colour for a warning that is not an error: a control that still works, but
/// not the way the patch implies.
pub(crate) const CAUTION: egui::Color32 = egui::Color32::from_rgb(200, 140, 60);

/// A control that is only in effect while its socket is empty.
///
/// The rule used to be a line of prose under the node — "b is used only while
/// its input is unconnected" — which is a thing to read rather than a thing to
/// see. Greying the control out says it in the place it applies, and the
/// hover says why.
pub(crate) fn fallback<R>(
    ui: &mut egui::Ui,
    connected: bool,
    add: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let out = ui.add_enabled_ui(!connected, add);
    if connected {
        out.response
            .on_hover_text("driven by what is wired into this socket");
    }
    out.inner
}

/// Which delay line a half belongs to.
///
/// One-based on screen for the same reason a slot is: the two halves are paired
/// by this number and nothing else, so it has to be readable at a glance.
pub(crate) fn line_control(ui: &mut egui::Ui, line: &mut u32) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label("line");
        let mut shown = *line + 1;
        if ui
            .add(egui::DragValue::new(&mut shown).range(1..=16))
            .changed()
        {
            *line = shown.max(1) - 1;
            changed = true;
        }
    });
    changed
}

/// Which wrapper slot a node reads or drives, and what that slot is bound to.
pub(crate) fn slot_picker(ui: &mut egui::Ui, slot: &mut usize, cx: &NodeUi<'_>) -> bool {
    let mut changed = false;
    let slots = cx.slot_count.max(1);
    ui.horizontal(|ui| {
        // One-based on screen, zero-based in the data: the DAW's automation
        // lanes are called "Slot 1".."Slot 32", and disagreeing with them is
        // how a user binds the wrong control.
        let mut shown = *slot + 1;
        if ui
            .add(egui::DragValue::new(&mut shown).range(1..=slots))
            .changed()
        {
            *slot = shown.clamp(1, slots) - 1;
            changed = true;
        }
        ui.label(format!("{:.3}", cx.live.get(*slot).copied().unwrap_or(0.0)));
    });
    match cx.bindings.iter().find(|(i, _, _)| i == slot) {
        Some((_, name, true)) => {
            ui.weak(name);
        }
        Some((_, name, false)) => {
            ui.colored_label(CAUTION, name)
                .on_hover_text("not resolved against the loaded sub-plugin");
        }
        None => {
            ui.weak("not bound to a parameter");
        }
    }
    changed
}

/// Which MIDI key a node watches, shown as a note name beside the number.
///
/// A key switch is set by ear and named in the same breath, and 24 is not a
/// name. The number stays because DAWs disagree with each other about which C
/// is middle C, and the number never does.
pub(crate) fn key_control(ui: &mut egui::Ui, label: &str, key: &mut u8) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(label);
        let mut value = i32::from(*key);
        if ui
            .add(egui::DragValue::new(&mut value).range(0..=127))
            .changed()
        {
            *key = value.clamp(0, 127) as u8;
            changed = true;
        }
        ui.weak(key_name(*key));
    });
    changed
}

/// A MIDI key as a note name, with 60 as C3 — one of the several conventions
/// in use, and the one the rest of this editor reads in.
pub(crate) fn key_name(key: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!("{}{}", NAMES[key as usize % 12], i32::from(key) / 12 - 2)
}

/// Free-running or tempo-synced, and how fast either way.
pub(crate) fn rate_control(ui: &mut egui::Ui, rate: &mut Rate) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let synced = matches!(rate, Rate::Beats(_));
        if ui.selectable_label(!synced, "Hz").clicked() && synced {
            *rate = Rate::Hz(1.0);
            changed = true;
        }
        if ui.selectable_label(synced, "beats").clicked() && !synced {
            *rate = Rate::Beats(1.0);
            changed = true;
        }
        match rate {
            Rate::Hz(hz) => {
                changed |= ui
                    .add(
                        egui::DragValue::new(hz)
                            .speed(0.05)
                            .range(0.0..=40.0)
                            .suffix(" Hz"),
                    )
                    .changed();
            }
            Rate::Beats(beats) => {
                changed |= ui
                    .add(
                        egui::DragValue::new(beats)
                            .speed(0.05)
                            .range(0.03125..=64.0),
                    )
                    .on_hover_text("beats per cycle")
                    .changed();
            }
        }
    });
    changed
}

/// A labelled drop-down over a fixed set of values.
pub(crate) fn combo<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    label: &str,
    current: &mut T,
    all: &[T],
    name: fn(T) -> &'static str,
) -> bool {
    let mut changed = false;
    // Whatever the row has left rather than a fixed width: `NODE_WIDTH` is
    // in canvas units and the `Ui` here is already zoomed, so a constant made
    // the dropdown the one control that did not scale with the rest.
    egui::ComboBox::from_id_salt(ui.id().with(label))
        .selected_text(name(*current))
        .width(ui.available_width())
        .show_ui(ui, |ui| {
            for &option in all {
                if ui
                    .selectable_label(*current == option, name(option))
                    .clicked()
                {
                    *current = option;
                    changed = true;
                }
            }
        });
    changed
}

/// Combo boxes are only so wide, and a parameter name can be long.
pub(crate) fn shorten(text: &str) -> String {
    if text.chars().count() <= 16 {
        return text.to_string();
    }
    text.chars().take(15).collect::<String>() + "\u{2026}"
}
