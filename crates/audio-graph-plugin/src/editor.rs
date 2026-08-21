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

use nice_plug::editor::ResizeHint;
use nice_plug::editor::dpi::LogicalSize;
use nice_plug_egui::{EguiEditorState, NiceEguiApp, RepaintNotifier};
use plugin_host_api::ParamInfo;
use subhost_adapter::MainThread;

use crate::graph_ui::{GraphContext, GraphEditor};
use crate::shared::Shared;

/// How often the sub-plugin's window gets its tick, in milliseconds.
///
/// Only resize bookkeeping and the close check happen here (§5.2) — the DAW
/// pumps the actual messages — so 60 Hz is generous, and it costs nothing while
/// no sub-editor is open.
const TICK_MS: u32 = 16;

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
    deferred: Option<MainThread<vst3_host_view::Deferred>>,

    /// Populated on first use, refreshed on demand.
    ///
    /// Scanned by filename rather than by opening each module: opening 30
    /// plugins to draw a list would take seconds and, as M2 found, some of them
    /// crash and at least one hangs.
    entries: Vec<crate::graph_ui::PluginEntry>,
    scanned: bool,

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
        self.view.instances = (0..subhost_adapter::MAX_INSTANCES)
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

    fn rescan(&mut self) {
        self.entries.clear();
        for dir in vst3_host::default_plugin_directories() {
            for path in vst3_host::find_modules(&dir) {
                let name = path
                    .file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                self.entries
                    .push(crate::graph_ui::PluginEntry { name, path });
            }
        }
        self.entries
            .sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        self.scanned = true;
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
            });
        }

        if changed {
            self.commands.push(Command::GraphEdited);
        }
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
    /// `vst3_host_view::forward_key`.
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
            vst3_host_view::forward_key(self.daw_window, vk, pressed);
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
        self.daw_window = vst3_host_view::root_window(raw_window(frame)) as usize;

        match vst3_host_view::deferred() {
            Ok(deferred) => {
                let shared = self.shared.clone();
                // The sub-plugin's window needs a tick to answer its resize
                // requests and to notice the user closing it (§5.2). It cannot
                // ride on the draw callback: answering a resize means resizing
                // a window, which is precisely what must not happen there.
                deferred.set_tick(TICK_MS, move || {
                    shared.main().host.tick_editors();
                    // The same turn of the loop is as good a moment as any to
                    // free the programs the audio thread has handed back
                    // (§9.1). Nothing else on the main thread is guaranteed to
                    // run while a patch just sits there playing.
                    shared.reclaim();
                });
                self.deferred = Some(MainThread::new(deferred));
            }
            Err(e) => log::warn!("audio-graph: {e}"),
        }
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
        self.dispatch();
    }

    fn editor_closed(&mut self) {
        // The wrapper's window is going away, so every sub-plugin window must
        // too: they are top level with no owner, and leaving one behind strands
        // it with nothing ticking it.
        self.shared.main().host.close_all_editors();
        // Takes the timer, and anything still queued, with it.
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
