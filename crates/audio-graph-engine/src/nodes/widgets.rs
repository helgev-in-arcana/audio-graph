//! Shared UI widgets and context structures for rendering graph nodes with egui.

use crate::nodes::Rate;

/// Standard width of a node's body in canvas units.
pub const NODE_WIDTH: f32 = 232.0;

/// UI state snapshot of a hosted sub-plugin instance provided by the host wrapper.
#[derive(Default, Clone)]
pub struct InstanceView {
    pub loaded: bool,
    pub name: String,
    pub editor_open: bool,
    /// `(id, name)` for every parameter, to fill a socket's dropdown.
    pub params: Vec<(u32, String)>,
}

/// Asynchronous action requested by node UI controls to be executed by the host wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeAction {
    OpenSubEditor(usize),
    CloseSubEditor(usize),
}

/// External context and runtime state passed to node UI rendering callbacks.
pub struct NodeUi<'a> {
    /// Total number of available automation slots in the host wrapper.
    pub slot_count: usize,
    /// Slot mappings: (slot index, target parameter name, whether binding resolved).
    pub bindings: &'a [(usize, String, bool)],
    /// Live normalized parameter values for each slot.
    pub live: &'a [f32],
    /// Whether the hosted plugin supports polyphonic parameter modulation.
    pub poly_modulation: bool,
    /// Audio engine block processing quantum (in samples) and sample rate.
    pub quantum: u32,
    pub sample_rate: f64,
    /// Sub-plugin instance metadata indexed by instance ID.
    pub instances: &'a [InstanceView],
    /// List of requested actions queued for execution by the host wrapper.
    pub actions: Vec<NodeAction>,
}

impl NodeUi<'_> {
    pub fn act(&mut self, action: NodeAction) {
        self.actions.push(action);
    }
}

/// Warning highlight color for non-critical advisory notices.
pub(crate) const CAUTION: egui::Color32 = egui::Color32::from_rgb(200, 140, 60);

/// Renders an inline fallback control disabled/greyed out when an input socket is connected.
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

/// Renders delay line ID selector (1-indexed for display).
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

/// Renders wrapper slot selector and displays current binding name and live value.
pub(crate) fn slot_picker(ui: &mut egui::Ui, slot: &mut usize, cx: &NodeUi<'_>) -> bool {
    let mut changed = false;
    let slots = cx.slot_count.max(1);
    ui.horizontal(|ui| {
        // 1-indexed for display to match host DAW slot labeling.
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

/// Renders MIDI key selector (0..=127) with note name display.
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

/// Converts a MIDI note number (0..=127) to standard note name notation (e.g. 60 -> "C3").
pub(crate) fn key_name(key: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!("{}{}", NAMES[key as usize % 12], i32::from(key) / 12 - 2)
}

/// Renders rate selector supporting free-running Hz or tempo-synced beat divisions.
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

/// Renders a labeled dropdown combo box for a slice of enum values.
pub(crate) fn combo<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    label: &str,
    current: &mut T,
    all: &[T],
    name: fn(T) -> &'static str,
) -> bool {
    let mut changed = false;
    // Scale combo box to occupy available row width.
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

/// Truncates strings longer than 16 characters with an ellipsis for compact UI display.
pub(crate) fn shorten(text: &str) -> String {
    if text.chars().count() <= 16 {
        return text.to_string();
    }
    text.chars().take(15).collect::<String>() + "\u{2026}"
}
