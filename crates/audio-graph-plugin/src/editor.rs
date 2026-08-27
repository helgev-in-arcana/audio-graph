//! The wrapper's own editor: pick a sub-plugin, bind its parameters to slots.
//!
//! Deliberately plain. Its job is to make the two things a user cannot
//! otherwise do — choosing a sub-plugin, and deciding which of its parameters
//! the DAW's automation lanes drive — possible at all, and to make them visible
//! while checking the wrapper in a real DAW. It is not the eventual UI; §9's
//! node graph (§9) sits alongside it on its own tab: the slot table says
//! *which* sub-plugin parameter a lane drives, the graph says what drives
//! the lane.
//!
//! # The draw callback records; it does not act
//!
//! Every button here pushes a [`Command`] and returns. Nothing that loads a
//! plugin, opens a window or resizes one happens inside `ui()`.
//!
//! That is not tidiness. egui-baseview holds a `RefCell` borrow across the draw
//! callback, and all of those operations dispatch Win32 messages synchronously.
//! The message arrives back inside egui-baseview while the borrow is still live
//! and the process dies — not a panic that unwinds, a
//! `STATUS_STACK_BUFFER_OVERRUN`, because it happens inside a callback that
//! cannot unwind. Clicking "Open plugin GUI" took the host down with it.
//!
//! So the commands go to [`plugin_host::Deferred`], which runs them on the
//! next turn of the DAW's message loop, once the frame is over. The
//! sub-plugin window's periodic tick, which answers `resizeView` by resizing a
//! window, is the same kind of work and is likewise kept out of the draw
//! callback — it runs from the plugin instance's own tick (`crate::tick`),
//! which keeps going whether or not this editor is open.
//!
//! # Threads
//!
//! `NiceEguiApp` is `Send` because baseview runs the UI on its own thread under
//! X11. On Windows and macOS it runs on the caller's thread, which for a plugin
//! editor is the DAW's main thread — the same thread the sub-plugin's VST3
//! objects are pinned to. The main-thread-only pieces are held in
//! [`MainThread`], which asserts the owning thread on every access, so an X11
//! port fails loudly and immediately rather than corrupting anything quietly.

use std::path::PathBuf;
use std::sync::Arc;

use nice_plug::editor::ResizeHint;
use nice_plug::editor::dpi::LogicalSize;
use nice_plug_egui::{EguiEditorState, NiceEguiApp, RepaintNotifier};
use plugin_host::MainThread;
use plugin_host::ParamInfo;

use crate::graph_ui::{GraphContext, GraphEditor};
use crate::shared::Shared;

/// Something the user asked for, to be carried out once the frame is over.
enum Command {
    /// A plugin node was added: load into `instance` and give `node` the
    /// sockets the plugin turns out to have (§14.2).
    LoadPlugin {
        node: audio_graph_engine::NodeId,
        instance: usize,
        path: PathBuf,
    },
    UnloadInstance(usize),
    OpenSub(usize),
    CloseSub(usize),
    /// The graph changed: recompile, publish, save.
    ///
    /// Editing the graph itself happens inline — it touches no window, so
    /// none of the reentrancy above applies — but the work that *follows* an
    /// edit is worth doing once per frame rather than once per mouse-move.
    GraphEdited,
    SetQuantum(u32),
}

/// What the editor draws, refreshed from [`Shared`] rather than read through
/// the lock while drawing.
///
/// Two reasons. Drawing straight from the lock means a frame that cannot take
/// it has nothing to draw, and replacing the whole UI with a "busy" line for one
/// frame is exactly the flicker it looks like. And the parameter list can run to
/// thousands of entries — copying it every frame to satisfy the borrow checker
/// is work nobody asked for.
#[derive(Default)]
struct View {
    /// The [`Shared::generation`] the vectors below were built from.
    generation: u64,
    class: Option<(String, String)>,
    loaded: bool,
    sub_editor_open: bool,
    params: Vec<ParamInfo>,
    /// `(index, parameter name, currently resolved)`.
    slots: Vec<(usize, String, bool)>,
    /// One entry per instance slot, for the plugin nodes to draw themselves
    /// from.
    instances: Vec<crate::graph_ui::InstanceView>,
    free_instance: Option<usize>,
    /// Whether the sub-plugin can take per-voice modulation (§3.3).
    poly_modulation: bool,
}

/// What the last command did, or why it did not.
///
/// Shared rather than owned because the command runs after the frame that asked
/// for it, and its result has to reach the next frame somehow.
#[derive(Default, Clone)]
struct Status(Arc<std::sync::Mutex<String>>);

impl Status {
    fn set(&self, text: impl Into<String>) {
        *self.0.lock().unwrap() = text.into();
    }

    fn get(&self) -> String {
        self.0.lock().unwrap().clone()
    }
}

pub struct WrapperEditor {
    shared: Arc<Shared>,
    repaint: RepaintNotifier,
    /// `None` until `build`: it binds to the message loop of the thread the
    /// editor actually runs on, which is not known until then.
    deferred: Option<MainThread<plugin_host::Deferred>>,

    /// Populated on first use, refreshed on demand.
    ///
    /// Listed by filename from disk and classified from the scan cache, which
    /// is what makes the list appear at once: opening 30 plugins to draw it
    /// would take seconds and, as M2 found, some of them crash and at least one
    /// hangs. The opening happens on `scan`'s thread, and what it learns lands
    /// here when it is done.
    entries: Vec<crate::graph_ui::PluginEntry>,
    scanned: bool,
    /// A scan running on its own thread, if one is. Never more than one: a
    /// second would open the same modules again for the same answer.
    scan: Option<std::sync::mpsc::Receiver<Vec<plugin_host::catalogue::Module>>>,

    /// Whether the plugin-folders window is showing.
    folders_open: bool,
    /// What the user has typed into that window's path field, not yet added.
    folder_input: String,

    status: Status,

    /// Filled while drawing, drained at the end of the same `ui` call.
    commands: Vec<Command>,
    view: View,
    graph_ui: GraphEditor,
    /// The DAW's top-level window, so the sub-plugin's editor can be owned by
    /// it and stay in front. Null until `build`, and when standalone.
    daw_window: usize,
}

impl WrapperEditor {
    pub fn new(shared: Arc<Shared>, repaint: RepaintNotifier) -> WrapperEditor {
        WrapperEditor {
            shared,
            repaint,
            deferred: None,
            entries: Vec::new(),
            scanned: false,
            scan: None,
            folders_open: false,
            folder_input: String::new(),
            status: Status::default(),
            commands: Vec::new(),
            view: View::default(),
            graph_ui: GraphEditor::default(),
            daw_window: 0,
        }
    }

    /// Refresh what gets drawn.
    ///
    /// Cheap fields every time; the vectors only when something actually
    /// changed shape. Rebuilding a two-thousand-entry parameter list sixty
    /// times a second would be silly; noticing a counter is not.
    ///
    /// No lock is involved. Everything read here is main-thread state and the
    /// audio thread has no way to reach it — which is the whole reason having
    /// the editor open no longer costs the audio path anything.
    fn refresh(&mut self) {
        if !self.scanned {
            self.rescan();
        }
        self.collect_scan();
        let state = self.shared.main();

        self.view.class = state
            .host
            .class(0)
            .map(|c| (c.name.clone(), c.vendor.clone()));
        self.view.loaded = state.host.is_loaded(0);
        self.view.sub_editor_open = state.host.editor_is_open(0);
        self.view.poly_modulation = state.host.capabilities(0).poly_modulation;

        let generation = self.shared.generation();
        if generation == self.view.generation && !self.view.params.is_empty() {
            return;
        }
        self.view.generation = generation;
        self.view.params = state.host.params(0).to_vec();

        self.view.free_instance = state.host.free_instance();
        self.view.instances = (0..crate::config::MAX_INSTANCES)
            .map(|i| crate::graph_ui::InstanceView {
                loaded: state.host.is_loaded(i),
                name: state
                    .host
                    .class(i)
                    .map_or_else(String::new, |c| c.name.clone()),
                editor_open: state.host.editor_is_open(i),
                params: state
                    .host
                    .params(i)
                    .iter()
                    .map(|p| (p.id.0, p.name.clone()))
                    .collect(),
            })
            .collect();

        let table = state.host.slots();
        self.view.slots = table
            .slots()
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                let binding = slot.binding.as_ref()?;
                Some((i, binding.param_name.clone(), table.resolved(i).is_some()))
            })
            .collect();
    }

    /// List every plugin module on the machine, both formats together.
    ///
    /// Two halves. The list of modules is paths only, straight off the disk,
    /// and is instantly available. What each module *is* — an effect or an
    /// instrument — can only be had by opening it, so it comes from the scan
    /// cache, and anything the cache does not know yet is `Unknown` until the
    /// background scan started here says otherwise.
    fn rescan(&mut self) {
        self.fill_entries(&plugin_host::catalogue::cached());
        self.scanned = true;
        self.start_scan();
    }

    /// Rebuild the menu's entries from the modules on disk and what `known`
    /// says about them.
    fn fill_entries(&mut self, known: &[plugin_host::catalogue::Module]) {
        let pinned = plugin_host::config::pinned();
        self.entries.clear();
        for (format, path) in plugin_host::installed_modules() {
            let name = path
                .file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            let kind = known
                .iter()
                .find(|m| m.path == path)
                .map_or(plugin_host::catalogue::Kind::Unknown, |m| m.kind());
            let pinned = pinned.contains(&path);
            self.entries.push(crate::graph_ui::PluginEntry {
                name,
                format,
                path,
                pinned,
                kind,
            });
        }
        self.sort_entries();
    }

    /// Pinned first, then by name.
    ///
    /// By name, not by format: a user looking for "Raum" should not have to know
    /// which format it was installed as, and the tag on the row says which one
    /// they are about to load. Pinned plugins are a short list the user wrote
    /// themselves, so they sort among themselves the same way rather than in the
    /// order they happened to be pinned.
    fn sort_entries(&mut self) {
        self.entries
            .sort_by_key(|e| (!e.pinned, e.name.to_lowercase()));
    }

    /// Start bringing the scan cache up to date, on a thread of its own.
    ///
    /// Off the UI thread because it loads third-party code: a plugin that takes
    /// a second to open — or, as M2 found, hangs — must not take the editor
    /// with it. Nothing is shared with it but the channel; it works from the
    /// cache file and hands back what it found.
    fn start_scan(&mut self) {
        if self.scan.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        match std::thread::Builder::new()
            .name("audio-graph plugin scan".into())
            .spawn(move || {
                // Every thread that loads a plugin needs this, and this one
                // loads all of them (§13).
                plugin_host::init_thread();
                let _ = tx.send(plugin_host::catalogue::refresh());
            }) {
            Ok(_) => self.scan = Some(rx),
            Err(e) => self.status.set(format!("scan not started: {e}")),
        }
    }

    /// Take the scan's answer if it has one.
    ///
    /// Polled rather than pushed: the editor already repaints every frame for
    /// the meters, so there is nothing to wake up. A sender dropped without a
    /// value — the thread panicked inside somebody's plugin — clears the slot
    /// so that "Rescan" can try again.
    fn collect_scan(&mut self) {
        let Some(rx) = &self.scan else { return };
        match rx.try_recv() {
            Ok(modules) => {
                self.fill_entries(&modules);
                self.scan = None;
                let unknown = self
                    .entries
                    .iter()
                    .filter(|e| e.kind == plugin_host::catalogue::Kind::Unknown)
                    .count();
                self.status.set(if unknown == 0 {
                    format!("scanned {} modules", self.entries.len())
                } else {
                    format!(
                        "scanned {} modules, {unknown} would not open",
                        self.entries.len()
                    )
                });
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.scan = None;
                self.status.set("the scan did not finish");
            }
        }
    }

    fn graph_panel(&mut self, ui: &mut egui::Ui) {
        // The graph is main-thread state and this is the main thread, so it is
        // edited in place. What must not happen inline is the *consequence* of
        // an edit — see `Command::GraphEdited`.
        let context = GraphContext {
            plugins: &self.entries,
            instances: &self.view.instances,
            free_instance: self.view.free_instance,
            bindings: &self.view.slots,
            poly_modulation: self.view.poly_modulation,
            error: self.shared.main().compile_error.clone(),
            live: self.shared.live_slots(),
            quantum: self.shared.quantum(),
            sample_rate: self.shared.sample_rate() as f64,
        };

        let mut state = self.shared.main();
        let changed = self.graph_ui.ui(ui, &mut state.graph, &context);
        drop(state);

        // Anything the canvas could not do itself: loading a plugin, opening a
        // window. Same reason as everything else here — see the module comment.
        for action in self.graph_ui.take_actions() {
            self.commands.push(match action {
                crate::graph_ui::GraphAction::LoadPlugin {
                    node,
                    instance,
                    path,
                } => Command::LoadPlugin {
                    node,
                    instance,
                    path,
                },
                crate::graph_ui::GraphAction::UnloadInstance(i) => Command::UnloadInstance(i),
                crate::graph_ui::GraphAction::OpenSubEditor(i) => Command::OpenSub(i),
                crate::graph_ui::GraphAction::CloseSubEditor(i) => Command::CloseSub(i),
                // Not a command: writing the config touches no window and
                // dispatches no message, so it is safe inline for the same
                // reason the folders window is.
                crate::graph_ui::GraphAction::PinPlugin { path, pinned } => {
                    if let Err(e) = plugin_host::config::set_pinned(&path, pinned) {
                        log::warn!("audio-graph: the pinned plugins could not be saved: {e}");
                    }
                    for entry in &mut self.entries {
                        if entry.path == path {
                            entry.pinned = pinned;
                        }
                    }
                    self.sort_entries();
                    continue;
                }
            });
        }

        if changed {
            self.commands.push(Command::GraphEdited);
        }
    }

    /// Where sub-plugins are looked for, and the user's say in it.
    ///
    /// Its own window rather than a row in the settings strip: the list is as
    /// long as the user's folders, and the strip has one line.
    ///
    /// Nothing here is deferred. Reading and writing the config file touches no
    /// window and dispatches no messages, so the reentrancy the rest of this
    /// module is built around does not apply — and the rescan it leads to is
    /// already done inline, from `refresh`.
    fn folders_window(&mut self, ctx: &egui::Context) {
        if !self.folders_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Plugin folders")
            .open(&mut open)
            .resizable(true)
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.label(
                    "Every folder scanned for sub-plugins, and one level below each. \
                     This list is the whole of it: nothing is scanned that is not \
                     here. The usual folders for this machine were filled in to \
                     start with, and are yours to remove like any other.",
                );

                // The list scrolls and the controls below it do not: a machine
                // with a lot of plugin folders can outgrow the screen, and an
                // Add button pushed off the bottom is an Add button that does
                // not exist.
                let mut remove = None;
                let directories = plugin_host::config::directories();
                ui.add_space(6.0);
                egui::ScrollArea::vertical()
                    .max_height(260.0)
                    .show(ui, |ui| {
                        if directories.is_empty() {
                            // Reachable, and deliberately not undone on its own:
                            // a user who removed every folder asked for exactly
                            // this, and only the button below puts them back.
                            ui.weak("No folders. Nothing will be scanned.");
                            return;
                        }
                        // `remove` is decided here and acted on after the scroll
                        // area closes: removing inside the loop would edit the
                        // list being drawn from, and `self` is not reachable
                        // from in here anyway.
                        for dir in &directories {
                            ui.horizontal(|ui| {
                                if ui.button("Remove").clicked() {
                                    remove = Some(dir.clone());
                                }
                                let label = ui.label(dir.display().to_string());
                                // A folder on a drive that is not plugged in
                                // stays in the list — it is still what the user
                                // asked for — but saying so beats an empty
                                // plugin menu and no reason for it.
                                if !dir.is_dir() {
                                    label.on_hover_text("this folder is not there right now");
                                    ui.weak("(missing)");
                                }
                            });
                        }
                    });

                if let Some(dir) = remove {
                    match plugin_host::config::remove_directory(&dir) {
                        Ok(()) => {
                            self.status.set(format!("removed {}", dir.display()));
                            self.scanned = false;
                        }
                        Err(e) => self.status.set(format!("settings not saved: {e}")),
                    }
                }

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut self.folder_input)
                            .hint_text("path to a folder of plugins")
                            .desired_width(300.0),
                    );
                    let entered =
                        field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if (ui.button("Add").clicked() || entered)
                        && !self.folder_input.trim().is_empty()
                    {
                        let dir = PathBuf::from(self.folder_input.trim());
                        // Refused rather than saved: a typo that silently
                        // scans nothing is worse than being told.
                        if !dir.is_dir() {
                            self.status
                                .set(format!("{} is not a folder", dir.display()));
                        } else {
                            match plugin_host::config::add_directory(dir.clone()) {
                                Ok(()) => {
                                    self.status.set(format!("added {}", dir.display()));
                                    self.folder_input.clear();
                                    self.scanned = false;
                                }
                                Err(e) => self.status.set(format!("settings not saved: {e}")),
                            }
                        }
                    }
                });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .button("Rescan")
                        .on_hover_text(
                            "open every module again, rather than trusting what was                              found last time",
                        )
                        .clicked()
                    {
                        if let Err(e) = plugin_host::catalogue::forget() {
                            self.status.set(format!("cache not cleared: {e}"));
                        }
                        self.scanned = false;
                        self.status.set("rescanning");
                    }
                    if self.scan.is_some() {
                        ui.weak("scanning…");
                    }
                    // Adds, never replaces: the user's own folders are not what
                    // they asked to undo. Also how a folder that appeared after
                    // the list was first filled in — a format installed since,
                    // a CLAP_PATH set since — gets picked up.
                    if ui
                        .button("Add the usual folders")
                        .on_hover_text(
                            "put back any of this machine's conventional plugin folders \
                             that are not on the list",
                        )
                        .clicked()
                    {
                        match plugin_host::config::restore_defaults() {
                            Ok(()) => {
                                self.status.set("the usual folders are on the list");
                                self.scanned = false;
                            }
                            Err(e) => self.status.set(format!("settings not saved: {e}")),
                        }
                    }
                    ui.weak(format!("{} modules found", self.entries.len()));
                });

                // The settings are shared by every instance in this process and
                // outlive all of them, which is surprising enough to say out
                // loud, and the path is what a user needs to back it up.
                if let Some(path) = plugin_host::config::config_path() {
                    ui.add_space(4.0);
                    ui.weak(format!(
                        "Shared by every Audio Graph instance. Saved in {}",
                        path.display()
                    ));
                }
            });
        self.folders_open = open;
    }

    fn settings_panel(&mut self, ui: &mut egui::Ui) {
        ui.label("Modulation rate").on_hover_text(
            "how often the graph's outputs reach the sub-plugin, in samples. \
             Smaller is smoother and costs more events.",
        );
        let current = self.shared.quantum();
        for choice in subhost_adapter::QUANTUM_CHOICES {
            if ui
                .selectable_label(current == choice, choice.to_string())
                .clicked()
                && current != choice
            {
                self.commands.push(Command::SetQuantum(choice));
            }
        }

        ui.separator();
        if ui
            .selectable_label(self.folders_open, "Plugin folders")
            .on_hover_text("where sub-plugins are looked for")
            .clicked()
        {
            self.folders_open = !self.folders_open;
        }
    }

    /// Hand everything the user clicked to the message loop.
    fn dispatch(&mut self) {
        if self.commands.is_empty() {
            return;
        }
        let Some(deferred) = self.deferred.as_ref() else {
            // No queue means no message loop to defer to, and running these
            // inline is the exact thing this module exists to avoid.
            self.status
                .set("the editor has no message loop; command dropped");
            self.commands.clear();
            return;
        };
        let commands = std::mem::take(&mut self.commands);
        let shared = self.shared.clone();
        let status = self.status.clone();
        let owner = self.daw_window;
        deferred
            .get()
            .post(move || run(&shared, &status, owner, commands));
    }
}

/// Carry out what the user asked for.
///
/// Runs from the message loop, never from a draw callback — see the module
/// comment for why that distinction is fatal rather than stylistic.
fn run(shared: &Arc<Shared>, status: &Status, owner: usize, commands: Vec<Command>) {
    for command in commands {
        // Anything below can change what the editor should be showing, and it
        // draws from a snapshot rather than from the lock.
        shared.changed();
        match command {
            Command::GraphEdited => {
                shared.publish_graph();
                shared.store_state();
                match shared.main().compile_error.clone() {
                    Some(e) => status.set(format!("graph not applied: {e}")),
                    None => status.set("graph applied"),
                }
            }
            Command::SetQuantum(quantum) => {
                shared.set_quantum(quantum);
                shared.store_state();
                status.set(format!("modulation rate {quantum} samples"));
            }
            Command::LoadPlugin {
                node,
                instance,
                path,
            } => {
                match shared.load_into(instance, &path) {
                    Ok(()) => status.set(format!("loaded {}", path.display())),
                    // Not fatal: the node stays, with no sockets and saying so,
                    // the same way an unresolved binding stays (§8.3).
                    Err(e) => status.set(format!("load failed: {e}")),
                }
                // Either way — a plugin that failed to load has no buses, and
                // the node has to stop showing the ones it used to have.
                shared.discover_ports(node);
                shared.store_state();
            }
            Command::UnloadInstance(instance) => {
                shared.unload_instance(instance);
                shared.store_state();
                status.set(format!("instance {} unloaded", instance + 1));
            }
            Command::OpenSub(instance) => {
                let result = shared
                    .main()
                    .host
                    .open_editor(instance, owner as *mut std::ffi::c_void);
                match result {
                    Ok(()) => status.set("plugin GUI open"),
                    Err(e) => status.set(format!("open GUI: {e}")),
                }
            }
            Command::CloseSub(instance) => {
                shared.main().host.close_editor(instance);
                status.set("plugin GUI closed");
            }
        }
    }
}

impl WrapperEditor {
    /// Give the DAW back the keys this editor has no use for.
    ///
    /// The editor window has focus while the user is looking at it, and a child
    /// window is where keyboard messages stop — so without this, the space bar
    /// goes nowhere and the DAW will not start or stop. See
    /// `plugin_host::forward_key`.
    ///
    /// The rule is egui's own: if it wants keyboard input, a text field is
    /// being edited and every key belongs to it. Otherwise the key is ours only
    /// if the canvas has a use for it, which is a short list.
    fn pass_keys_to_the_daw(&self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let forward: Vec<(u16, bool)> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|event| match event {
                    egui::Event::Key {
                        key,
                        pressed,
                        repeat,
                        ..
                    } if !repeat && !ours(*key) => virtual_key(*key).map(|vk| (vk, *pressed)),
                    _ => None,
                })
                .collect()
        });
        for (vk, pressed) in forward {
            plugin_host::forward_key(self.daw_window, vk, pressed);
        }
    }
}

impl NiceEguiApp for WrapperEditor {
    fn build(
        &mut self,
        _egui_ctx: egui::Context,
        _nice_gui_ctx: nice_plug::context::gui::GuiContext,
        frame: &mut nice_plug_egui::Frame,
    ) -> Result<(), nice_plug_egui::baseview::HandlerError> {
        // The window the sub-plugin's editor will be owned by, so it floats
        // above the DAW instead of being buried the moment the user clicks
        // anywhere else. Our own view sits deep inside the DAW's window tree,
        // so walk up to the root — see `ContainerWindow::new` for why it has to
        // be the root and not this view.
        self.daw_window = plugin_host::root_window(raw_window(frame)) as usize;

        match plugin_host::deferred() {
            Ok(deferred) => self.deferred = Some(MainThread::new(deferred)),
            Err(e) => log::warn!("audio-graph: {e}"),
        }
        // No tick is set up here. The sub-plugin's window needs one to answer
        // its resize requests and to notice the user closing it (§5.2), and so
        // does a CLAP sub-plugin's main-thread callback — but both have to keep
        // happening while this window is shut, so the plugin instance owns that
        // timer now (see `crate::tick`).
        Ok(())
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut nice_plug_egui::Frame) {
        self.pass_keys_to_the_daw(ui.ctx());

        // Slot values move under automation with no input from the user, so the
        // meters need a repaint even when nothing was clicked.
        self.repaint.request_repaint();

        // `try_lock`, not `lock`. A deferred command holds this while it loads
        // a plugin, and loading dispatches messages, which can ask for a
        // repaint. Waiting here would deadlock against work being done further
        // up this very thread's stack.
        // Take the lock only to copy out what has changed, never to draw. A
        // frame that cannot get it draws the previous snapshot, which looks
        // like nothing happened — as opposed to blanking the whole window for
        // one frame, which looks like a flicker because it is one.
        self.refresh();

        // One screen: the graph is the editor. A plugin is loaded by adding
        // a node for it, and what a parameter does is said by what is wired to
        // its socket -- so there is nothing left for a second page to hold.
        ui.horizontal(|ui| {
            self.settings_panel(ui);
            ui.separator();
            let status = self.status.get();
            if !status.is_empty() {
                ui.weak(status);
            }
        });
        ui.separator();
        self.graph_panel(ui);
        // After the panels, so that a folder added this frame is scanned on the
        // next one rather than half-applied to this one's plugin menus.
        self.folders_window(ui.ctx());
        self.dispatch();
    }

    fn editor_closed(&mut self) {
        // The wrapper's window is going away, so every sub-plugin window must
        // too: they are top level with no owner, and leaving one behind strands
        // it with nothing ticking it.
        self.shared.main().host.close_all_editors();
        // Takes anything still queued with it.
        self.deferred = None;
    }
}

/// This editor's own platform window handle, or null if it cannot be had.
fn raw_window(frame: &nice_plug_egui::Frame) -> *mut std::ffi::c_void {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = frame.baseview_window().window_handle() else {
        return std::ptr::null_mut();
    };
    match handle.as_raw() {
        RawWindowHandle::Win32(h) => h.hwnd.get() as *mut std::ffi::c_void,
        RawWindowHandle::AppKit(h) => h.ns_view.as_ptr(),
        RawWindowHandle::Xlib(h) => h.window as *mut std::ffi::c_void,
        _ => std::ptr::null_mut(),
    }
}

/// Keys the editor itself acts on, which are therefore not the DAW's.
///
/// Deliberately short. Tab and the arrows move focus between widgets and
/// Escape abandons a half-drawn link; everything else — letters, digits,
/// function keys, and the space bar above all — is a DAW shortcut as far as
/// this editor is concerned.
fn ours(key: egui::Key) -> bool {
    use egui::Key::*;
    matches!(
        key,
        Escape | Tab | ArrowUp | ArrowDown | ArrowLeft | ArrowRight
    )
}

/// An egui key as Windows names it.
///
/// Letters, digits and function keys are contiguous in both, so they need a
/// range each rather than a table. Punctuation is left out on purpose: the
/// Win32 codes for it are OEM keys whose meaning depends on the keyboard
/// layout, and guessing wrong sends the DAW a keystroke the user did not type.
fn virtual_key(key: egui::Key) -> Option<u16> {
    use egui::Key as K;
    Some(match key {
        K::Space => 0x20,
        K::Enter => 0x0d,
        K::Backspace => 0x08,
        K::Delete => 0x2e,
        K::Insert => 0x2d,
        K::Home => 0x24,
        K::End => 0x23,
        K::PageUp => 0x21,
        K::PageDown => 0x22,
        _ => {
            let name = key.name();
            let byte = name.as_bytes();
            match byte {
                // "A".."Z" and "0".."9" share their ASCII value with their
                // virtual key code.
                [c @ b'A'..=b'Z'] => u16::from(*c),
                [c @ b'0'..=b'9'] => u16::from(*c),
                // "F1".."F24" — VK_F1 is 0x70 and they run consecutively.
                [b'F', ..] => {
                    let n: u16 = name[1..].parse().ok()?;
                    if (1..=24).contains(&n) {
                        0x6f + n
                    } else {
                        return None;
                    }
                }
                _ => return None,
            }
        }
    })
}

/// The editor's initial size.
pub const EDITOR_SIZE: (f64, f64) = (780.0, 640.0);

/// Below this the node canvas has no room left to be a canvas.
const EDITOR_MIN_SIZE: (f32, f32) = (620.0, 460.0);

/// Build the editor.
pub fn create(shared: Arc<Shared>) -> Option<nice_plug_egui::EguiEditor<WrapperEditor>> {
    let repaint = RepaintNotifier::new();
    let state = EguiEditorState::from_size(LogicalSize::new(EDITOR_SIZE.0, EDITOR_SIZE.1), 1.0);
    let app = WrapperEditor::new(shared, repaint.clone());
    nice_plug_egui::create_egui_editor(
        state,
        repaint,
        // A node canvas that cannot grow is a node canvas with about four
        // nodes in it. Opting in also matters for what the *host* does: the
        // default hint reports `canResize = false` to VST3, and a DAW that has
        // been told the view is fixed will resize its frame without ever
        // telling the view about it — which is what leaves grey margins down
        // the right and bottom edges rather than more canvas.
        nice_plug_egui::EguiNiceSettings::new()
            .with_tile("Audio Graph")
            .with_resize_hint(
                ResizeHint::resizable()
                    .with_min_logical_size(LogicalSize::new(EDITOR_MIN_SIZE.0, EDITOR_MIN_SIZE.1)),
            ),
        app,
    )
}

#[cfg(test)]
mod key_tests {
    use super::{ours, virtual_key};
    use egui::Key;

    #[test]
    fn the_space_bar_reaches_the_daw() {
        // The whole point: this is the key that starts and stops the transport.
        assert!(!ours(Key::Space));
        assert_eq!(virtual_key(Key::Space), Some(0x20));
    }

    #[test]
    fn letters_digits_and_function_keys_map_to_their_win32_codes() {
        assert_eq!(virtual_key(Key::A), Some(0x41));
        assert_eq!(virtual_key(Key::Z), Some(0x5a));
        assert_eq!(virtual_key(Key::Num0), Some(0x30));
        assert_eq!(virtual_key(Key::Num9), Some(0x39));
        assert_eq!(virtual_key(Key::F1), Some(0x70));
        assert_eq!(virtual_key(Key::F12), Some(0x7b));
    }

    #[test]
    fn keys_with_no_layout_independent_code_are_not_guessed() {
        // Punctuation is an OEM key on Windows and its meaning moves with the
        // keyboard layout. Sending nothing is better than sending a keystroke
        // the user did not type.
        assert_eq!(virtual_key(Key::Semicolon), None);
        assert_eq!(virtual_key(Key::Backslash), None);
        // Beyond VK_F24 there is nothing to map onto.
        assert_eq!(virtual_key(Key::F35), None);
    }

    #[test]
    fn the_keys_the_canvas_uses_stay_here() {
        for key in [Key::Escape, Key::Tab, Key::ArrowUp, Key::ArrowLeft] {
            assert!(ours(key), "{key:?} should not be forwarded");
        }
    }
}
