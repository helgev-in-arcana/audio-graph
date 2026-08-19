//! The sub-plugin as the wrapper sees it.
//!
//! Splits along the same seam as the backend (§4.2): [`SubHost`] is the
//! main-thread half that loads, binds and saves, and it hands out a
//! [`SubHostProcessor`] for the audio thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use plugin_host_api::{
    AudioBuffers, AudioConfig, Event, EventSink, HostContext, ParamEvent, ParamId, ParamInfo,
    ProcessStatus, SubPluginMain, SubPluginProcessor, Target, TimeContext,
};
use vst3_host::{Cid, ClassInfo, Module, Vst3Plugin};

use vst3_host_view::EditorWindow;

use crate::main_thread::MainThread;
use crate::slots::{ResolvedTarget, SlotTable};
use crate::state::WrapperState;

pub use crate::state::SubPluginRef;

/// The loaded sub-plugin, plus the slot table that drives it.
///
/// The VST3 objects are main-thread only, which the outer plugin cannot express
/// in its own type — see [`MainThread`].
pub struct SubHost {
    loaded: Option<MainThread<Loaded>>,
    slots: SlotTable,
    context: Arc<dyn HostContext>,
    /// Reported by the sub-plugin at its last activate, cached so the DAW can
    /// be answered without touching the plugin (§7.4).
    sub_latency: u32,
}

/// Field order here *is* the teardown order, and §5.3 is entirely about
/// teardown order.
///
/// The editor holds an `IPlugView` created by the controller. VST3 requires
/// that view to be removed from its parent and released before the controller
/// terminates; doing it the other way round leaves the plugin operating on a
/// window that no longer exists, and it faults. Declaring the editor first
/// makes the correct order the one that happens automatically — including on
/// the path §5.3 warns about, where the DAW destroys the whole instance without
/// ever telling us to close the editor.
struct Loaded {
    editor: Option<EditorWindow>,
    /// Released before `module`: the instance must go before the library its
    /// code lives in.
    plugin: Vst3Plugin,
    /// Held, not used: the instance's vtables point into this module's code, so
    /// it must outlive the instance. Dropping it first unloads the library out
    /// from under the plugin.
    #[allow(dead_code)]
    module: Module,
    reference: SubPluginRef,
    class: ClassInfo,
}

impl SubHost {
    pub fn new(context: Arc<dyn HostContext>) -> SubHost {
        SubHost { loaded: None, slots: SlotTable::default(), context, sub_latency: 0 }
    }

    pub fn slots(&self) -> &SlotTable {
        &self.slots
    }

    pub fn slots_mut(&mut self) -> &mut SlotTable {
        &mut self.slots
    }

    pub fn is_loaded(&self) -> bool {
        self.loaded.is_some()
    }

    pub fn sub_latency(&self) -> u32 {
        self.sub_latency
    }

    /// What is loaded, for display and for saving.
    pub fn reference(&self) -> Option<&SubPluginRef> {
        self.loaded.as_ref().map(|l| &l.get().reference)
    }

    pub fn params(&self) -> &[ParamInfo] {
        match &self.loaded {
            Some(l) => SubPluginMain::params(&l.get().plugin),
            None => &[],
        }
    }

    /// Load a sub-plugin, choosing `class_cid` or the module's first audio
    /// module class.
    ///
    /// Replaces whatever was loaded. Slot *bindings* are kept and re-resolved
    /// against the new plugin, so swapping a plugin for a newer version of
    /// itself keeps every mapping (§8.3).
    pub fn load(&mut self, path: &Path, class_cid: Option<Cid>) -> Result<(), String> {
        let module = Module::open(path).map_err(|e| e.to_string())?;
        let classes = module.audio_modules().map_err(|e| e.to_string())?;
        let class = match class_cid {
            Some(cid) => classes
                .into_iter()
                .find(|c| c.cid == cid)
                .ok_or_else(|| format!("{path:?} has no class {cid}"))?,
            None => classes
                .into_iter()
                .next()
                .ok_or_else(|| format!("{path:?} exports no audio module class"))?,
        };

        let plugin = Vst3Plugin::create(&module, class.cid, Arc::clone(&self.context))
            .map_err(|e| e.to_string())?;

        let reference = SubPluginRef {
            format: "vst3".into(),
            plugin_id: class.cid.to_hex(),
            path_hint: path.to_string_lossy().into_owned(),
            display_name: class.name.clone(),
        };

        // Unload only after the new one is known good, so a failed load leaves
        // the user with what they had rather than with nothing.
        self.unload();
        self.slots.resolve_against(&reference.plugin_id, SubPluginMain::params(&plugin));
        self.loaded =
            Some(MainThread::new(Loaded { editor: None, plugin, module, reference, class }));
        Ok(())
    }

    pub fn unload(&mut self) {
        // Dropping `loaded` drops the editor first — see the note on `Loaded`.
        self.loaded = None;
        self.sub_latency = 0;
        // Bindings stay; only their resolution goes.
        self.slots.unresolve_all();
    }

    /// Re-find a sub-plugin whose recorded path no longer exists.
    ///
    /// Projects move between machines, and a plugin folder that differs by one
    /// directory should not cost the user their patch. The class id is the
    /// authority; the path is only a hint (§8.3).
    pub fn resolve_reference(reference: &SubPluginRef) -> Option<PathBuf> {
        let hint = PathBuf::from(&reference.path_hint);
        if hint.exists() {
            return Some(hint);
        }
        let Some(wanted) = Cid::from_hex(&reference.plugin_id) else {
            return None;
        };

        for dir in vst3_host::default_plugin_directories() {
            for candidate in vst3_host::find_modules(&dir) {
                let Ok(module) = Module::open(&candidate) else { continue };
                let Ok(classes) = module.audio_modules() else { continue };
                if classes.iter().any(|c| c.cid == wanted) {
                    return Some(candidate);
                }
            }
        }
        None
    }

    pub fn class(&self) -> Option<&ClassInfo> {
        self.loaded.as_ref().map(|l| &l.get().class)
    }

    /// Open the sub-plugin's own editor in a top-level window (§5.1).
    ///
    /// Not composited into the wrapper's own editor: a native child window
    /// cannot be composited over a GPU surface, so a separate window is the
    /// only workable arrangement — and it is the arrangement that keeps ADR-6
    /// open, since a child process can create its own window with no
    /// cross-process embedding involved.
    pub fn open_editor(&mut self) -> Result<(), String> {
        let loaded = self.loaded.as_mut().ok_or("no sub-plugin loaded")?.get_mut();
        if loaded.editor.is_some() {
            return Ok(());
        }
        let view = loaded.plugin.create_view().ok_or("this plugin has no editor")?;
        loaded.editor = Some(EditorWindow::open(view, &loaded.class.name)?);
        Ok(())
    }

    /// Close the sub-plugin's editor, running the §5.3 sequence.
    pub fn close_editor(&mut self) {
        if let Some(loaded) = self.loaded.as_mut() {
            // Dropping the EditorWindow runs the sequence; there is no way to
            // close one without it.
            loaded.get_mut().editor = None;
        }
    }

    pub fn editor_is_open(&self) -> bool {
        self.loaded.as_ref().is_some_and(|l| l.get().editor.is_some())
    }

    /// Drive the editor for one UI tick: apply pending resizes, and close it if
    /// the user asked.
    ///
    /// Call from the host's UI thread. A plugin must not pump messages itself —
    /// the DAW is already doing that — so this only handles the parts that are
    /// ours.
    pub fn tick_editor(&mut self) {
        let Some(loaded) = self.loaded.as_mut() else { return };
        let loaded = loaded.get_mut();
        let Some(editor) = loaded.editor.as_mut() else { return };

        editor.sync_size();
        if editor.close_requested() {
            loaded.editor = None;
        }
    }

    /// Bind a slot to one of the loaded plugin's parameters.
    pub fn bind_slot(&mut self, slot: usize, param_id: ParamId) -> Result<(), String> {
        let loaded = self.loaded.as_ref().ok_or("no sub-plugin loaded")?.get();
        let param = SubPluginMain::params(&loaded.plugin)
            .iter()
            .find(|p| p.id == param_id)
            .ok_or_else(|| format!("no parameter {}", param_id.0))?;
        let plugin_id = loaded.reference.plugin_id.clone();
        let param = param.clone();
        self.slots.bind(slot, &plugin_id, &param);
        Ok(())
    }

    /// Enter the processing phase.
    pub fn activate(&mut self, config: AudioConfig) -> Result<SubHostProcessor, String> {
        let targets = self.slots.active_targets();
        let loaded = self.loaded.as_mut().ok_or("no sub-plugin loaded")?.get_mut();
        let processor = loaded.plugin.activate(config).map_err(|e| e.to_string())?;
        self.sub_latency = loaded.plugin.latency_samples();

        Ok(SubHostProcessor {
            processor,
            targets,
            last_sent: vec![f64::NAN; crate::slots::SLOT_COUNT],
            scratch: Vec::with_capacity(crate::slots::SLOT_COUNT),
        })
    }

    pub fn deactivate(&mut self, processor: SubHostProcessor) {
        if let Some(loaded) = self.loaded.as_mut() {
            loaded.get_mut().plugin.deactivate(processor.processor);
        }
    }

    /// Collect everything that goes into the DAW's project file (§8.3).
    pub fn save_state(&self) -> WrapperState {
        let mut state = WrapperState::new(self.slots.to_state());
        if let Some(loaded) = self.loaded.as_ref().map(MainThread::get) {
            state.sub_plugin = Some(loaded.reference.clone());
            match loaded.plugin.save_state() {
                Ok(bytes) => state.set_sub_state(&bytes),
                // The wrapper's own state is still worth saving: losing the
                // graph and the bindings as well would turn one plugin's
                // failure into a lost project.
                Err(e) => log::error!("sub-plugin state could not be saved: {e}"),
            }
        }
        state
    }

    /// Restore from saved state, reloading the sub-plugin if it can be found.
    ///
    /// Returns a description of anything that could not be restored, rather
    /// than an error: a missing sub-plugin must not stop the rest of the patch
    /// from loading.
    pub fn load_state(&mut self, state: &WrapperState) -> Vec<String> {
        let mut problems = Vec::new();
        self.slots.load_state(state.slots.clone());

        let Some(reference) = &state.sub_plugin else {
            self.unload();
            return problems;
        };

        match Self::resolve_reference(reference) {
            Some(path) => {
                let cid = Cid::from_hex(&reference.plugin_id);
                if let Err(e) = self.load(&path, cid) {
                    problems.push(format!("could not load {}: {e}", reference.display_name));
                } else if let Some(bytes) = state.sub_state_bytes() {
                    if let Some(loaded) = self.loaded.as_mut() {
                        if let Err(e) = loaded.get_mut().plugin.load_state(&bytes) {
                            problems.push(format!(
                                "{} loaded but its settings did not restore: {e}",
                                reference.display_name
                            ));
                        }
                    }
                } else {
                    problems.push(format!(
                        "{} loaded but no settings were saved",
                        reference.display_name
                    ));
                }
            }
            None => problems.push(format!(
                "{} could not be found; its slot bindings are kept and will \
                 resolve if it is reinstalled",
                reference.display_name
            )),
        }

        problems
    }
}

/// Audio-thread half of the adapter.
pub struct SubHostProcessor {
    processor: Box<dyn SubPluginProcessor>,
    /// Slot index and the parameter it drives, captured at activate so the
    /// audio thread never walks the slot table.
    targets: Vec<(usize, ResolvedTarget)>,
    /// Last value sent per slot, so an unchanged slot costs no event.
    ///
    /// Starts as NaN so the first block always sends: NaN compares unequal to
    /// everything, including itself.
    last_sent: Vec<f64>,
    /// Reused event buffer. Sized at activate; `process` must not allocate.
    scratch: Vec<Event>,
}

impl SubHostProcessor {
    /// Run one block through the sub-plugin.
    ///
    /// `slot_values` are the wrapper's own parameter values in 0..1, straight
    /// from the DAW's automation. In v1 they drive the sub-plugin's parameters
    /// directly (Drive mode, ADR-5); the node graph of M5 goes between them.
    pub fn process(
        &mut self,
        buffers: &mut AudioBuffers<'_>,
        slot_values: &[f64],
        events: &[Event],
        context: &TimeContext,
        out_events: &mut EventSink,
    ) -> ProcessStatus {
        self.scratch.clear();

        // Slot values first, at offset 0, so anything in `events` for this
        // block still takes precedence.
        for &(slot, target) in &self.targets {
            let Some(&normalized) = slot_values.get(slot) else { continue };
            if self.last_sent[slot] == normalized {
                continue;
            }
            self.last_sent[slot] = normalized;
            self.scratch.push(Event::Param(ParamEvent::SetValue {
                id: target.id,
                target: Target::Global,
                value: target.to_plain(normalized),
                sample_offset: 0,
            }));
        }
        self.scratch.extend_from_slice(events);

        self.processor.process(buffers, &self.scratch, context, out_events)
    }

    pub fn reset(&mut self) {
        self.processor.reset();
        // Force a resend: after a reset the sub-plugin's idea of its parameters
        // is no longer something we can assume.
        self.last_sent.iter_mut().for_each(|v| *v = f64::NAN);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_host_api::{BufferLayout, ParamFlags};

    /// A processor that records what it was handed.
    struct Recorder {
        seen: std::sync::Arc<std::sync::Mutex<Vec<Event>>>,
    }

    impl SubPluginProcessor for Recorder {
        fn process(
            &mut self,
            _buffers: &mut AudioBuffers<'_>,
            events: &[Event],
            _context: &TimeContext,
            _out: &mut EventSink,
        ) -> ProcessStatus {
            self.seen.lock().unwrap().extend_from_slice(events);
            ProcessStatus::Continue
        }
        fn reset(&mut self) {}
    }

    fn harness(targets: Vec<(usize, ResolvedTarget)>) -> (SubHostProcessor, std::sync::Arc<std::sync::Mutex<Vec<Event>>>) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let processor = SubHostProcessor {
            processor: Box::new(Recorder { seen: seen.clone() }),
            targets,
            last_sent: vec![f64::NAN; crate::slots::SLOT_COUNT],
            scratch: Vec::with_capacity(crate::slots::SLOT_COUNT),
        };
        (processor, seen)
    }

    fn run(p: &mut SubHostProcessor, values: &[f64]) {
        let input = [0.0f32; 8];
        let mut output = [0.0f32; 8];
        let mut buffers = AudioBuffers::new(&input, &mut output, 2, 2, 4, BufferLayout::Planar);
        let mut sink = EventSink::new();
        p.process(&mut buffers, values, &[], &TimeContext::default(), &mut sink);
    }

    #[test]
    fn slot_values_reach_the_sub_plugin_in_plain_units() {
        let target = ResolvedTarget { id: ParamId(9), min: 20.0, max: 20_000.0 };
        let (mut p, seen) = harness(vec![(0, target)]);
        let mut values = vec![0.0; crate::slots::SLOT_COUNT];
        values[0] = 0.5;
        run(&mut p, &values);

        let events = seen.lock().unwrap().clone();
        assert_eq!(events.len(), 1);
        match events[0] {
            Event::Param(ParamEvent::SetValue { id, value, .. }) => {
                assert_eq!(id, ParamId(9));
                assert_eq!(value, 10_010.0);
            }
            other => panic!("unexpected event {other:?}"),
        }
    }

    #[test]
    fn an_unchanged_slot_sends_nothing_after_the_first_block() {
        // Sending 32 redundant events every block would waste the sub-plugin's
        // parameter queue and, worse, retrigger smoothing on plugins that ramp
        // on every incoming point.
        let target = ResolvedTarget { id: ParamId(1), min: 0.0, max: 1.0 };
        let (mut p, seen) = harness(vec![(0, target)]);
        let values = vec![0.25; crate::slots::SLOT_COUNT];

        run(&mut p, &values);
        assert_eq!(seen.lock().unwrap().len(), 1, "first block must send");

        run(&mut p, &values);
        assert_eq!(seen.lock().unwrap().len(), 1, "unchanged slot should send nothing");

        let mut moved = values.clone();
        moved[0] = 0.75;
        run(&mut p, &moved);
        assert_eq!(seen.lock().unwrap().len(), 2, "a change must send again");
    }

    #[test]
    fn reset_forces_the_next_block_to_resend() {
        let target = ResolvedTarget { id: ParamId(1), min: 0.0, max: 1.0 };
        let (mut p, seen) = harness(vec![(0, target)]);
        let values = vec![0.25; crate::slots::SLOT_COUNT];

        run(&mut p, &values);
        p.reset();
        run(&mut p, &values);
        assert_eq!(seen.lock().unwrap().len(), 2, "state after reset cannot be assumed");
    }

    #[test]
    fn unbound_slots_produce_no_events() {
        let (mut p, seen) = harness(Vec::new());
        run(&mut p, &vec![0.5; crate::slots::SLOT_COUNT]);
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn a_binding_kept_across_a_swap_still_maps_correctly() {
        // §8.3 in one check: bindings are by id, so a plugin that reorders its
        // parameters between versions still drives the right control.
        let mut table = SlotTable::default();
        let param = ParamInfo {
            id: ParamId(42),
            name: "Drive".into(),
            module: String::new(),
            min: 0.0,
            max: 10.0,
            default: 0.0,
            flags: ParamFlags::AUTOMATABLE,
        };
        table.bind(5, "CID", &param);
        let targets = table.active_targets();
        assert_eq!(targets, vec![(5, ResolvedTarget { id: ParamId(42), min: 0.0, max: 10.0 })]);
    }
}
