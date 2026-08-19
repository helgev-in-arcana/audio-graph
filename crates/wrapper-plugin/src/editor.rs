//! The wrapper's own editor: pick a sub-plugin, bind its parameters to slots.
//!
//! Deliberately plain. Its job is to make the two things a user cannot
//! otherwise do — choosing a sub-plugin, and deciding which of its parameters
//! the DAW's automation lanes drive — possible at all, and to make them visible
//! while checking the wrapper in a real DAW. It is not the eventual UI; §9's
//! node graph replaces the slot table with something much richer.
//!
//! # Threads
//!
//! `NiceEguiApp` is `Send` because baseview runs the UI on its own thread under
//! X11. On Windows and macOS it runs on the caller's thread, which for a plugin
//! editor is the DAW's main thread — the same thread the sub-plugin's VST3
//! objects are pinned to. Everything here goes through [`Shared`], whose
//! `SubHost` asserts its owning thread on every access, so an X11 port will
//! fail loudly and immediately rather than corrupt anything quietly.

use std::path::PathBuf;
use std::sync::Arc;

use nice_plug::editor::dpi::LogicalSize;
use nice_plug_egui::{EguiEditorState, NiceEguiApp, RepaintNotifier};
use plugin_host_api::{ParamId, ParamInfo};
use subhost_adapter::SLOT_COUNT;

use crate::shared::Shared;

/// A `.vst3` found on disk, before anything has been loaded from it.
///
/// Scanned by filename rather than by opening each module: opening 30 plugins
/// to draw a list would take seconds and, as M2 found, some of them crash.
struct Entry {
    name: String,
    path: PathBuf,
}

pub struct WrapperEditor {
    shared: Arc<Shared>,
    repaint: RepaintNotifier,

    /// Populated on first use, refreshed on demand.
    entries: Vec<Entry>,
    scanned: bool,

    plugin_filter: String,
    param_filter: String,
    /// Which slot the next parameter click binds to. Advanced automatically so
    /// binding several parameters in a row needs no extra clicks.
    next_slot: usize,
    status: String,
}

impl WrapperEditor {
    pub fn new(shared: Arc<Shared>, repaint: RepaintNotifier) -> WrapperEditor {
        WrapperEditor {
            shared,
            repaint,
            entries: Vec::new(),
            scanned: false,
            plugin_filter: String::new(),
            param_filter: String::new(),
            next_slot: 0,
            status: String::new(),
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

    fn sub_plugin_panel(&mut self, ui: &mut egui::Ui) {
        let loaded = {
            let state = self.shared.lock();
            state.host.class().map(|c| (c.name.clone(), c.vendor.clone()))
        };

        ui.heading("Sub-plugin");
        ui.horizontal(|ui| match &loaded {
            Some((name, vendor)) => {
                ui.label(egui::RichText::new(name).strong());
                ui.weak(vendor);
            }
            None => {
                ui.weak("none loaded — the wrapper passes audio through");
            }
        });

        if loaded.is_some() {
            ui.horizontal(|ui| {
                let open = self.shared.lock().host.editor_is_open();
                if open {
                    if ui.button("Close plugin GUI").clicked() {
                        self.shared.lock().host.close_editor();
                    }
                } else if ui.button("Open plugin GUI").clicked() {
                    // The sub-plugin's own window, top level and separate
                    // (§5.1). This is the path a DAW exercises hardest, so it
                    // is one click away rather than buried.
                    if let Err(e) = self.shared.lock().host.open_editor() {
                        self.status = format!("open GUI: {e}");
                    }
                }
                if ui.button("Unload").clicked() {
                    self.shared.lock().unload();
                    self.shared.store_state();
                    self.status = "unloaded".into();
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
            // Loading takes long enough that the audio thread will miss the
            // lock and pass audio through for a moment; see `shared`.
            let result = self.shared.lock().load(&path);
            match result {
                Ok(()) => {
                    self.shared.store_state();
                    self.status = format!("loaded {}", path.display());
                }
                Err(e) => self.status = format!("load failed: {e}"),
            }
        }
    }

    fn parameter_panel(&mut self, ui: &mut egui::Ui) {
        let params: Vec<ParamInfo> = self.shared.lock().host.params().to_vec();
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
            ui.label("→ slot");
            ui.add(egui::DragValue::new(&mut self.next_slot).range(0..=SLOT_COUNT - 1));
        });

        let needle = self.param_filter.to_lowercase();
        let mut to_bind: Option<ParamId> = None;
        egui::ScrollArea::vertical().id_salt("params").max_height(280.0).show(ui, |ui| {
            for param in &params {
                if !needle.is_empty() && !param.name.to_lowercase().contains(&needle) {
                    continue;
                }
                ui.horizontal(|ui| {
                    if ui.button(format!("→{}", self.next_slot)).clicked() {
                        to_bind = Some(param.id);
                    }
                    ui.label(&param.name);
                    ui.weak(format!("[{} .. {}]", param.min, param.max));
                });
            }
        });

        if let Some(id) = to_bind {
            let slot = self.next_slot;
            let result = {
                let mut state = self.shared.lock();
                // The processor reads the resolved targets once, at activate,
                // so a new binding only reaches audio after a restart.
                state.host.bind_slot(slot, id).and_then(|()| state.rebind())
            };
            match result {
                Ok(()) => {
                    self.shared.store_state();
                    self.status = format!("slot {slot} bound");
                    self.next_slot = (slot + 1).min(SLOT_COUNT - 1);
                }
                Err(e) => self.status = format!("bind failed: {e}"),
            }
        }
    }

    fn slot_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Slots");
        ui.weak("the DAW automates these; each drives one sub-plugin parameter");

        let rows: Vec<(usize, Option<String>, bool)> = {
            let state = self.shared.lock();
            let table = state.host.slots();
            table
                .slots()
                .iter()
                .enumerate()
                .map(|(i, slot)| {
                    let label = slot.binding.as_ref().map(|b| b.param_name.clone());
                    (i, label, table.resolved(i).is_some())
                })
                .collect()
        };

        let params = self.shared.params().clone();
        let mut clear: Option<usize> = None;
        let mut any = false;
        egui::ScrollArea::vertical().id_salt("slots").max_height(420.0).show(ui, |ui| {
            for (i, label, resolved) in &rows {
                let Some(label) = label else { continue };
                any = true;
                ui.horizontal(|ui| {
                    ui.monospace(format!("slot{i}"));
                    if *resolved {
                        ui.label(label);
                    } else {
                        // §8.3: a binding outlives a sub-plugin that cannot be
                        // found, so it has to be shown as such rather than
                        // silently dropped.
                        ui.colored_label(egui::Color32::from_rgb(200, 140, 60), label)
                            .on_hover_text("not resolved against the loaded sub-plugin");
                    }
                    let value = params.slots[*i].value.value();
                    ui.add(
                        egui::ProgressBar::new(value)
                            .desired_width(110.0)
                            .text(format!("{value:.3}")),
                    );
                    if ui.small_button("x").clicked() {
                        clear = Some(*i);
                    }
                });
            }
            if !any {
                ui.weak("nothing bound yet");
            }
        });

        if let Some(i) = clear {
            {
                let mut state = self.shared.lock();
                state.host.slots_mut().clear(i);
                let _ = state.rebind();
            }
            self.shared.store_state();
            self.status = format!("slot {i} cleared");
        }
    }
}

impl NiceEguiApp for WrapperEditor {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut nice_plug_egui::Frame) {
        // The sub-plugin's window needs a UI tick to answer its resize requests
        // and to notice the user closing it (§5.2). A plugin must not pump the
        // message loop itself, so this is the only place it can happen: the
        // wrapper editor's own repaint.
        let sub_editor_open = {
            let mut state = self.shared.lock();
            state.host.tick_editor();
            state.host.editor_is_open()
        };
        // Slot values move under automation with no input from the user, so the
        // meters need a repaint even when nothing was clicked.
        self.repaint.request_repaint();
        let _ = sub_editor_open;

        // `columns` rather than a `horizontal` of two `vertical`s: a vertical
        // layout claims all the width that is going, so the second one ends up
        // off the edge of the window entirely.
        ui.columns(2, |cols| {
            self.sub_plugin_panel(&mut cols[0]);
            cols[0].add_space(8.0);
            self.parameter_panel(&mut cols[0]);
            self.slot_panel(&mut cols[1]);
        });

        if !self.status.is_empty() {
            ui.separator();
            ui.weak(self.status.clone());
        }
    }

    fn editor_closed(&mut self) {
        // The wrapper's window is going away, so the sub-plugin's window must
        // too: it is top level with no owner, and leaving it behind strands it
        // with nothing ticking it.
        self.shared.lock().host.close_editor();
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
