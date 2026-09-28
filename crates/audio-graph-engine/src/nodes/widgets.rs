//! Shared UI widgets and context structures for rendering graph nodes with egui.
//!
//! Only compiled with the `ui` feature, which only the wrapper turns on. The CLI
//! and the adapter link the same crate without egui in the tree at all —
//! `cargo tree -p host-cli` is the check.
//!
//! The feature exists so that a node's controls can sit in the node's own file
//! rather than in a second `match` in the editor crate. Putting the canvas on
//! one side of the line and the node on the other is what settles where a new
//! node's code goes: all of it in the node's file.
//!
//! What stays with the canvas is everything *about the canvas* — panning,
//! zooming, drawing links, the add-node menu, loading plugins. A node never
//! learns any of that; it is handed a `Ui` the right size and a [`NodeUi`] of
//! facts about the world outside the graph.

use crate::nodes::Rate;

/// Standard width of a node's body in canvas units.
///
/// Here rather than in the editor because a node's controls are laid out against
/// it — a combo box wider than the node it sits in is the kind of thing that
/// only shows up once somebody adds a node.
pub const NODE_WIDTH: f32 = 232.0;

/// One sub-plugin instance, as the node holding it needs to draw it.
///
/// Filled in by the wrapper: this crate has no idea what is loaded, and does not
/// gain one by drawing it.
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
/// Opening a window may not happen inside a draw callback, so the request is
/// recorded and carried out afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeAction {
    OpenSubEditor(usize),
    CloseSubEditor(usize),
    /// Arm learning on an instance: the next parameter moved in its window
    /// gets a socket. See [`NodeUi::learning`].
    Learn(usize),
    StopLearning,
}

/// The last parameter moved in one sub-plugin's own window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Touch {
    /// Which edit this was, counted across every instance; 0 for an instance
    /// nobody has touched. Compared for change, never for order.
    pub edit: u32,
    pub param: u32,
}

/// What a node's controls know about the world outside the graph.
///
/// Deliberately narrow. The editor's own context carries the scanned plugin
/// list, the free instance number and the canvas's error banner as well; none of
/// that is a node's business, and leaving it out is what keeps this crate from
/// needing to know what a plugin format is.
pub struct NodeUi<'a> {
    /// How many slots the wrapper has, so a slot picker cannot point past the
    /// table.
    pub slot_count: usize,
    /// Slot index → the sub-plugin parameter it drives and whether that binding
    /// resolved. Shown on slot nodes so the graph reads as "drive the filter
    /// cutoff" rather than as "drive slot 12".
    pub bindings: &'a [(usize, String, bool)],
    /// Live normalized parameter values for each slot.
    pub live: &'a [f32],
    /// What is arriving at each of this node's input sockets, by port: `None`
    /// for an empty socket, and for one whose value the editor has not heard
    /// back yet.
    pub inputs: &'a [Option<f64>],
    /// Whether the hosted plugin supports polyphonic parameter modulation.
    pub poly_modulation: bool,
    /// The sub-block size and the sample rate, which together are the floor a
    /// delay time cannot go below. The editor shows it and holds the control at
    /// it; the audio thread applies it again regardless, because these two can
    /// change while a patch is loaded.
    pub quantum: u32,
    pub sample_rate: f64,
    /// Indexed by instance number, so a plugin node can look itself up.
    pub instances: &'a [InstanceView],
    /// The last parameter moved in each instance's own window, by instance.
    pub touched: &'a [Touch],
    /// The instance whose next touched parameter becomes a socket, and the
    /// [`Touch::edit`] it had when learning was armed — a touch still carrying
    /// that number happened before the user asked.
    pub learning: Option<(usize, u32)>,
    /// List of requested actions queued for execution by the host wrapper.
    pub actions: Vec<NodeAction>,
}

impl NodeUi<'_> {
    pub fn act(&mut self, action: NodeAction) {
        self.actions.push(action);
    }

    /// What is arriving at input `port`, if it is wired and has been heard.
    pub fn input(&self, port: u8) -> Option<f64> {
        self.inputs.get(port as usize).copied().flatten()
    }
}

/// Colour for a warning that is not an error: a control that still works, but
/// not the way the patch implies.
pub(crate) const CAUTION: egui::Color32 = egui::Color32::from_rgb(200, 140, 60);

/// A control that is only in effect while its socket is empty.
///
/// The rule could be a line of prose under the node — "b is used only while its
/// input is unconnected" — but that is a thing to read rather than a thing to
/// see. Greying the control out says it in the place it applies, and the hover
/// says why.
///
/// Once the socket's value has been heard, the control shows that instead of
/// `value`, drawn by `add` so it keeps the control's own format and width. It
/// loses its frame rather than its colour: a greyed number is hard to read
/// while it moves, and the missing box is what says it cannot be edited. `add`
/// works on a copy there, so the stored value is left for when the link goes.
pub(crate) fn fallback(
    ui: &mut egui::Ui,
    connected: bool,
    live: Option<f64>,
    value: &mut f64,
    add: impl FnOnce(&mut egui::Ui, &mut f64) -> bool,
) -> bool {
    let hover = "driven by what is wired into this socket";
    match (connected, live) {
        (false, _) => add(ui, value),
        (true, None) => {
            let out = ui.add_enabled_ui(false, |ui| add(ui, value));
            out.response.on_hover_text(hover);
            out.inner
        }
        (true, Some(mut live)) => {
            let out = ui.scope(|ui| {
                // Disabled for the input it would otherwise take, at full
                // opacity for the colour it would otherwise lose.
                let opacity = ui.opacity();
                ui.disable();
                ui.set_opacity(opacity);
                // Every state, not just `noninteractive`: a disabled widget
                // keeps its sense, so it is still drawn as inactive or hovered.
                // The stroke keeps its width so the text sits where it would
                // with the frame.
                let widgets = &mut ui.visuals_mut().widgets;
                let text = widgets.inactive.fg_stroke;
                for state in [
                    &mut widgets.noninteractive,
                    &mut widgets.inactive,
                    &mut widgets.hovered,
                    &mut widgets.active,
                    &mut widgets.open,
                ] {
                    state.fg_stroke = text;
                    state.bg_fill = egui::Color32::TRANSPARENT;
                    state.weak_bg_fill = egui::Color32::TRANSPARENT;
                    state.bg_stroke.color = egui::Color32::TRANSPARENT;
                    state.expansion = 0.0;
                }
                add(ui, &mut live);
            });
            out.response.on_hover_text(hover);
            false
        }
    }
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

/// Renders wrapper slot selector and displays current binding name and live value.
pub(crate) fn slot_picker(ui: &mut egui::Ui, slot: &mut usize, cx: &NodeUi<'_>) -> bool {
    let mut changed = false;
    let slots = cx.slot_count.max(1);
    ui.horizontal(|ui| {
        // One-based on screen, zero-based in the data: the DAW's automation
        // lanes are called "Slot 1".."Slot 32", and disagreeing with them is how
        // a user binds the wrong control.
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
/// name. The number stays because DAWs disagree with each other about which C is
/// middle C, and the number never does.
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

/// A MIDI key as a note name, with 60 as C3 — one of the several conventions in
/// use, and the one the rest of this editor reads in.
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
    // Whatever the row has left rather than a fixed width: `NODE_WIDTH` is in
    // canvas units and the `Ui` here is already zoomed, so a constant made the
    // dropdown the one control that did not scale with the rest.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Whether any rectangle the frame painted can be seen.
    fn paints_a_box(connected: bool, live: Option<f64>, hovered: bool) -> bool {
        let ctx = egui::Context::default();
        let mut value = 0.25;
        let mut shapes = Vec::new();
        // Two frames with the pointer where the control is: a widget's
        // hovered look is decided from the frame before.
        for _ in 0..2 {
            let mut input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(200.0, 100.0),
                )),
                ..Default::default()
            };
            if hovered {
                input
                    .events
                    .push(egui::Event::PointerMoved(egui::pos2(20.0, 15.0)));
            }
            let output = ctx.run_ui(input, |ui| {
                fallback(ui, connected, live, &mut value, |ui, v| {
                    ui.add(egui::DragValue::new(v).speed(0.01)).changed()
                });
            });
            shapes = output.shapes.clone();
            output.drop_without_applying_deltas();
        }
        shapes.iter().any(|clipped| match &clipped.shape {
            egui::Shape::Rect(rect) => {
                rect.fill != egui::Color32::TRANSPARENT
                    || (rect.stroke.width > 0.0 && rect.stroke.color != egui::Color32::TRANSPARENT)
            }
            _ => false,
        })
    }

    /// A control showing its socket's value has no box around it, hovered or
    /// not: the missing box is what says it cannot be edited.
    #[test]
    fn a_control_showing_its_socket_has_no_box() {
        assert!(
            paints_a_box(false, None, false),
            "an editable control has one"
        );
        assert!(!paints_a_box(true, Some(0.5), false));
        assert!(!paints_a_box(true, Some(0.5), true));
    }
}
