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

use audio_graph_engine::{
    Graph, NODE_WIDTH, NodeAction, NodeGroup, NodeId, NodeKind, NodeUi, Plugin, PluginPorts,
    PortType, catalogue,
};

/// Re-exported so the wrapper fills one in without naming two crates. It is
/// the engine's type: what a plugin node draws is the engine's business now.
pub use audio_graph_engine::InstanceView;
use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};
use subhost_adapter::SLOT_COUNT;

/// What each kind of socket is painted.
///
/// A socket's type decides what may be plugged into it, and until it was
/// coloured the only way to find that out was to hover — or to try, and have
/// the link silently refused. Aux inputs get a colour of their own for the
/// same reason: a sidechain that looks like the main input is one people wire
/// into by mistake, and the mistake makes no sound.
mod socket {
    use egui::Color32;
    pub const AUDIO: Color32 = Color32::from_rgb(96, 170, 240);
    pub const AUX: Color32 = Color32::from_rgb(190, 130, 235);
    pub const PARAM: Color32 = Color32::from_rgb(128, 200, 120);
    pub const NOTE: Color32 = Color32::from_rgb(232, 176, 80);
}

/// The colour of one socket, and of the links leaving it.
fn socket_colour(port: &audio_graph_engine::Port) -> Color32 {
    match port.ty {
        PortType::Audio { .. } if port.aux => socket::AUX,
        PortType::Audio { .. } => socket::AUDIO,
        PortType::Param => socket::PARAM,
        PortType::Note => socket::NOTE,
    }
}

/// Vertical space between a node's top edge and its first port.
const PORT_TOP: f32 = 26.0;
const PORT_SPACING: f32 = 18.0;
const PORT_RADIUS: f32 = 5.0;
const GRID: f32 = 24.0;

/// A plugin module found on disk, offered in the add-node menu.
pub struct PluginEntry {
    /// The file name, extension and all — which is also how the user recognises
    /// it, since a module's own name is only knowable by loading it.
    pub name: String,
    pub format: plugin_host::Format,
    pub path: PathBuf,
    /// Whether the user pinned it to the top of the list. Carried on the entry
    /// rather than read from the config per row, because the menu draws every
    /// frame and the config is behind a lock.
    pub pinned: bool,
    /// Effect or instrument, as the scan cache has it — `Unknown` until this
    /// module has been opened once.
    pub kind: plugin_host::catalogue::Kind,
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
    /// Pin or unpin a module in the add-node menu. The canvas cannot do it
    /// itself because the answer outlives the frame: it is written to the
    /// config and it reorders the list the menu is reading.
    PinPlugin {
        path: PathBuf,
        pinned: bool,
    },
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

/// How far the canvas may be zoomed. Past the low end the text stops being
/// text; past the high end one node fills the window.
const ZOOM_RANGE: std::ops::RangeInclusive<f32> = 0.4..=2.0;

/// Canvas state that belongs to the view rather than to the patch.
pub struct GraphEditor {
    pan: Vec2,
    /// Canvas scale. Applied to positions *and* to the style the nodes are
    /// drawn with, so a zoomed-out node is a smaller node rather than the same
    /// node with its text spilling out of it.
    ///
    /// Not part of the patch: two people opening the same project should each
    /// get their own view, the same way the pan is each their own.
    zoom: f32,
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

impl Default for GraphEditor {
    fn default() -> GraphEditor {
        GraphEditor {
            pan: Vec2::ZERO,
            zoom: 1.0,
            dragging: None,
            linking: None,
            add_at: None,
            plugin_filter: String::new(),
            actions: Vec::new(),
        }
    }
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

        // Ctrl+wheel zooms about the pointer: whatever is under the cursor
        // stays under it, which is what makes zooming feel like moving closer
        // rather than like the patch sliding away.
        //
        // `zoom_delta`, not the scroll delta: egui takes the wheel away from
        // scrolling the moment ctrl is held and hands it over as a zoom factor
        // instead, so the scroll delta this first read was always zero. It has
        // applied the exponential already — a notch is the same proportion at
        // every scale — and a trackpad pinch arrives the same way.
        //
        // Asked of the pointer rather than of the canvas `Response`: over a
        // node it is the node's own widgets that are hovered, and that is
        // where zooming has to work most of all.
        if let Some(pointer) = ui
            .ctx()
            .pointer_latest_pos()
            .filter(|p| canvas.contains(*p))
        {
            let step = ui.input(|i| i.zoom_delta());
            if step != 1.0 {
                let before = self.zoom;
                self.zoom = (before * step).clamp(*ZOOM_RANGE.start(), *ZOOM_RANGE.end());
                // The graph point under the pointer must not move:
                //   pointer = canvas.min + pan + graph * zoom
                let anchor = pointer - canvas.min - self.pan;
                self.pan -= anchor * (self.zoom / before - 1.0);
            }
        }
        let zoom = self.zoom;

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
        let mut ports: Vec<Placed> = Vec::with_capacity(graph.nodes.len());
        let mut to_remove: Option<NodeId> = None;
        let mut to_connect: Option<(NodeId, u8, NodeId, u8)> = None;
        let mut to_disconnect: Option<(NodeId, u8)> = None;

        for index in 0..graph.nodes.len() {
            let id = graph.nodes[index].id;
            let pos =
                origin + Vec2::new(graph.nodes[index].pos[0], graph.nodes[index].pos[1]) * zoom;
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
            ports.push(Placed {
                id,
                outputs: outcome.output_ports,
                output_colours: outcome.output_colours,
                inputs: outcome.input_ports,
            });
        }

        // Drawing the links, now that every node has been placed.
        let mut shapes: Vec<egui::Shape> = Vec::with_capacity(graph.links.len() + 1);
        for link in &graph.links {
            let from = ports.iter().find(|p| p.id == link.from);
            let to = ports.iter().find(|p| p.id == link.to);
            if let (Some(from), Some(to)) = (from, to)
                && let Some(&source) = from.outputs.get(link.from_port as usize)
                && let Some(&target) = to.inputs.get(link.to_port as usize)
            {
                let colour = from
                    .output_colours
                    .get(link.from_port as usize)
                    .copied()
                    .unwrap_or_else(|| ui.visuals().weak_text_color());
                shapes.push(link_shape(source, target, colour.gamma_multiply(0.85)));
            }
        }
        if let Some((from, from_port)) = self.linking
            && let Some(node) = ports.iter().find(|p| p.id == from)
            && let Some(&source) = node.outputs.get(from_port as usize)
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
            if let Some(NodeKind::Plugin(Plugin { instance, .. })) = graph.node(id).map(|n| &n.kind)
            {
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
            self.add_at =
                Some(Pos2::ZERO + (pointer - self.pan - canvas.min.to_vec2()).to_vec2() / zoom);
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
                self.zoom = 1.0;
            }
            // Both an indicator and the way back: ctrl+wheel is not a gesture
            // anyone finds unless they already expect it to be there.
            if ui
                .button(format!("{:.0}%", self.zoom * 100.0))
                .on_hover_text("ctrl+wheel over the canvas to zoom — click to go back to 100%")
                .clicked()
            {
                self.zoom = 1.0;
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
        let step = (GRID * self.zoom).max(6.0);
        let stroke = Stroke::new(1.0, Color32::from_gray(60).gamma_multiply(0.5));
        let offset = Vec2::new(self.pan.x.rem_euclid(step), self.pan.y.rem_euclid(step));
        let mut x = canvas.min.x + offset.x - step;
        while x < canvas.max.x {
            painter.vline(x, canvas.y_range(), stroke);
            x += step;
        }
        let mut y = canvas.min.y + offset.y - step;
        while y < canvas.max.y {
            painter.hline(canvas.x_range(), y, stroke);
            y += step;
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
        // Not `title()`: a plugin node is named for what is loaded in it,
        // and only the wrapper knows what that is.
        let title = graph.nodes[index].kind.ui_title(&node_ui(ctx));

        let zoom = self.zoom;
        let width = NODE_WIDTH * zoom;
        let body = Rect::from_min_size(pos, Vec2::new(width, 0.0));
        // The drag handle is registered before the node's contents so that the
        // widgets inside win the pointer. egui resolves a click to the last
        // widget registered over it, and the title bar is both the handle and
        // the home of the delete button.
        let handle = Rect::from_min_size(pos, Vec2::new(width, PORT_TOP * zoom));
        let drag = ui.interact(handle, ui.id().with(("node", id)), Sense::drag());

        let mut child = ui.new_child(
            egui::UiBuilder::new()
                // Without a salt of its own, every node's contents land in the
                // same id namespace: two nodes of a kind then share one combo
                // box's open/closed state and one button's click, and the
                // second one drawn wins. Node id makes each node its own world.
                .id_salt(("graph-node", id))
                .max_rect(Rect::from_min_size(body.min, Vec2::new(width, 4000.0)))
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );
        // Clipped to the canvas so a node dragged off the edge does not paint
        // over the panels around it.
        child.set_clip_rect(canvas);
        // Zooming the *style* rather than only the geometry is what makes a
        // zoomed node a smaller node instead of the same node with its text
        // hanging out of it.
        if zoom != 1.0 {
            child.set_style(zoomed_style(ui.style(), zoom));
        }

        // A socket's circle is painted on the node's edge, centred on it, so
        // the outer half of it lies over the node's own contents. Anything
        // narrower than the whole circle leaves a socket name with a hole
        // punched through it. `Frame::group` hardcodes 6 and does not scale
        // with the zoom, which is the other half of the same problem.
        let margin = PORT_RADIUS * 2.0 * zoom;
        let frame = egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(margin.round().clamp(1.0, 127.0) as i8))
            .fill(ui.visuals().panel_fill)
            .stroke(Stroke::new(
                1.0,
                ui.visuals().widgets.noninteractive.bg_stroke.color,
            ));

        // Where each socket's row ended up, so the circles can be painted on
        // the node's edge at exactly the height of the row beside them. A row
        // is now a socket *and* the control that stands in for it, so this is
        // also what keeps the circle centred on a two-line control.
        let mut input_rows: Vec<f32> = Vec::with_capacity(inputs.len());
        let mut output_rows: Vec<f32> = Vec::with_capacity(outputs.len());

        // Which inputs already have a link, worked out before the closure
        // borrows the graph mutably. A fed socket is one whose fallback
        // control has nothing left to say, and greying it out where it sits
        // says so better than a line of prose under the node.
        let connected: Vec<bool> = (0..inputs.len())
            .map(|i| graph.source_of(id, i as u8).is_some())
            .collect();

        // What the node's own controls asked the wrapper for, and which socket
        // the user asked to take away. Both are carried out after the closure
        // has given the graph back.
        let mut actions: Vec<NodeAction> = Vec::new();
        let mut dropped: Option<u8> = None;
        let mut dropped_output: Option<u8> = None;

        let response = frame.show(&mut child, |ui| {
            ui.set_width(width - 2.0 * margin);
            // The whole bar is laid out right to left, so it reads name ·
            // GUI · always on · x on screen. The buttons are placed first and
            // the name takes what is left, because the other way round a
            // plugin called "audio-graph CLAP test plugin" pushed every button
            // off the node.
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Delete sits on the outside because it is the one that
                    // cannot be taken back, and because that is where every
                    // window has put it for thirty years.
                    if ui
                        .small_button("x")
                        .on_hover_text("delete this node")
                        .clicked()
                    {
                        outcome.remove = true;
                    }
                    // Only worth offering where it changes anything: a node
                    // with no output is already compiled.
                    if !outputs.is_empty() {
                        let on = &mut graph.nodes[index].always_on;
                        // Framed in both states. A `toggle_value` that is off
                        // draws as bare text, which in a title bar beside a
                        // node's name is not something anybody reads as a
                        // control until they happen to hover it.
                        if ui
                            .add(
                                egui::Button::new("always on")
                                    .small()
                                    .selected(*on)
                                    .frame(true)
                                    .frame_when_inactive(true),
                            )
                            .on_hover_text(
                                "run this node even with nothing wired to its output \
                                 — for analysers",
                            )
                            .clicked()
                        {
                            *on = !*on;
                            outcome.changed = true;
                        }
                    }
                    let mut cx = node_ui(ctx);
                    outcome.changed |= graph.nodes[index].kind.title_controls(ui, &mut cx);
                    actions.append(&mut cx.actions);
                    // The name fills what the buttons left, laid out the
                    // other way round again so it starts at the node's left
                    // edge instead of hugging them.
                    //
                    // Truncated rather than wrapped: a plugin name that grew
                    // the title bar to two lines would move every socket on
                    // the node down with it, and the full name is a hover
                    // away.
                    let rest = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
                    ui.allocate_ui_with_layout(
                        rest,
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.add(
                                egui::Label::new(egui::RichText::new(&title).strong())
                                    .truncate()
                                    .selectable(false),
                            )
                            .on_hover_text(&title);
                        },
                    );
                });
            });
            ui.separator();

            // The node's own settings sit directly under the title, and
            // nothing below them moves when they change. They used to sit
            // between the sockets, which was fine while only the inputs could
            // grow — now that outputs can too, a drop-down that slides up and
            // down as sockets are added is a control you have to find again
            // every time.
            let body = ui.scope(|ui| {
                let mut cx = node_ui(ctx);
                let changed = graph.nodes[index].kind.controls(ui, &mut cx);
                actions.append(&mut cx.actions);
                changed
            });
            outcome.changed |= body.inner;
            // Measured rather than asked, because only the node knows whether
            // it drew anything and a separator over nothing is a line across
            // an empty node.
            let drew_body = body.response.rect.height() > 1.0;
            let add_output_label = graph.nodes[index].kind.add_output_label();
            if drew_body && (!outputs.is_empty() || add_output_label.is_some()) {
                ui.separator();
            }

            // Outputs above inputs, the way Blender's nodes read: what this
            // node produces first, what it needs after. The names are here
            // rather than only in a tooltip because a socket you have to hover
            // to identify is one you get wrong before you ever hover it.
            for (i, port) in outputs.iter().enumerate() {
                let row = ui
                    .horizontal(|ui| {
                        // Right to left, so the name is against the socket and
                        // the control sits beside it rather than adrift in the
                        // middle of the node. The remove button is placed
                        // first for the same reason it is on an input row: it
                        // takes its width before the control is offered what
                        // is left.
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(port.name.as_ref());
                            if port.removable
                                && ui
                                    .small_button("x")
                                    .on_hover_text("remove this output")
                                    .clicked()
                            {
                                dropped_output = Some(i as u8);
                            }
                            let mut cx = node_ui(ctx);
                            outcome.changed |=
                                graph.nodes[index].kind.output_control(ui, i as u8, &mut cx);
                            actions.append(&mut cx.actions);
                        });
                    })
                    .response
                    .rect;
                output_rows.push(row.center().y);
            }

            // The button that makes more of them, under the sockets it adds
            // to, and against the same edge: an output row runs to the right,
            // so the button that grows the list does too.
            //
            // "+" rather than "+ out": what it adds is the row it sits under,
            // and the word was the wider half of a button nobody needs to read
            // twice. The name it would have carried is the tooltip.
            if let Some(label) = add_output_label {
                // Inside a `horizontal`, not a bare `with_layout`. The node's
                // ui is given four thousand pixels of height to lay out in, and
                // a right-to-left layout asked to centre in that takes all of
                // it: the node grew a blank column taller than the canvas.
                // `horizontal` is what shrinks the row back to its content.
                let clicked = ui
                    .horizontal(|ui| {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.small_button("+").on_hover_text(label).clicked()
                        })
                        .inner
                    })
                    .inner;
                if clicked {
                    graph.nodes[index].kind.add_output();
                    outcome.changed = true;
                }
            }

            let add_label = graph.nodes[index].kind.add_input_label();
            if (!outputs.is_empty() || add_output_label.is_some() || drew_body)
                && (!inputs.is_empty() || add_label.is_some())
            {
                ui.separator();
            }

            for (i, port) in inputs.iter().enumerate() {
                let wired = connected.get(i).copied().unwrap_or(false);
                let row = ui
                    .horizontal(|ui| {
                        // Right to left, like the title bar and for the same
                        // reason: the remove button takes its width first, and
                        // what is left is what the control may have. A control
                        // that asks `available_width` then gets an honest
                        // answer, rather than one that ignores the button and
                        // runs under it.
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if port.removable {
                                // Outside whatever `input_control` disabled,
                                // on purpose: a socket with a link in it is
                                // still one you may want gone, and taking it
                                // away is exactly what cuts the link.
                                if ui
                                    .small_button("x")
                                    .on_hover_text("remove this input")
                                    .clicked()
                                {
                                    dropped = Some(i as u8);
                                }
                            }
                            let rest =
                                egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
                            ui.allocate_ui_with_layout(
                                rest,
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    ui.label(port.name.as_ref());
                                    let mut cx = node_ui(ctx);
                                    outcome.changed |= graph.nodes[index]
                                        .kind
                                        .input_control(ui, i as u8, wired, &mut cx);
                                    actions.append(&mut cx.actions);
                                },
                            );
                        });
                    })
                    .response
                    .rect;
                input_rows.push(row.center().y);
            }

            // The last row, under the sockets it makes more of, and left
            // against the edge its sockets are on.
            if let Some(label) = add_label
                && ui.small_button("+").on_hover_text(label).clicked()
            {
                graph.nodes[index].kind.add_input();
                outcome.changed = true;
            }
        });

        // A socket the user took away. The node has already shrunk itself; all
        // that is left is the graph's half — cutting what was plugged in, and
        // sliding every later socket's link down so it still means the socket
        // it meant before.
        if let Some(port) = dropped {
            let count = graph.nodes[index].kind.remove_input(port);
            if count > 0 {
                graph.drop_inputs(id, port, count);
                outcome.changed = true;
            }
        }
        if let Some(port) = dropped_output {
            let count = graph.nodes[index].kind.remove_output(port);
            if count > 0 {
                graph.drop_outputs(id, port, count);
                outcome.changed = true;
            }
        }
        for action in actions {
            self.actions.push(match action {
                NodeAction::OpenSubEditor(instance) => GraphAction::OpenSubEditor(instance),
                NodeAction::CloseSubEditor(instance) => GraphAction::CloseSubEditor(instance),
            });
        }

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
                let target = (pointer - grab - self.pan - canvas.min.to_vec2()) / zoom;
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

        // Ports, painted on the node's edges at the height of their rows.
        let painter = ui.painter_at(canvas);
        for (i, port) in inputs.iter().enumerate() {
            let y = input_rows
                .get(i)
                .copied()
                .unwrap_or(rect.min.y + (PORT_TOP + i as f32 * PORT_SPACING) * zoom);
            let centre = Pos2::new(rect.min.x, y);
            let connected = graph.source_of(id, i as u8).is_some();
            let socket = Socket {
                colour: socket_colour(port),
                connected,
                name: &format!("{} ({})", port.name, port.ty.label()),
            };
            if self.port(ui, &painter, (id, i as u8), centre, socket) {
                outcome.clicked_input = Some(i as u8);
            }
            outcome.input_ports.push(centre);
        }
        for (i, port) in outputs.iter().enumerate() {
            let y = output_rows
                .get(i)
                .copied()
                .unwrap_or(rect.min.y + (PORT_TOP + i as f32 * PORT_SPACING) * zoom);
            let centre = Pos2::new(rect.max.x, y);
            let connected = graph
                .links
                .iter()
                .any(|l| l.from == id && l.from_port == i as u8);
            let colour = socket_colour(port);
            let socket = Socket {
                colour,
                connected,
                name: &format!("{} ({})", port.name, port.ty.label()),
            };
            // Output keys start past the inputs so the two never collide.
            let key = (id, 128 + i as u8);
            if self.port(ui, &painter, key, centre, socket) {
                outcome.clicked_output = Some(i as u8);
            }
            // The link takes the colour of the socket it leaves, so a cable can
            // be followed across a crowded canvas without tracing it.
            outcome.output_ports.push(centre);
            outcome.output_colours.push(colour);
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
        socket: Socket<'_>,
    ) -> bool {
        let Socket {
            colour,
            connected,
            name,
        } = socket;
        let radius = PORT_RADIUS * self.zoom;
        let hit = Rect::from_center_size(centre, Vec2::splat(radius * 3.0));
        let response = ui.interact(hit, ui.id().with(("port", which)), Sense::click());
        // The colour says what the socket takes; filled against hollow says
        // whether anything is in it. Two facts, two channels — dimming the
        // colour to mean "empty" would have made them one.
        if connected {
            painter.circle_filled(centre, radius, colour);
        } else {
            painter.circle(
                centre,
                radius,
                ui.visuals().extreme_bg_color,
                Stroke::new(1.6, colour),
            );
        }
        if response.hovered() {
            painter.circle_stroke(
                centre,
                radius + 2.5,
                Stroke::new(1.0, ui.visuals().strong_text_color()),
            );
            response.clone().on_hover_text(name);
        }
        response.clicked()
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
            .fixed_pos(canvas.min + at.to_vec2() * self.zoom + self.pan)
            .show(ui.ctx(), |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    // Two columns, each scrolling on its own: the built-in
                    // nodes are a fixed list one learns by heart, the plugins
                    // are however many are installed, and scrolling one to the
                    // bottom should not push the other out of reach.
                    ui.set_width(540.0);
                    let mut chosen: Option<PathBuf> = None;
                    let mut pin: Option<(PathBuf, bool)> = None;

                    ui.columns(2, |cols| {
                        let ui = &mut cols[0];
                        ui.strong("Node");
                        egui::ScrollArea::vertical()
                            .id_salt("add-kind")
                            .max_height(360.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.weak("Delay");
                                ui.horizontal_wrapped(|ui| {
                                    // A wrapped row leaves no gap between lines
                                    // unless asked: square it with the gap
                                    // between buttons.
                                    ui.spacing_mut().item_spacing.y = ui.spacing().item_spacing.x;
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
                                });

                                // Wrapped rather than one per row, and under a
                                // heading per kind of wire: the flat list had
                                // grown past the point where a reader could
                                // find anything in it without reading all of
                                // it, and "which sort of thing is this" is the
                                // question being asked while the menu is open.
                                let entries = catalogue();
                                for group in NodeGroup::ALL {
                                    // A group with nothing in it — as `Plugin`
                                    // and the delays are, being added elsewhere
                                    // — gets no heading rather than an empty
                                    // one.
                                    if !entries.iter().any(|(g, _, _)| *g == group) {
                                        continue;
                                    }
                                    ui.weak(group.label());
                                    ui.horizontal_wrapped(|ui| {
                                        ui.spacing_mut().item_spacing.y =
                                            ui.spacing().item_spacing.x;
                                        for (g, label, kind) in &entries {
                                            if *g != group {
                                                continue;
                                            }
                                            if ui.button(*label).clicked() {
                                                graph.add(kind.clone(), [at.x, at.y]);
                                                added = true;
                                                close = true;
                                            }
                                        }
                                    });
                                }
                            });

                        let ui = &mut cols[1];
                        ui.strong("Plugin");
                        match ctx.free_instance {
                            Some(_) => {
                                ui.horizontal(|ui| {
                                    ui.label("filter");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.plugin_filter)
                                            .desired_width(f32::INFINITY),
                                    );
                                });
                                // Under the filter rather than over it: the
                                // filter applies to both tabs, and typing is
                                // what the user does first.
                                let needle = self.plugin_filter.to_lowercase();
                                egui::ScrollArea::vertical()
                                    .id_salt("add-plugin")
                                    .max_height(360.0)
                                    // The list fills the column: left to shrink
                                    // to its content, the scrollbar sat mid-way
                                    // across the popup instead of at its edge.
                                    .auto_shrink([false, true])
                                    .show(ui, |ui| {
                                        // Padded so a long name never runs
                                        // under the scrollbar.
                                        egui::Frame::new()
                                            .inner_margin(egui::Margin::symmetric(10, 0))
                                            .show(ui, |ui| {
                                                let mut shown = 0usize;
                                                for entry in ctx.plugins {
                                                    // The format is searchable too, so
                                                    // typing "clap" narrows the list to
                                                    // one format without a separate
                                                    // control.
                                                    if !needle.is_empty()
                                                        && !entry
                                                            .name
                                                            .to_lowercase()
                                                            .contains(&needle)
                                                        && !entry
                                                            .format
                                                            .tag()
                                                            .contains(needle.as_str())
                                                    {
                                                        continue;
                                                    }
                                                    match plugin_row(ui, entry) {
                                                        RowHit::Load => {
                                                            chosen = Some(entry.path.clone());
                                                        }
                                                        RowHit::TogglePin => {
                                                            pin = Some((
                                                                entry.path.clone(),
                                                                !entry.pinned,
                                                            ));
                                                        }
                                                        RowHit::Nothing => {}
                                                    }
                                                    shown += 1;
                                                }
                                                if shown == 0 {
                                                    ui.weak("nothing here");
                                                }
                                            });
                                    });
                            }
                            None => {
                                ui.weak("no free instance — the wrapper is full");
                            }
                        }
                    });

                    if let (Some(path), Some(instance)) = (chosen, ctx.free_instance) {
                        // The node appears now and its sockets arrive when the
                        // plugin has finished loading, which takes hundreds of
                        // milliseconds.
                        let node = graph.add(
                            NodeKind::Plugin(Plugin {
                                instance,
                                ports: PluginPorts::default(),
                            }),
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

                    if let Some((path, pinned)) = pin {
                        self.actions.push(GraphAction::PinPlugin { path, pinned });
                    }

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

/// What clicking somewhere in a plugin's row asked for.
#[derive(PartialEq, Eq)]
enum RowHit {
    Nothing,
    /// The row itself: load this plugin.
    Load,
    /// The star at its right end: pin it, or unpin it if it was pinned.
    TogglePin,
}

/// One plugin in the add-node menu's list.
///
/// The format tag leads the row at a fixed width so every name starts at the
/// same x: trailing tags left the names ragged, and a name too long for the
/// column wraps under itself rather than pushing its tag off the edge. The pin
/// is at the far right, out of the path of the click that loads the plugin.
fn plugin_row(ui: &mut egui::Ui, entry: &PluginEntry) -> RowHit {
    const TAG_WIDTH: f32 = 38.0;
    const PIN_WIDTH: f32 = 18.0;

    // A point over `small`: small alone read as a footnote next to the name.
    let tag_size = egui::TextStyle::Small.resolve(ui.style()).size + 1.0;

    let width = ui.available_width();
    // Reserved now, painted once the row's height is known — a hover
    // highlight has to go behind text that has not been laid out yet.
    let bg = ui.painter().add(egui::Shape::Noop);
    let mut pinned = None;
    let response = ui
        .allocate_ui_with_layout(
            egui::vec2(width, 0.0),
            egui::Layout::left_to_right(egui::Align::TOP),
            |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let line = ui.text_style_height(&egui::TextStyle::Body);
                ui.add_sized(
                    [TAG_WIDTH, line],
                    egui::Label::new(
                        egui::RichText::new(entry.format.tag())
                            .weak()
                            .size(tag_size),
                    )
                    .selectable(false),
                );
                // The pin is placed before the name and laid out from the right,
                // so that the name — the one part that wraps — is what gives on
                // a narrow column.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                    let star = if entry.pinned { "★" } else { "☆" };
                    let pin = ui.add_sized(
                        [PIN_WIDTH, line],
                        egui::Button::new(egui::RichText::new(star).size(tag_size)).frame(false),
                    );
                    // Unpinned stars are drawn on every row, so say what they do
                    // rather than leaving a column of decoration.
                    let pin = pin.on_hover_text(if entry.pinned {
                        "unpin"
                    } else {
                        "pin to the top"
                    });
                    pinned = Some(pin.clicked());
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::TOP), |ui| {
                        ui.add(egui::Label::new(&entry.name).wrap().selectable(false));
                    });
                });
            },
        )
        .response
        .interact(Sense::click());

    if response.hovered() {
        ui.painter().set(
            bg,
            egui::Shape::rect_filled(
                response.rect,
                ui.visuals().widgets.hovered.corner_radius,
                ui.visuals().widgets.hovered.weak_bg_fill,
            ),
        );
    }

    // The pin wins: it sits inside the row, so a click on it is a click on the
    // row too, and loading the plugin is not what the user asked for.
    if pinned == Some(true) {
        RowHit::TogglePin
    } else if response.clicked() {
        RowHit::Load
    } else {
        RowHit::Nothing
    }
}

/// The facts a node's own controls are handed.
///
/// Built fresh at each call rather than once per node, because it also carries
/// what those controls asked the wrapper to do and every caller drains its own
/// — a node's title bar, its body and each of its socket rows are three
/// separate asks.
fn node_ui<'a>(ctx: &'a GraphContext<'a>) -> NodeUi<'a> {
    NodeUi {
        slot_count: SLOT_COUNT,
        bindings: ctx.bindings,
        live: &ctx.live,
        poly_modulation: ctx.poly_modulation,
        quantum: ctx.quantum,
        sample_rate: ctx.sample_rate,
        instances: ctx.instances,
        actions: Vec::new(),
    }
}

/// One socket, as far as painting it is concerned.
struct Socket<'a> {
    colour: Color32,
    connected: bool,
    /// The long form, with the type in it — the tooltip. The short name is
    /// drawn inside the node, beside the circle.
    name: &'a str,
}

/// Where one node's sockets ended up on screen, for the links to find.
struct Placed {
    id: NodeId,
    outputs: Vec<Pos2>,
    /// One per output, so a link can take the colour of the socket it leaves.
    output_colours: Vec<Color32>,
    inputs: Vec<Pos2>,
}

/// What one node's frame reported back.
#[derive(Default)]
struct NodeOutcome {
    changed: bool,
    remove: bool,
    clicked_input: Option<u8>,
    clicked_output: Option<u8>,
    output_ports: Vec<Pos2>,
    output_colours: Vec<Color32>,
    input_ports: Vec<Pos2>,
}

/// `base` with every size in it multiplied by `zoom`.
///
/// egui has a zoom of its own, but it is global: it would scale the panels and
/// the toolbar along with the canvas, which is not what a node editor's zoom
/// means. Scaling a `Style` and handing it to the node's own `Ui` keeps it to
/// the canvas.
fn zoomed_style(base: &egui::Style, zoom: f32) -> egui::Style {
    let mut style = base.clone();
    for font in style.text_styles.values_mut() {
        font.size *= zoom;
    }
    let s = &mut style.spacing;
    s.item_spacing *= zoom;
    s.button_padding *= zoom;
    s.menu_margin *= zoom;
    s.indent *= zoom;
    s.interact_size *= zoom;
    s.slider_width *= zoom;
    s.combo_width *= zoom;
    s.text_edit_width *= zoom;
    s.icon_width *= zoom;
    s.icon_width_inner *= zoom;
    s.icon_spacing *= zoom;
    style
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One egui context, one canvas, and a clock — a canvas that can be driven
    /// without a window.
    ///
    /// egui runs perfectly well headless: the output is a list of shapes nobody
    /// has to paint. Worth doing here because the input plumbing is exactly
    /// what was wrong the first time — the zoom read `smooth_scroll_delta`,
    /// which egui zeroes the moment ctrl is held, so it did nothing and nothing
    /// said so.
    ///
    /// The context is held across frames rather than made fresh each time. It
    /// is where egui smooths wheel input, and a wheel event delivered to a
    /// context that is thrown away before the next frame produces no motion at
    /// all — which is its own way of testing nothing.
    struct Canvas {
        ctx: egui::Context,
        editor: GraphEditor,
        graph: Graph,
        time: f64,
    }

    impl Canvas {
        fn new() -> Canvas {
            Canvas {
                ctx: egui::Context::default(),
                editor: GraphEditor::default(),
                graph: Graph::default_patch(),
                time: 0.0,
            }
        }

        /// One frame, 1/60 s after the last, with `events` delivered to it.
        fn frame(&mut self, events: Vec<egui::Event>) {
            self.time += 1.0 / 60.0;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 700.0))),
                time: Some(self.time),
                predicted_dt: 1.0 / 60.0,
                events,
                ..Default::default()
            };
            let editor = &mut self.editor;
            let graph = &mut self.graph;
            let output = self.ctx.run_ui(input, |ui| {
                let context = GraphContext {
                    plugins: &[],
                    instances: &[],
                    free_instance: Some(0),
                    bindings: &[],
                    poly_modulation: false,
                    error: None,
                    live: [0.0; SLOT_COUNT],
                    quantum: 32,
                    sample_rate: 48_000.0,
                };
                editor.ui(ui, graph, &context);
            });
            output.drop_without_applying_deltas();
        }

        /// A wheel notch over the canvas, and the frames it takes egui to smooth
        /// it out.
        fn wheel(&mut self, lines: f32, modifiers: egui::Modifiers) {
            // The pointer has to be over the canvas, and `pointer_latest_pos`
            // is only set once a frame has seen it move there.
            self.frame(vec![egui::Event::PointerMoved(Pos2::new(450.0, 450.0))]);
            self.frame(vec![egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Line,
                delta: Vec2::new(0.0, lines),
                phase: egui::TouchPhase::Move,
                modifiers,
            }]);
            for _ in 0..30 {
                self.frame(Vec::new());
            }
        }
    }

    #[test]
    fn ctrl_wheel_zooms_the_canvas() {
        let mut canvas = Canvas::new();
        assert_eq!(canvas.editor.zoom, 1.0);

        canvas.wheel(3.0, egui::Modifiers::COMMAND);
        let inned = canvas.editor.zoom;
        assert!(inned > 1.0, "zoomed in, factor {inned}");

        canvas.wheel(-6.0, egui::Modifiers::COMMAND);
        assert!(
            canvas.editor.zoom < 1.0,
            "and back out past where it started, factor {}",
            canvas.editor.zoom
        );
    }

    /// Without ctrl the same wheel is a scroll, and the canvas leaves it alone.
    #[test]
    fn a_plain_wheel_does_not_zoom() {
        let mut canvas = Canvas::new();
        canvas.wheel(3.0, egui::Modifiers::NONE);
        assert_eq!(canvas.editor.zoom, 1.0);
    }

    /// §14.16. The point under the pointer is the one that must not move, which
    /// means the pan has to take up the difference.
    #[test]
    fn zooming_holds_the_point_under_the_pointer() {
        let mut canvas = Canvas::new();
        assert_eq!(canvas.editor.pan, Vec2::ZERO);
        canvas.wheel(3.0, egui::Modifiers::COMMAND);
        assert!(canvas.editor.zoom > 1.0);
        assert_ne!(
            canvas.editor.pan,
            Vec2::ZERO,
            "the pan compensated for the growth"
        );
        // Zooming in about a point below and right of the origin pulls the
        // patch up and left, so the pan goes negative in both.
        assert!(canvas.editor.pan.x < 0.0 && canvas.editor.pan.y < 0.0);
    }

    /// The zoom is clamped, and the clamp is not a place the pan can get stuck:
    /// spinning the wheel at the limit must not keep moving the view.
    #[test]
    fn the_zoom_stops_at_the_limit() {
        let mut canvas = Canvas::new();
        for _ in 0..12 {
            canvas.wheel(6.0, egui::Modifiers::COMMAND);
        }
        assert_eq!(canvas.editor.zoom, *ZOOM_RANGE.end());
        let settled = canvas.editor.pan;
        canvas.wheel(6.0, egui::Modifiers::COMMAND);
        assert_eq!(canvas.editor.pan, settled, "nothing moves at the limit");
    }
}
