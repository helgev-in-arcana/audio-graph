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

use std::path::PathBuf;

use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};
use subhost_adapter::SLOT_COUNT;
use wrapper_engine::{
    ExprSource, Graph, MathOp, NodeId, NodeKind, ParamPort, PluginPorts, PortType, Rate, Waveform,
};

const NODE_WIDTH: f32 = 186.0;
/// Vertical space between a node's top edge and its first port.
const PORT_TOP: f32 = 26.0;
const PORT_SPACING: f32 = 18.0;
const PORT_RADIUS: f32 = 5.0;
const GRID: f32 = 24.0;

/// A `.vst3` found on disk, offered in the add-node menu.
pub struct PluginEntry {
    pub name: String,
    pub path: PathBuf,
}

/// One sub-plugin instance, as the canvas needs to draw the node holding it.
#[derive(Default, Clone)]
pub struct InstanceView {
    pub loaded: bool,
    pub name: String,
    pub editor_open: bool,
    /// `(id, name)` for every parameter, to fill a socket's dropdown.
    pub params: Vec<(u32, String)>,
}

/// Something the canvas cannot do itself, because it loads a plugin or touches
/// a window — see the module comment on [`crate::editor`] for why that must not
/// happen inside a draw callback.
pub enum GraphAction {
    /// Load `path` into `instance` and give `node` the sockets it turns out to
    /// have (§14.2).
    LoadPlugin {
        node: NodeId,
        instance: usize,
        path: PathBuf,
    },
    UnloadInstance(usize),
    OpenSubEditor(usize),
    CloseSubEditor(usize),
}

/// What the canvas needs to know about the world outside the graph.
pub struct GraphContext<'a> {
    /// What can be loaded, scanned from the plugin directories.
    pub plugins: &'a [PluginEntry],
    /// Indexed by instance number, so a plugin node can look itself up.
    pub instances: &'a [InstanceView],
    /// The lowest instance number nothing is loaded into, or `None` when the
    /// wrapper is full.
    pub free_instance: Option<usize>,
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
    /// The sub-block size and the sample rate, which together are the floor a
    /// delay time cannot go below (§14.4). The editor shows it and holds the
    /// control at it; the audio thread applies it again regardless, because
    /// these two can change while a patch is loaded.
    pub quantum: u32,
    pub sample_rate: f64,
}

/// Canvas state that belongs to the view rather than to the patch.
#[derive(Default)]
pub struct GraphEditor {
    pan: Vec2,
    /// The node being dragged and where inside it the pointer grabbed.
    dragging: Option<(NodeId, Vec2)>,
    /// An output port the user has picked up but not yet dropped, and which
    /// of the node's outputs it was.
    linking: Option<(NodeId, u8)>,
    /// Where a right-click asked for a new node, in graph coordinates.
    add_at: Option<Pos2>,
    /// Filter text in the add-node menu's plugin list.
    plugin_filter: String,
    /// Actions for the caller to carry out once the frame is over.
    actions: Vec<GraphAction>,
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
        let mut ports: Vec<(NodeId, Vec<Pos2>, Vec<Pos2>)> = Vec::with_capacity(graph.nodes.len());
        let mut to_remove: Option<NodeId> = None;
        let mut to_connect: Option<(NodeId, u8, NodeId, u8)> = None;
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
                    Some((from, from_port)) => to_connect = Some((from, from_port, id, input)),
                    // Clicking a connected input with nothing in hand takes the
                    // link off, which is the same gesture as making one and so
                    // needs no separate control.
                    None => to_disconnect = Some((id, input)),
                }
            }
            if let Some(output) = outcome.clicked_output {
                self.linking = Some((id, output));
            }
            ports.push((id, outcome.output_ports, outcome.input_ports));
        }

        // Drawing the links, now that every node has been placed.
        let mut shapes: Vec<egui::Shape> = Vec::with_capacity(graph.links.len() + 1);
        for link in &graph.links {
            let from = ports.iter().find(|(id, _, _)| *id == link.from);
            let to = ports.iter().find(|(id, _, _)| *id == link.to);
            if let (Some((_, outs, _)), Some((_, _, ins))) = (from, to)
                && let Some(&source) = outs.get(link.from_port as usize)
                && let Some(&target) = ins.get(link.to_port as usize)
            {
                shapes.push(link_shape(source, target, ui.visuals().weak_text_color()));
            }
        }
        if let Some((from, from_port)) = self.linking
            && let Some((_, outs, _)) = ports.iter().find(|(id, _, _)| *id == from)
            && let Some(&source) = outs.get(from_port as usize)
            && let Some(pointer) = ui.ctx().pointer_latest_pos()
        {
            shapes.push(link_shape(
                source,
                pointer,
                ui.visuals().selection.stroke.color,
            ));
        }
        painter.set(link_layer, egui::Shape::Vec(shapes));

        if let Some(id) = to_remove {
            // Deleting the node is what unloads the plugin. There is no
            // separate "unload" anywhere, because a node with no plugin in it
            // is not a thing the user asked for.
            if let Some(NodeKind::Plugin { instance, .. }) = graph.node(id).map(|n| &n.kind) {
                self.actions.push(GraphAction::UnloadInstance(*instance));
            }
            graph.remove(id);
            changed = true;
        }
        if let Some((from, from_port, to, input)) = to_connect {
            graph.connect(from, from_port, to, input);
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
        changed |= self.add_menu(ui, canvas, graph, ctx);

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
            if ui
                .button("Reset")
                .on_hover_text("back to audio in -> audio out")
                .clicked()
            {
                *graph = Graph::default_patch();
                *changed = true;
            }
            if !graph.is_empty()
                && ui
                    .button("Clear")
                    .on_hover_text("delete every node — nothing drawn means silence")
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

        let inputs = graph.nodes[index].kind.input_ports();
        let outputs = graph.nodes[index].kind.output_ports();
        let title = graph.nodes[index].kind.title();

        let body = Rect::from_min_size(pos, Vec2::new(NODE_WIDTH, 0.0));
        // The drag handle is registered before the node's contents so that the
        // widgets inside win the pointer. egui resolves a click to the last
        // widget registered over it, and the title bar is both the handle and
        // the home of the delete button.
        let handle = Rect::from_min_size(pos, Vec2::new(NODE_WIDTH, PORT_TOP));
        let drag = ui.interact(handle, ui.id().with(("node", id)), Sense::drag());

        let mut child = ui.new_child(
            egui::UiBuilder::new()
                // Without a salt of its own, every node's contents land in the
                // same id namespace: two nodes of a kind then share one combo
                // box's open/closed state and one button's click, and the
                // second one drawn wins. Node id makes each node its own world.
                .id_salt(("graph-node", id))
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
                    // Only worth offering where it changes anything: a node
                    // with no output is already compiled.
                    if !outputs.is_empty() {
                        let on = &mut graph.nodes[index].always_on;
                        if ui
                            .toggle_value(on, "on")
                            .on_hover_text(
                                "run this node even with nothing wired to its output                                  — for analysers",
                            )
                            .changed()
                        {
                            outcome.changed = true;
                        }
                    }
                });
            });
            ui.separator();
            outcome.changed |= self.controls(ui, &mut graph.nodes[index].kind, ctx);
        });

        let rect = response.response.rect;

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
        for (i, port) in inputs.iter().enumerate() {
            let centre = Pos2::new(rect.min.x, rect.min.y + PORT_TOP + i as f32 * PORT_SPACING);
            let connected = graph.source_of(id, i as u8).is_some();
            let label = format!("{} ({})", port.name, port.ty.label());
            if self.port(ui, &painter, (id, i as u8), centre, connected, &label) {
                outcome.clicked_input = Some(i as u8);
            }
            outcome.input_ports.push(centre);
        }
        // One socket per output bus, down the right-hand edge.
        for (i, port) in outputs.iter().enumerate() {
            let centre = Pos2::new(rect.max.x, rect.min.y + PORT_TOP + i as f32 * PORT_SPACING);
            let connected = graph
                .links
                .iter()
                .any(|l| l.from == id && l.from_port == i as u8);
            let label = format!("{} ({})", port.name, port.ty.label());
            // Output keys start past the inputs so the two never collide.
            let key = (id, 128 + i as u8);
            if self.port(ui, &painter, key, centre, connected, &label) {
                outcome.clicked_output = Some(i as u8);
            }
            outcome.output_ports.push(centre);
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
    fn controls(&mut self, ui: &mut egui::Ui, kind: &mut NodeKind, ctx: &GraphContext<'_>) -> bool {
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
            NodeKind::DelayRead {
                line,
                ty,
                max_time,
                time,
            } => {
                changed |= line_control(ui, line);
                // The floor of §14.4, in the units the control is in. It is
                // the sub-block size, which the user chose, so it moves when
                // they change that setting — and the value is raised with
                // it rather than the delay quietly running longer than it says.
                let floor = ctx.quantum as f64 / ctx.sample_rate.max(1.0);
                if *time < floor {
                    *time = floor;
                    changed = true;
                }
                ui.horizontal(|ui| {
                    ui.label("time (s)");
                    changed |= ui
                        .add(
                            egui::DragValue::new(time)
                                .speed(0.001)
                                .range(floor..=*max_time),
                        )
                        .changed();
                });
                ui.horizontal(|ui| {
                    ui.label("max (s)");
                    changed |= ui
                        .add(egui::DragValue::new(max_time).speed(0.01).range(0.01..=2.0))
                        .changed();
                });
                if matches!(ty, PortType::Audio { .. }) {
                    ui.weak(format!(
                        "at least {:.1} ms — one sub-block (§14.4)",
                        floor * 1000.0
                    ));
                    ui.weak("wire the time socket to sweep it — the pitch moves with it");
                }
            }
            NodeKind::Plugin { instance, ports } => {
                changed |= self.plugin_controls(ui, *instance, ports, ctx);
            }
            NodeKind::Mix { inputs, gains, .. } => {
                ui.horizontal(|ui| {
                    ui.label("inputs");
                    let mut count = *inputs as u32;
                    // One is allowed, and useful: a mix of one input *is* a
                    // gain, which is what turns a feedback delay's loop down
                    // below unity so it decays.
                    if ui
                        .add(egui::DragValue::new(&mut count).range(1..=8))
                        .changed()
                    {
                        *inputs = count as u8;
                        changed = true;
                    }
                });
                // Grown here rather than at load: a patch saved before the
                // gains existed has none, and every missing one is unity.
                gains.resize(*inputs as usize, 1.0);
                for (i, gain) in gains.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(format!("gain {}", i + 1));
                        changed |= ui
                            .add(egui::DragValue::new(gain).speed(0.005).range(0.0..=2.0))
                            .changed();
                    });
                }
                ui.weak("a gain is used only while its socket is unconnected");
            }
            NodeKind::AudioIn { bus, .. } | NodeKind::AudioOut { bus, .. } => {
                ui.horizontal(|ui| {
                    ui.label("bus");
                    // One-based on screen: the DAW calls them "Main" and
                    // "Sidechain", not "0" and "1".
                    let mut shown = *bus as u32 + 1;
                    if ui
                        .add(egui::DragValue::new(&mut shown).range(1..=2))
                        .changed()
                    {
                        *bus = (shown - 1) as usize;
                        changed = true;
                    }
                    ui.weak(if *bus == 0 { "main" } else { "sidechain" });
                });
            }
            NodeKind::DelayWrite { line, .. } => {
                changed |= line_control(ui, line);
            }
            // A source with nothing to set.
            NodeKind::NoteIn => {}
        }
        changed
    }

    /// A plugin node: what is loaded in it, and its parameter sockets.
    ///
    /// The sockets are the user's choice, not the plugin's (§14.12). A
    /// compressor has ninety parameters and a node with ninety sockets would be
    /// unusable, so they are added one at a time and each one picks what it
    /// drives from a dropdown.
    fn plugin_controls(
        &mut self,
        ui: &mut egui::Ui,
        instance: usize,
        ports: &mut PluginPorts,
        ctx: &GraphContext<'_>,
    ) -> bool {
        let mut changed = false;
        let view = ctx.instances.get(instance).cloned().unwrap_or_default();

        if view.loaded {
            ui.label(egui::RichText::new(&view.name).strong());
        } else {
            ui.colored_label(Color32::from_rgb(200, 140, 60), "not loaded")
                .on_hover_text(
                    "the plugin could not be found, or is still loading. Its links and                      parameter sockets are kept either way (§8.3).",
                );
        }

        ui.horizontal(|ui| {
            if view.loaded {
                if view.editor_open {
                    if ui.small_button("close GUI").clicked() {
                        self.actions.push(GraphAction::CloseSubEditor(instance));
                    }
                } else if ui.small_button("GUI").clicked() {
                    self.actions.push(GraphAction::OpenSubEditor(instance));
                }
            }
            if ui.small_button("+ param").clicked() {
                // The first parameter it has, so a freshly added socket points
                // at something real rather than at nothing.
                let (id, name) = view
                    .params
                    .first()
                    .cloned()
                    .unwrap_or((0, "parameter".to_string()));
                ports.params.push(ParamPort { id, name });
                changed = true;
            }
        });

        let mut remove: Option<usize> = None;
        for (index, param) in ports.params.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                let label = if param.name.is_empty() {
                    format!("#{}", param.id)
                } else {
                    param.name.clone()
                };
                egui::ComboBox::from_id_salt(("param", index))
                    .selected_text(shorten(&label))
                    .width(112.0)
                    .show_ui(ui, |ui| {
                        for (id, name) in &view.params {
                            if ui.selectable_label(*id == param.id, name).clicked()
                                && *id != param.id
                            {
                                param.id = *id;
                                param.name = name.clone();
                                changed = true;
                            }
                        }
                        if view.params.is_empty() {
                            ui.weak("load a plugin to choose");
                        }
                    });
                if ui.small_button("x").clicked() {
                    remove = Some(index);
                }
            });
        }
        if let Some(index) = remove {
            ports.params.remove(index);
            changed = true;
        }
        changed
    }

    /// The "add a node" menu, shown wherever the user asked for it.
    fn add_menu(
        &mut self,
        ui: &mut egui::Ui,
        canvas: Rect,
        graph: &mut Graph,
        ctx: &GraphContext<'_>,
    ) -> bool {
        let Some(at) = self.add_at else { return false };
        let mut added = false;
        let mut close = false;

        egui::Area::new(ui.id().with("add-node"))
            .order(egui::Order::Foreground)
            .fixed_pos(canvas.min + at.to_vec2() + self.pan)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(260.0);

                    // Plugins first: adding one is what this menu is mostly
                    // for, and there is no other way to load one.
                    ui.strong("Plugin");
                    match ctx.free_instance {
                        Some(instance) => {
                            ui.horizontal(|ui| {
                                ui.label("filter");
                                ui.text_edit_singleline(&mut self.plugin_filter);
                            });
                            let needle = self.plugin_filter.to_lowercase();
                            let mut chosen: Option<PathBuf> = None;
                            egui::ScrollArea::vertical()
                                .id_salt("add-plugin")
                                .max_height(220.0)
                                .show(ui, |ui| {
                                    for entry in ctx.plugins {
                                        if !needle.is_empty()
                                            && !entry.name.to_lowercase().contains(&needle)
                                        {
                                            continue;
                                        }
                                        if ui.selectable_label(false, &entry.name).clicked() {
                                            chosen = Some(entry.path.clone());
                                        }
                                    }
                                });
                            if let Some(path) = chosen {
                                // The node appears now and its sockets arrive
                                // when the plugin has finished loading, which
                                // takes hundreds of milliseconds.
                                let node = graph.add(
                                    NodeKind::Plugin {
                                        instance,
                                        ports: PluginPorts::default(),
                                    },
                                    [at.x, at.y],
                                );
                                self.actions.push(GraphAction::LoadPlugin {
                                    node,
                                    instance,
                                    path,
                                });
                                added = true;
                                close = true;
                            }
                        }
                        None => {
                            ui.weak("no free instance — the wrapper is full");
                        }
                    }

                    ui.separator();
                    ui.strong("Delay");
                    ui.weak("two nodes on one line: a write and a read (§14.4)");
                    for (label, ty) in [
                        ("Audio delay", PortType::STEREO),
                        ("Param delay", PortType::Param),
                    ] {
                        if ui.button(label).clicked() {
                            graph.add_delay(ty, [at.x, at.y]);
                            added = true;
                            close = true;
                        }
                    }

                    ui.separator();
                    ui.strong("Node");
                    egui::ScrollArea::vertical()
                        .id_salt("add-kind")
                        .max_height(240.0)
                        .show(ui, |ui| {
                            for (label, kind) in catalogue() {
                                if ui.button(label).clicked() {
                                    graph.add(kind, [at.x, at.y]);
                                    added = true;
                                    close = true;
                                }
                            }
                        });
                    ui.separator();
                    if ui.button("cancel").clicked() {
                        close = true;
                    }
                });
            });

        if close {
            self.add_at = None;
            self.plugin_filter.clear();
        }
        added
    }

    /// Actions the caller has to carry out after the frame.
    pub fn take_actions(&mut self) -> Vec<GraphAction> {
        std::mem::take(&mut self.actions)
    }
}

/// What one node's frame reported back.
#[derive(Default)]
struct NodeOutcome {
    changed: bool,
    remove: bool,
    clicked_input: Option<u8>,
    clicked_output: Option<u8>,
    output_ports: Vec<Pos2>,
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
        (
            "Audio in",
            NodeKind::AudioIn {
                bus: 0,
                channels: 2,
            },
        ),
        (
            "Audio out",
            NodeKind::AudioOut {
                bus: 0,
                channels: 2,
            },
        ),
        ("Note in", NodeKind::NoteIn),
        (
            "Mix",
            NodeKind::Mix {
                channels: 2,
                inputs: 2,
                gains: vec![1.0, 1.0],
            },
        ),
        (
            "Gain",
            NodeKind::Mix {
                channels: 2,
                inputs: 1,
                // Half back round is a delay that decays over a few repeats,
                // which is what a one-input mix is nearly always dropped in to
                // do. It is the same node as the one above — only the starting
                // shape differs, and having both in the menu is cheaper than
                // making the user work that out.
                gains: vec![0.5],
            },
        ),
    ]
}

/// Combo boxes are only so wide, and a parameter name can be long.
fn shorten(text: &str) -> String {
    if text.chars().count() <= 16 {
        return text.to_string();
    }
    text.chars().take(15).collect::<String>() + "\u{2026}"
}

/// Which delay line a half belongs to.
///
/// One-based on screen for the same reason a slot is: the two halves are paired
/// by this number and nothing else, so it has to be readable at a glance.
fn line_control(ui: &mut egui::Ui, line: &mut u32) -> bool {
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
