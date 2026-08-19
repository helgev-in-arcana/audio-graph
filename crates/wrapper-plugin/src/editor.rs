//! The wrapper's own editor: pick a sub-plugin, bind its parameters to slots.
//!
//! Deliberately plain. Its job is to make the two things a user cannot
//! otherwise do — choosing a sub-plugin, and deciding which of its parameters
//! the DAW's automation lanes drive — possible at all, and to make them visible
//! while checking the wrapper in a real DAW. It is not the eventual UI; §9's
//! node graph replaces the slot table with something much richer.
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
//! So the commands go to [`vst3_host_view::Deferred`], which runs them on the
//! next turn of the DAW's message loop, once the frame is over. The same
//! applies to the sub-plugin window's periodic tick, which answers `resizeView`
//! by resizing a window: it runs on a timer, not in the draw callback.
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

use nice_plug::editor::dpi::LogicalSize;
use nice_plug_egui::{EguiEditorState, NiceEguiApp, RepaintNotifier};
use plugin_host_api::{ParamId, ParamInfo};
use subhost_adapter::{MainThread, SLOT_COUNT};

use crate::shared::{Shared, SubState};

/// How often the sub-plugin's window gets its tick, in milliseconds.
///
/// Only resize bookkeeping and the close check happen here (§5.2) — the DAW
/// pumps the actual messages — so 60 Hz is generous, and it costs nothing while
/// no sub-editor is open.
const TICK_MS: u32 = 16;

/// Something the user asked for, to be carried out once the frame is over.
enum Command {
    Load(PathBuf),
    Unload,
    OpenSubEditor,
    CloseSubEditor,
    Bind { slot: usize, param: ParamId },
    ClearSlot(usize),
}

/// A `.vst3` found on disk, before anything has been loaded from it.
///
/// Scanned by filename rather than by opening each module: opening 30 plugins
/// to draw a list would take seconds and, as M2 found, some of them crash and
/// at least one hangs.
struct Entry {
    name: String,
    path: PathBuf,
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
    deferred: Option<MainThread<vst3_host_view::Deferred>>,

    /// Populated on first use, refreshed on demand.
    entries: Vec<Entry>,
    scanned: bool,

    plugin_filter: String,
    param_filter: String,
    /// Which slot the next parameter click binds to. Advanced automatically so
    /// binding several parameters in a row needs no extra clicks.
    next_slot: usize,
    status: Status,

    /// Filled while drawing, drained at the end of the same `ui` call.
    commands: Vec<Command>,
}

impl WrapperEditor {
    pub fn new(shared: Arc<Shared>, repaint: RepaintNotifier) -> WrapperEditor {
        WrapperEditor {
            shared,
            repaint,
            deferred: None,
            entries: Vec::new(),
            scanned: false,
            plugin_filter: String::new(),
            param_filter: String::new(),
            next_slot: 0,
            status: Status::default(),
            commands: Vec::new(),
        }
    }

    fn rescan(&mut self) {
        self.entries.clear();
        for dir in vst3_host::default_plugin_directories() {
            for path in vst3_host::find_modules(&dir) {
                let name = path
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                self.entries.push(Entry { name, path });
            }
        }
        self.entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        self.scanned = true;
    }

    fn sub_plugin_panel(&mut self, ui: &mut egui::Ui, state: &SubState) {
        ui.heading("Sub-plugin");
        ui.horizontal(|ui| match state.host.class() {
            Some(class) => {
                ui.label(egui::RichText::new(&class.name).strong());
                ui.weak(&class.vendor);
            }
            None => {
                ui.weak("none loaded — the wrapper passes audio through");
            }
        });

        if state.host.is_loaded() {
            ui.horizontal(|ui| {
                if state.host.editor_is_open() {
                    if ui.button("Close plugin GUI").clicked() {
                        self.commands.push(Command::CloseSubEditor);
                    }
                } else if ui.button("Open plugin GUI").clicked() {
                    self.commands.push(Command::OpenSubEditor);
                }
                if ui.button("Unload").clicked() {
                    self.commands.push(Command::Unload);
                }
            });
        }

        ui.separator();

        if !self.scanned {
            self.rescan();
        }

        ui.horizontal(|ui| {
            ui.label("Filter");
            ui.text_edit_singleline(&mut self.plugin_filter);
            if ui.button("Rescan").clicked() {
                self.rescan();
            }
        });

        let needle = self.plugin_filter.to_lowercase();
        let mut to_load: Option<PathBuf> = None;
        egui::ScrollArea::vertical().id_salt("plugins").max_height(200.0).show(ui, |ui| {
            for entry in &self.entries {
                if !needle.is_empty() && !entry.name.to_lowercase().contains(&needle) {
                    continue;
                }
                if ui.selectable_label(false, &entry.name).clicked() {
                    to_load = Some(entry.path.clone());
                }
            }
        });
        if let Some(path) = to_load {
            self.commands.push(Command::Load(path));
        }
    }

    fn parameter_panel(&mut self, ui: &mut egui::Ui, state: &SubState) {
        let params: &[ParamInfo] = state.host.params();
        ui.horizontal(|ui| {
            ui.heading("Parameters");
            ui.weak(format!("{}", params.len()));
        });
        if params.is_empty() {
            ui.weak("load a sub-plugin to see its parameters");
            return;
        }

        ui.horizontal(|ui| {
            ui.label("Filter");
            ui.text_edit_singleline(&mut self.param_filter);
            ui.label("bind to slot");
            ui.add(egui::DragValue::new(&mut self.next_slot).range(0..=SLOT_COUNT - 1));
        });

        let needle = self.param_filter.to_lowercase();
        let slot = self.next_slot;
        let mut to_bind: Option<ParamId> = None;
        egui::ScrollArea::vertical().id_salt("params").max_height(280.0).show(ui, |ui| {
            for param in params {
                if !needle.is_empty() && !param.name.to_lowercase().contains(&needle) {
                    continue;
                }
                ui.horizontal(|ui| {
                    if ui.button(format!("-> {slot}")).clicked() {
                        to_bind = Some(param.id);
                    }
                    ui.label(&param.name);
                    ui.weak(format!("[{} .. {}]", param.min, param.max));
                });
            }
        });

        if let Some(param) = to_bind {
            self.commands.push(Command::Bind { slot, param });
            self.next_slot = (slot + 1).min(SLOT_COUNT - 1);
        }
    }

    fn slot_panel(&mut self, ui: &mut egui::Ui, state: &SubState) {
        ui.heading("Slots");
        ui.weak("the DAW automates these; each drives one sub-plugin parameter");

        let table = state.host.slots();
        let params = self.shared.params();
        let mut clear: Option<usize> = None;
        let mut any = false;
        egui::ScrollArea::vertical().id_salt("slots").max_height(420.0).show(ui, |ui| {
            for (i, slot) in table.slots().iter().enumerate() {
                let Some(binding) = &slot.binding else { continue };
                any = true;
                ui.horizontal(|ui| {
                    ui.monospace(format!("slot{i}"));
                    if table.resolved(i).is_some() {
                        ui.label(&binding.param_name);
                    } else {
                        // §8.3: a binding outlives a sub-plugin that cannot be
                        // found, so it has to be shown as such rather than
                        // silently dropped.
                        ui.colored_label(egui::Color32::from_rgb(200, 140, 60), &binding.param_name)
                            .on_hover_text("not resolved against the loaded sub-plugin");
                    }
                    let value = params.slots[i].value.value();
                    ui.add(
                        egui::ProgressBar::new(value)
                            .desired_width(110.0)
                            .text(format!("{value:.3}")),
                    );
                    if ui.small_button("x").clicked() {
                        clear = Some(i);
                    }
                });
            }
            if !any {
                ui.weak("nothing bound yet");
            }
        });

        if let Some(i) = clear {
            self.commands.push(Command::ClearSlot(i));
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
            self.status.set("the editor has no message loop; command dropped");
            self.commands.clear();
            return;
        };
        let commands = std::mem::take(&mut self.commands);
        let shared = self.shared.clone();
        let status = self.status.clone();
        deferred.get().post(move || run(&shared, &status, commands));
    }
}

/// Carry out what the user asked for.
///
/// Runs from the message loop, never from a draw callback — see the module
/// comment for why that distinction is fatal rather than stylistic.
fn run(shared: &Arc<Shared>, status: &Status, commands: Vec<Command>) {
    for command in commands {
        match command {
            Command::Load(path) => {
                let result = shared.lock().load(&path);
                match result {
                    Ok(()) => {
                        shared.store_state();
                        status.set(format!("loaded {}", path.display()));
                    }
                    Err(e) => status.set(format!("load failed: {e}")),
                }
            }
            Command::Unload => {
                shared.lock().unload();
                shared.store_state();
                status.set("unloaded");
            }
            Command::OpenSubEditor => {
                let result = shared.lock().host.open_editor();
                match result {
                    Ok(()) => status.set("plugin GUI open"),
                    Err(e) => status.set(format!("open GUI: {e}")),
                }
            }
            Command::CloseSubEditor => {
                shared.lock().host.close_editor();
                status.set("plugin GUI closed");
            }
            Command::Bind { slot, param } => {
                let result = {
                    let mut state = shared.lock();
                    // The processor reads the resolved targets once, at
                    // activate, so a new binding only reaches audio after a
                    // restart.
                    state.host.bind_slot(slot, param).and_then(|()| state.rebind())
                };
                match result {
                    Ok(()) => {
                        shared.store_state();
                        status.set(format!("slot {slot} bound"));
                    }
                    Err(e) => status.set(format!("bind failed: {e}")),
                }
            }
            Command::ClearSlot(slot) => {
                {
                    let mut state = shared.lock();
                    state.host.slots_mut().clear(slot);
                    let _ = state.rebind();
                }
                shared.store_state();
                status.set(format!("slot {slot} cleared"));
            }
        }
    }
}

impl NiceEguiApp for WrapperEditor {
    fn build(
        &mut self,
        _egui_ctx: egui::Context,
        _nice_gui_ctx: nice_plug::context::gui::GuiContext,
        _frame: &mut nice_plug_egui::Frame,
    ) -> Result<(), nice_plug_egui::baseview::HandlerError> {
        match vst3_host_view::deferred() {
            Ok(deferred) => {
                let shared = self.shared.clone();
                // The sub-plugin's window needs a tick to answer its resize
                // requests and to notice the user closing it (§5.2). It cannot
                // ride on the draw callback: answering a resize means resizing
                // a window, which is precisely what must not happen there.
                deferred.set_tick(TICK_MS, move || {
                    if let Some(mut state) = shared.try_lock() {
                        state.host.tick_editor();
                    }
                });
                self.deferred = Some(MainThread::new(deferred));
            }
            Err(e) => log::warn!("audio-graph: {e}"),
        }
        Ok(())
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut nice_plug_egui::Frame) {
        // Slot values move under automation with no input from the user, so the
        // meters need a repaint even when nothing was clicked.
        self.repaint.request_repaint();

        // `try_lock`, not `lock`. A deferred command holds this while it loads
        // a plugin, and loading dispatches messages, which can ask for a
        // repaint. Waiting here would deadlock against work being done further
        // up this very thread's stack.
        // Cloned so the guard borrows a local rather than `self`; the panels
        // below need `&mut self` for their filter fields.
        let shared = self.shared.clone();
        let Some(state) = shared.try_lock() else {
            ui.weak("busy…");
            return;
        };

        // `columns` rather than a `horizontal` of two `vertical`s: a vertical
        // layout claims all the width that is going, so the second one ends up
        // off the edge of the window entirely.
        ui.columns(2, |cols| {
            self.sub_plugin_panel(&mut cols[0], &state);
            cols[0].add_space(8.0);
            self.parameter_panel(&mut cols[0], &state);
            self.slot_panel(&mut cols[1], &state);
        });

        let status = self.status.get();
        if !status.is_empty() {
            ui.separator();
            ui.weak(status);
        }

        // Released before dispatching: the commands take this lock themselves,
        // from the message loop.
        drop(state);
        self.dispatch();
    }

    fn editor_closed(&mut self) {
        // The wrapper's window is going away, so the sub-plugin's window must
        // too: it is top level with no owner, and leaving it behind strands it
        // with nothing ticking it.
        if let Some(mut state) = self.shared.try_lock() {
            state.host.close_editor();
        }
        // Takes the timer, and anything still queued, with it.
        self.deferred = None;
    }
}

/// The editor's initial size.
pub const EDITOR_SIZE: (f64, f64) = (780.0, 640.0);

/// Build the editor.
pub fn create(shared: Arc<Shared>) -> Option<nice_plug_egui::EguiEditor<WrapperEditor>> {
    let repaint = RepaintNotifier::new();
    let state = EguiEditorState::from_size(LogicalSize::new(EDITOR_SIZE.0, EDITOR_SIZE.1), 1.0);
    let app = WrapperEditor::new(shared, repaint.clone());
    nice_plug_egui::create_egui_editor(
        state,
        repaint,
        nice_plug_egui::EguiNiceSettings::new().with_tile("Audio Graph"),
        app,
    )
}
