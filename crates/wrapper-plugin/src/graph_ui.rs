//! The node canvas.
//!
//! Hand-rolled rather than taken from a crate. Every published egui node-graph
//! library — `egui_node_graph2`, `egui-snarl`, `egui-graph-edit` — is pinned to
//! egui 0.29–0.35, and `nice-plug-egui` requires 0.36.1. Two versions of egui
//! cannot share a `Ui`, so using one would mean holding the plugin's whole GUI
//! stack back a release. A canvas is a few hundred lines; that trade is not
//! close.
//!
//! Everything here runs on the main thread and edits [`Graph`] in place. That
//! is safe in a way the rest of the editor is not (see [`crate::editor`]):
//! moving a node or changing a number touches no window and dispatches no
//! platform message, so there is no reentrancy to defer around. Only the
//! *result* — recompiling and publishing — is worth doing once at the end of
//! the frame rather than on every mutation, and that is what the returned
//! `changed` flag is for.

use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};
use subhost_adapter::SLOT_COUNT;
use wrapper_engine::{ExprSource, Graph, MathOp, NodeId, NodeKind, Rate, Waveform};

const NODE_WIDTH: f32 = 186.0;
/// Vertical space between a node's top edge and its first port.
const PORT_TOP: f32 = 26.0;
const PORT_SPACING: f32 = 18.0;
const PORT_RADIUS: f32 = 5.0;
const GRID: f32 = 24.0;

/// What the canvas needs to know about the world outside the graph.
pub struct GraphContext<'a> {
    /// Slot index → the sub-plugin parameter it drives, for the ones that have
    /// one. Shown on slot nodes so the graph reads as "drive the filter cutoff"
    /// rather than as "drive slot 12".
    pub bindings: &'a [(usize, String, bool)],
    /// What the sub-plugin can accept (§3.3).
    pub poly_modulation: bool,
    /// Why the graph on screen is not the graph being heard.
    pub error: Option<String>,
    /// The value each slot currently has after the graph has had its say.
    pub live: [f32; SLOT_COUNT],
}

/// Canvas state that belongs to the view rather than to the patch.
#[derive(Default)]
pub struct GraphEditor {
    pan: Vec2,
    /// The node being dragged and where inside it the pointer grabbed.
    dragging: Option<(NodeId, Vec2)>,
    /// An output port the user has picked up but not yet dropped.
    linking: Option<NodeId>,
    /// Where a right-click asked for a new node, in graph coordinates.
    add_at: Option<Pos2>,
}

impl GraphEditor {
    /// Draw the canvas. Returns whether the graph was modified.
    pub fn ui(&mut self, ui: &mut egui::Ui, graph: &mut Graph, ctx: &GraphContext<'_>) -> bool {
        let mut changed = false;

        self.toolbar(ui, graph, ctx, &mut changed);

        let available = ui.available_size();
        let (canvas, response) = ui.allocate_exact_size(available, Sense::click_and_drag());
        let painter = ui.painter_at(canvas);
        painter.rect_filled(canvas, 4.0, ui.visuals().extreme_bg_color);

        // Panning the background moves the view, not the patch, so node
        // positions stay meaningful when the window is resized.
        if response.dragged_by(egui::PointerButton::Middle)
            || (response.dragged() && self.dragging.is_none() && self.linking.is_none())
        {
            self.pan += response.drag_delta();
        }

        self.grid(&painter, canvas);

        // Links are drawn under the nodes but their endpoints are not known
        // until the nodes have been laid out. egui answers that with a
        // placeholder shape that can be filled in later, which is cheaper and
        // more honest than drawing from the previous frame's geometry.
        let link_layer = painter.add(egui::Shape::Noop);

        let origin = canvas.min + self.pan;
        let mut ports: Vec<(NodeId, Pos2, Vec<Pos2>)> = Vec::with_capacity(graph.nodes.len());
        let mut to_remove: Option<NodeId> = None;
        let mut to_connect: Option<(NodeId, NodeId, u8)> = None;
        let mut to_disconnect: Option<(NodeId, u8)> = None;

        for index in 0..graph.nodes.len() {
            let id = graph.nodes[index].id;
            let pos = origin + Vec2::new(graph.nodes[index].pos[0], graph.nodes[index].pos[1]);
            let outcome = self.node(ui, canvas, graph, index, pos, ctx);

            changed |= outcome.changed;
            if outcome.remove {
                to_remove = Some(id);
            }
            if let Some(input) = outcome.clicked_input {
                match self.linking.take() {
                    Some(from) => to_connect = Some((from, id, input)),
                    // Clicking a connected input with nothing in hand takes the
                    // link off, which is the same gesture as making one and so
                    // needs no separate control.
                    None => to_disconnect = Some((id, input)),
                }
            }
            if outcome.clicked_output {
                self.linking = Some(id);
            }
            ports.push((id, outcome.output_port, outcome.input_ports));
        }

        // Drawing the links, now that every node has been placed.
        let mut shapes: Vec<egui::Shape> = Vec::with_capacity(graph.links.len() + 1);
        for link in &graph.links {
            let from = ports.iter().find(|(id, _, _)| *id == link.from);
            let to = ports.iter().find(|(id, _, _)| *id == link.to);
            if let (Some((_, out, _)), Some((_, _, ins))) = (from, to)
                && let Some(&target) = ins.get(link.input as usize)
            {
                shapes.push(link_shape(*out, target, ui.visuals().weak_text_color()));
            }
        }
        if let Some(from) = self.linking
            && let Some((_, out, _)) = ports.iter().find(|(id, _, _)| *id == from)
            && let Some(pointer) = ui.ctx().pointer_latest_pos()
        {
            shapes.push(link_shape(
                *out,
                pointer,
                ui.visuals().selection.stroke.color,
            ));
        }
        painter.set(link_layer, egui::Shape::Vec(shapes));

        if let Some(id) = to_remove {
            graph.remove(id);
            changed = true;
        }
        if let Some((from, to, input)) = to_connect {
            graph.connect(from, to, input);
            changed = true;
        }
        if let Some((to, input)) = to_disconnect {
            changed |= graph.source_of(to, input).is_some();
            graph.disconnect(to, input);
        }

        // Dropping a half-made link on empty canvas abandons it, which is what
        // every other node editor does and therefore what the hand expects.
        if response.clicked() {
            self.linking = None;
            self.add_at = None;
        }
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.linking = None;
        }

        if response.secondary_clicked()
            && let Some(pointer) = ui.ctx().pointer_latest_pos()
        {
            self.add_at = Some(pointer - self.pan - canvas.min.to_vec2());
        }
        changed |= self.add_menu(ui, canvas, graph);

        changed
    }

    fn toolbar(
        &mut self,
        ui: &mut egui::Ui,
        graph: &mut Graph,
        ctx: &GraphContext<'_>,
        changed: &mut bool,
    ) {
        ui.horizontal(|ui| {
            ui.heading("Graph");
            ui.weak(format!("{} nodes", graph.nodes.len()));
            if ui.button("Add node").clicked() {
                // Somewhere visible whatever the pan is.
                self.add_at = Some(Pos2::new(40.0, 40.0) - self.pan);
            }
            if ui.button("Centre").clicked() {
                self.pan = Vec2::ZERO;
            }
            if !graph.is_empty()
                && ui
                    .button("Clear")
                    .on_hover_text("delete every node")
                    .clicked()
            {
                *graph = Graph::new();
                *changed = true;
            }
        });
        ui.weak(
            "right-click the canvas to add a node · drag an output onto an input to connect \
             · click a connected input to disconnect",
        );
        if let Some(error) = &ctx.error {
            // The last program that compiled is still running, so this is a
            // warning about what is *not* being heard, not a failure.
            ui.colored_label(
                Color32::from_rgb(220, 150, 60),
                format!("not applied: {error} — the previous graph is still running"),
            );
        }
    }

    fn grid(&self, painter: &egui::Painter, canvas: Rect) {
        let stroke = Stroke::new(1.0, Color32::from_gray(60).gamma_multiply(0.5));
        let offset = Vec2::new(self.pan.x.rem_euclid(GRID), self.pan.y.rem_euclid(GRID));
        let mut x = canvas.min.x + offset.x - GRID;
        while x < canvas.max.x {
            painter.vline(x, canvas.y_range(), stroke);
            x += GRID;
        }
        let mut y = canvas.min.y + offset.y - GRID;
        while y < canvas.max.y {
            painter.hline(canvas.x_range(), y, stroke);
            y += GRID;
        }
    }

    /// Draw one node and report what the user did to it.
    fn node(
        &mut self,
        ui: &mut egui::Ui,
        canvas: Rect,
        graph: &mut Graph,
        index: usize,
        pos: Pos2,
        ctx: &GraphContext<'_>,
    ) -> NodeOutcome {
        let id = graph.nodes[index].id;
        let mut outcome = NodeOutcome::default();

        let inputs = graph.nodes[index].kind.inputs();
        let has_output = graph.nodes[index].kind.has_output();
        let title = graph.nodes[index].kind.title();

        let body = Rect::from_min_size(pos, Vec2::new(NODE_WIDTH, 0.0));
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(Rect::from_min_size(body.min, Vec2::new(NODE_WIDTH, 400.0)))
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        // Clipped to the canvas so a node dragged off the edge does not paint
        // over the panels around it.
        child.set_clip_rect(canvas);

        let frame = egui::Frame::group(ui.style())
            .fill(ui.visuals().panel_fill)
            .stroke(Stroke::new(
                1.0,
                ui.visuals().widgets.noninteractive.bg_stroke.color,
            ));

        let response = frame.show(&mut child, |ui| {
            ui.set_width(NODE_WIDTH - 16.0);
            ui.horizontal(|ui| {
                ui.strong(&title);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("x").clicked() {
                        outcome.remove = true;
                    }
                });
            });
            ui.separator();
            outcome.changed |= self.controls(ui, &mut graph.nodes[index].kind, ctx);
        });

        let rect = response.response.rect;

        // The title bar is the drag handle. Dragging from the body would fight
        // every slider and combo box inside it.
        let handle = Rect::from_min_size(rect.min, Vec2::new(rect.width(), PORT_TOP));
        let drag = ui.interact(handle, ui.id().with(("node", id)), Sense::drag());
        if drag.drag_started()
            && let Some(pointer) = ui.ctx().pointer_latest_pos()
        {
            self.dragging = Some((id, pointer - rect.min));
        }
        if let Some((dragged, grab)) = self.dragging
            && dragged == id
        {
            if drag.dragged()
                && let Some(pointer) = ui.ctx().pointer_latest_pos()
            {
                let target = pointer - grab - self.pan - canvas.min.to_vec2();
                graph.nodes[index].pos = [target.x, target.y];
                // A moved node is a changed patch — it has to be saved — but it
                // compiles to exactly the same program, so recompiling is
                // wasted work. Positions are handled by the caller's save, not
                // by `changed`.
            }
            if drag.drag_stopped() {
                self.dragging = None;
                // Reported as a change so the new position is saved. It
                // compiles to an identical program, so the recompile it also
                // triggers is one wasted pass at the end of a drag rather than
                // one per frame during it.
                outcome.changed = true;
            }
        }

        // Ports, drawn on the node's edges.
        let painter = ui.painter_at(canvas);
        for (i, name) in inputs.iter().enumerate() {
            let centre = Pos2::new(rect.min.x, rect.min.y + PORT_TOP + i as f32 * PORT_SPACING);
            let connected = graph.source_of(id, i as u8).is_some();
            if self.port(ui, &painter, (id, i as u8 + 1), centre, connected, name) {
                outcome.clicked_input = Some(i as u8);
            }
            outcome.input_ports.push(centre);
        }
        if has_output {
            let centre = Pos2::new(rect.max.x, rect.min.y + PORT_TOP);
            let connected = graph.links.iter().any(|l| l.from == id);
            if self.port(ui, &painter, (id, 0), centre, connected, "out") {
                outcome.clicked_output = true;
            }
            outcome.output_port = centre;
        }

        outcome
    }

    /// One port circle. Returns whether it was clicked.
    fn port(
        &self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        // The node and which of its ports this is — 0 for the output. Keyed on
        // identity rather than on position, because a port's position changes
        // every frame of a drag and two of them can land on the same pixel.
        which: (NodeId, u8),
        centre: Pos2,
        connected: bool,
        name: &str,
    ) -> bool {
        let hit = Rect::from_center_size(centre, Vec2::splat(PORT_RADIUS * 3.0));
        let response = ui.interact(hit, ui.id().with(("port", which)), Sense::click());
        let colour = if connected {
            ui.visuals().selection.stroke.color
        } else if response.hovered() {
            ui.visuals().strong_text_color()
        } else {
            ui.visuals().weak_text_color()
        };
        painter.circle_filled(centre, PORT_RADIUS, colour);
        if response.hovered() {
            response.clone().on_hover_text(name);
        }
        response.clicked()
    }

    /// The per-kind controls inside a node. Returns whether anything changed.
    fn controls(&self, ui: &mut egui::Ui, kind: &mut NodeKind, ctx: &GraphContext<'_>) -> bool {
        let mut changed = false;
        match kind {
            NodeKind::Constant { value } => {
                changed |= ui.add(egui::Slider::new(value, 0.0..=1.0)).changed();
            }
            NodeKind::SlotIn { slot } | NodeKind::SlotOut { slot } => {
                changed |= slot_picker(ui, slot, ctx);
            }
            NodeKind::Lfo {
                waveform,
                rate,
                phase,
                depth,
                offset,
            } => {
                changed |= combo(ui, "wave", waveform, &Waveform::ALL, Waveform::label);
                changed |= rate_control(ui, rate);
                ui.horizontal(|ui| {
                    ui.label("phase");
                    changed |= ui
                        .add(egui::DragValue::new(phase).speed(0.01).range(0.0..=1.0))
                        .changed();
                });
                ui.horizontal(|ui| {
                    ui.label("depth");
                    changed |= ui
                        .add(egui::DragValue::new(depth).speed(0.01).range(-1.0..=1.0))
                        .changed();
                    ui.label("centre");
                    changed |= ui
                        .add(egui::DragValue::new(offset).speed(0.01).range(-1.0..=1.0))
                        .changed();
                });
            }
            NodeKind::Expression { source } => {
                changed |= combo(ui, "source", source, &ExprSource::ALL, ExprSource::label);
                if source.is_per_note() && !ctx.poly_modulation {
                    // §3.3 asks for per-voice sources to be greyed out when the
                    // sub-plugin cannot take per-voice modulation. In v1 the
                    // graph is monophonic, so these still do something useful —
                    // they are just flattened. Saying so is more use than
                    // disabling a control that works.
                    ui.colored_label(Color32::from_rgb(200, 160, 70), "newest note only")
                        .on_hover_text(
                            "the sub-plugin cannot take per-voice modulation, so every \
                             held note contributes to one value",
                        );
                }
            }
            NodeKind::Math { op, b } => {
                changed |= combo(ui, "op", op, &MathOp::ALL, MathOp::label);
                ui.horizontal(|ui| {
                    ui.label("b");
                    changed |= ui.add(egui::DragValue::new(b).speed(0.01)).changed();
                });
                ui.weak("b is used only while its input is unconnected");
            }
            NodeKind::RangeMap {
                in_lo,
                in_hi,
                out_lo,
                out_hi,
                clamp,
            } => {
                ui.horizontal(|ui| {
                    ui.label("in");
                    changed |= ui.add(egui::DragValue::new(in_lo).speed(0.01)).changed();
                    changed |= ui.add(egui::DragValue::new(in_hi).speed(0.01)).changed();
                });
                ui.horizontal(|ui| {
                    ui.label("out");
                    changed |= ui.add(egui::DragValue::new(out_lo).speed(0.01)).changed();
                    changed |= ui.add(egui::DragValue::new(out_hi).speed(0.01)).changed();
                });
                changed |= ui.checkbox(clamp, "clamp").changed();
            }
        }
        changed
    }

    /// The "add a node" menu, shown wherever the user asked for it.
    fn add_menu(&mut self, ui: &mut egui::Ui, canvas: Rect, graph: &mut Graph) -> bool {
        let Some(at) = self.add_at else { return false };
        let mut added = false;
        let mut close = false;

        egui::Area::new(ui.id().with("add-node"))
            .order(egui::Order::Foreground)
            .fixed_pos(canvas.min + at.to_vec2() + self.pan)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(150.0);
                    for (label, kind) in catalogue() {
                        if ui.button(label).clicked() {
                            graph.add(kind, [at.x, at.y]);
                            added = true;
                            close = true;
                        }
                    }
                    ui.separator();
                    if ui.button("cancel").clicked() {
                        close = true;
                    }
                });
            });

        if close {
            self.add_at = None;
        }
        added
    }
}

/// What one node's frame reported back.
#[derive(Default)]
struct NodeOutcome {
    changed: bool,
    remove: bool,
    clicked_input: Option<u8>,
    clicked_output: bool,
    output_port: Pos2,
    input_ports: Vec<Pos2>,
}

fn catalogue() -> Vec<(&'static str, NodeKind)> {
    vec![
        ("Constant", NodeKind::Constant { value: 0.5 }),
        ("Slot in", NodeKind::SlotIn { slot: 0 }),
        (
            "LFO",
            NodeKind::Lfo {
                waveform: Waveform::Sine,
                rate: Rate::Hz(1.0),
                phase: 0.0,
                // Centred on 0.5 with a half swing fills 0..1 exactly, which is
                // the range a slot wants; anything else needs a Range map and
                // would make a freshly dropped LFO look broken.
                depth: 0.5,
                offset: 0.5,
            },
        ),
        (
            "Expression",
            NodeKind::Expression {
                source: ExprSource::Pressure,
            },
        ),
        (
            "Math",
            NodeKind::Math {
                op: MathOp::Multiply,
                b: 1.0,
            },
        ),
        (
            "Range map",
            NodeKind::RangeMap {
                in_lo: 0.0,
                in_hi: 1.0,
                out_lo: 0.0,
                out_hi: 1.0,
                clamp: true,
            },
        ),
        ("Slot out", NodeKind::SlotOut { slot: 0 }),
    ]
}

fn slot_picker(ui: &mut egui::Ui, slot: &mut usize, ctx: &GraphContext<'_>) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        // One-based on screen, zero-based in the data: the DAW's automation
        // lanes are called "Slot 1".."Slot 32", and disagreeing with them is
        // how a user binds the wrong control.
        let mut shown = *slot + 1;
        if ui
            .add(egui::DragValue::new(&mut shown).range(1..=SLOT_COUNT))
            .changed()
        {
            *slot = shown.clamp(1, SLOT_COUNT) - 1;
            changed = true;
        }
        ui.label(format!(
            "{:.3}",
            ctx.live.get(*slot).copied().unwrap_or(0.0)
        ));
    });
    match ctx.bindings.iter().find(|(i, _, _)| i == slot) {
        Some((_, name, true)) => {
            ui.weak(name);
        }
        Some((_, name, false)) => {
            ui.colored_label(Color32::from_rgb(200, 140, 60), name)
                .on_hover_text("not resolved against the loaded sub-plugin");
        }
        None => {
            ui.weak("not bound to a parameter");
        }
    }
    changed
}

fn rate_control(ui: &mut egui::Ui, rate: &mut Rate) -> bool {
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
fn combo<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    label: &str,
    current: &mut T,
    all: &[T],
    name: fn(T) -> &'static str,
) -> bool {
    let mut changed = false;
    egui::ComboBox::from_id_salt(ui.id().with(label))
        .selected_text(name(*current))
        .width(NODE_WIDTH - 40.0)
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

/// A cubic curve between two ports, leaving and arriving horizontally.
fn link_shape(from: Pos2, to: Pos2, colour: Color32) -> egui::Shape {
    let reach = ((to.x - from.x).abs() * 0.5).clamp(30.0, 120.0);
    egui::Shape::CubicBezier(egui::epaint::CubicBezierShape::from_points_stroke(
        [
            from,
            from + Vec2::new(reach, 0.0),
            to - Vec2::new(reach, 0.0),
            to,
        ],
        false,
        Color32::TRANSPARENT,
        Stroke::new(2.0, colour),
    ))
}
