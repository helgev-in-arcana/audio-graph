//! The sub-plugin as the wrapper sees it.
//!
//! Splits along the same seam as the backend (§4.2): [`SubHost`] is the
//! main-thread half that loads, binds and saves, and it hands out a
//! [`SubHostProcessor`] for the audio thread.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::schedule::SlotSchedule;
use plugin_host::{
    AudioBuffers, AudioConfig, ClassInfo, Event, EventSink, Format, HostContext, ParamEvent,
    ParamId, ParamInfo, Plugin, ProcessStatus, SubPluginMain, SubPluginProcessor, Target,
    TimeContext,
};

use crate::main_thread::MainThread;
use crate::slots::{ResolvedTarget, SlotTable};
use crate::state::WrapperState;
use audio_graph_engine::{InstanceIo, ParamTarget};

pub use crate::state::SubPluginRef;

/// The loaded sub-plugin, plus the slot table that drives it.
///
/// The VST3 objects are main-thread only, which the outer plugin cannot express
/// in its own type — see [`MainThread`].
pub struct SubHost {
    /// One entry per plugin node the graph may address (§14.1). Sparse: a slot
    /// stays empty when its node has been deleted, because the graph names
    /// instances by index and renumbering them would repoint every node.
    instances: Vec<Option<MainThread<Loaded>>>,
    slots: SlotTable,
    context: Arc<dyn HostContext>,
    /// Each instance's latency at its last activate, cached so the DAW can be
    /// answered without touching a plugin (§7.4). The compiler needs these too,
    /// to line up parallel paths (§14.6).
    latencies: Vec<u32>,
}

/// How many plugin nodes one wrapper may host.
///
/// A ceiling rather than guidance: the audio graph names instances by index and
/// the buffer pool is sized at activate, so the number has to be known before
/// the user starts drawing.
pub const MAX_INSTANCES: usize = 16;

/// One loaded sub-plugin, plus how it was found.
///
/// The §5.3 teardown order — editor before instance, instance before module —
/// used to be spelled out here as field order. It now lives inside
/// `plugin_host::Plugin`, which owns all three and is the same shape for both
/// formats, so there is one place to get it right instead of one per caller.
struct Loaded {
    plugin: Plugin,
    reference: SubPluginRef,
}

impl SubHost {
    pub fn new(context: Arc<dyn HostContext>) -> SubHost {
        SubHost {
            instances: Vec::new(),
            slots: SlotTable::default(),
            context,
            latencies: Vec::new(),
        }
    }

    /// Highest instance index in use, plus one. Not a count: the middle may be
    /// empty.
    pub fn instance_count(&self) -> usize {
        self.instances.len()
    }

    fn at(&self, instance: usize) -> Option<&Loaded> {
        self.instances
            .get(instance)
            .and_then(|slot| slot.as_ref())
            .map(MainThread::get)
    }

    fn at_mut(&mut self, instance: usize) -> Option<&mut Loaded> {
        self.instances
            .get_mut(instance)
            .and_then(|slot| slot.as_mut())
            .map(MainThread::get_mut)
    }

    /// Grow the table so `instance` can be written to.
    fn reserve(&mut self, instance: usize) -> Result<(), String> {
        if instance >= MAX_INSTANCES {
            return Err(format!("at most {MAX_INSTANCES} plugin nodes"));
        }
        if self.instances.len() <= instance {
            self.instances.resize_with(instance + 1, || None);
            self.latencies.resize(instance + 1, 0);
        }
        Ok(())
    }

    /// Every instance's latency, indexed the way the graph indexes them.
    ///
    /// Handed to the compiler, which needs it to line up parallel paths and to
    /// know how short a feedback loop may be (§14.6, §14.4).
    pub fn latencies(&self) -> &[u32] {
        &self.latencies
    }

    pub fn slots(&self) -> &SlotTable {
        &self.slots
    }

    pub fn slots_mut(&mut self) -> &mut SlotTable {
        &mut self.slots
    }

    pub fn is_loaded(&self, instance: usize) -> bool {
        self.at(instance).is_some()
    }

    /// Whether anything at all is loaded.
    pub fn any_loaded(&self) -> bool {
        self.instances.iter().any(Option::is_some)
    }

    pub fn sub_latency(&self, instance: usize) -> u32 {
        self.latencies.get(instance).copied().unwrap_or(0)
    }

    /// What is loaded, for display and for saving.
    pub fn reference(&self, instance: usize) -> Option<&SubPluginRef> {
        self.at(instance).map(|l| &l.reference)
    }

    /// What the loaded sub-plugin can accept (§3.3).
    ///
    /// The default — everything false — is also the honest answer when nothing
    /// is loaded: a graph cannot send per-voice modulation to a plugin that is
    /// not there.
    pub fn capabilities(&self, instance: usize) -> plugin_host::Capabilities {
        self.at(instance)
            .map_or_else(Default::default, |l| l.plugin.capabilities())
    }

    /// Hand one instance its own opaque state blob.
    ///
    /// For a harness that wants a plugin to open with a particular patch
    /// already in it — a DAW does this through the normal state path.
    pub fn load_sub_state(&mut self, instance: usize, blob: &[u8]) -> Result<(), String> {
        let loaded = self.at_mut(instance).ok_or("no sub-plugin loaded")?;
        loaded.plugin.load_state(blob).map_err(|e| e.to_string())
    }

    /// The plugin's buses and note capability, for building a node's sockets
    /// (§14.2). Empty when nothing is loaded, which draws as a node with no
    /// sockets rather than as an error.
    pub fn io_layout(&self, instance: usize) -> plugin_host::IoLayout {
        self.at(instance)
            .map(|l| SubPluginMain::io_layout(&l.plugin))
            .unwrap_or_default()
    }

    /// The lowest instance index nothing is loaded into.
    ///
    /// Reused rather than always-increasing so that deleting a plugin node and
    /// adding another does not walk off the end of [`MAX_INSTANCES`].
    pub fn free_instance(&self) -> Option<usize> {
        (0..MAX_INSTANCES).find(|&i| !self.is_loaded(i))
    }

    pub fn params(&self, instance: usize) -> &[ParamInfo] {
        match self.at(instance) {
            Some(l) => SubPluginMain::params(&l.plugin),
            None => &[],
        }
    }

    /// Load a sub-plugin, choosing `class_id` or the module's first offering.
    ///
    /// Replaces whatever was loaded. Slot *bindings* are kept and re-resolved
    /// against the new plugin, so swapping a plugin for a newer version of
    /// itself keeps every mapping (§8.3).
    ///
    /// The format is the path's business, not the caller's: `plugin-host`
    /// reads it off the extension, so a `.clap` and a `.vst3` arrive here the
    /// same way and nothing below this line branches on which it was.
    pub fn load(
        &mut self,
        instance: usize,
        path: &Path,
        class_id: Option<&str>,
    ) -> Result<(), String> {
        self.reserve(instance)?;
        let plugin =
            Plugin::load(path, class_id, Arc::clone(&self.context)).map_err(|e| e.to_string())?;

        let class = plugin.class();
        let reference = SubPluginRef {
            format: class.format.tag().into(),
            plugin_id: class.id.clone(),
            path_hint: path.to_string_lossy().into_owned(),
            display_name: class.name.clone(),
        };

        // Unload only after the new one is known good, so a failed load leaves
        // the user with what they had rather than with nothing.
        self.unload(instance);
        self.slots.resolve_against(
            instance as u32,
            &reference.plugin_id,
            SubPluginMain::params(&plugin),
        );
        self.instances[instance] = Some(MainThread::new(Loaded { plugin, reference }));
        Ok(())
    }

    pub fn unload(&mut self, instance: usize) {
        // Dropping the entry drops the editor first — see the note on `Loaded`.
        if let Some(slot) = self.instances.get_mut(instance) {
            *slot = None;
        }
        if let Some(latency) = self.latencies.get_mut(instance) {
            *latency = 0;
        }
        // Bindings stay; only this instance's resolutions go. The other
        // instances are still loaded and still being driven.
        self.slots.unresolve(instance as u32);
    }

    pub fn unload_all(&mut self) {
        self.instances.clear();
        self.latencies.clear();
        self.slots.unresolve_all();
    }

    /// Re-find a sub-plugin whose recorded path no longer exists.
    ///
    /// Projects move between machines, and a plugin folder that differs by one
    /// directory should not cost the user their patch. The class id is the
    /// authority; the path is only a hint (§8.3).
    pub fn resolve_reference(reference: &SubPluginRef) -> Option<PathBuf> {
        // A reference saved before CLAP existed has no format tag worth
        // trusting beyond `"vst3"`, and one saved by a newer build might name a
        // format this build does not have. Both are "not found" rather than an
        // error, which is what the caller already handles.
        let format = Format::from_tag(&reference.format)?;
        plugin_host::resolve_reference(&plugin_host::PluginRef {
            format,
            id: reference.plugin_id.clone(),
            path_hint: PathBuf::from(&reference.path_hint),
            display_name: reference.display_name.clone(),
        })
    }

    pub fn class(&self, instance: usize) -> Option<&ClassInfo> {
        self.at(instance).map(|l| l.plugin.class())
    }

    /// Open the sub-plugin's own editor in a top-level window (§5.1).
    ///
    /// Not composited into the wrapper's own editor: a native child window
    /// cannot be composited over a GPU surface, so a separate window is the
    /// only workable arrangement — and it is the arrangement that keeps ADR-6
    /// open, since a child process can create its own window with no
    /// cross-process embedding involved.
    /// `owner` is the window the sub-plugin's editor should float above: the
    /// DAW's root window when running as a plugin, null when standalone. An
    /// ownerless window is a peer of the DAW's, so clicking in the DAW buries
    /// it — which is what this argument exists to prevent.
    pub fn open_editor(
        &mut self,
        instance: usize,
        owner: *mut std::ffi::c_void,
    ) -> Result<(), String> {
        let loaded = self.at_mut(instance).ok_or("no sub-plugin loaded")?;
        loaded.plugin.open_editor(owner)
    }

    /// Close the sub-plugin's editor, running the §5.3 sequence.
    pub fn close_editor(&mut self, instance: usize) {
        if let Some(loaded) = self.at_mut(instance) {
            loaded.plugin.close_editor();
        }
    }

    /// Close every open sub-editor. Used on teardown, where §5.3's ordering
    /// matters and no caller should have to remember which ones were open.
    pub fn close_all_editors(&mut self) {
        for instance in 0..self.instances.len() {
            self.close_editor(instance);
        }
    }

    pub fn editor_is_open(&self, instance: usize) -> bool {
        self.at(instance).is_some_and(|l| l.plugin.editor_is_open())
    }

    /// Drive every loaded sub-plugin for one UI tick.
    ///
    /// Call from the host's UI thread, every frame, whether or not any editor
    /// is open — a CLAP plugin's main-thread callbacks and timers run through
    /// here too, and one starved of them stalls. A plugin must not pump
    /// messages itself, since the DAW is already doing that; this only handles
    /// the parts that are ours.
    pub fn tick_editors(&mut self) {
        for instance in 0..self.instances.len() {
            if let Some(loaded) = self.at_mut(instance) {
                loaded.plugin.tick();
            }
        }
    }

    /// Bind a slot to one of a loaded plugin's parameters.
    pub fn bind_slot(
        &mut self,
        instance: usize,
        slot: usize,
        param_id: ParamId,
    ) -> Result<(), String> {
        let loaded = self.at(instance).ok_or("no sub-plugin loaded")?;
        let param = SubPluginMain::params(&loaded.plugin)
            .iter()
            .find(|p| p.id == param_id)
            .ok_or_else(|| format!("no parameter {}", param_id.0))?;
        let plugin_id = loaded.reference.plugin_id.clone();
        let param = param.clone();
        self.slots.bind(slot, instance as u32, &plugin_id, &param);
        Ok(())
    }

    /// Everything driving one instance's parameters, as the audio thread
    /// wants it: `(lane, target)`.
    ///
    /// Two sources, one shape. The slot table is the DAW's automation lanes
    /// (§8), and the lanes past it are parameter sockets the user wired in the
    /// graph (§14.12). The merge in `SubHostProcessor::process` does not care
    /// which is which, and that is the point.
    fn targets_for(
        &self,
        instance: usize,
        graph_params: &[ParamTarget],
    ) -> Vec<(usize, ResolvedTarget)> {
        // Only the slots bound against *this* instance. Handing every instance
        // the whole table would make one slot drive the same parameter on every
        // copy (§12-7).
        let mut targets = self.slots.active_targets(instance as u32);
        let params = self.params(instance);
        for (lane, target) in graph_params.iter().enumerate() {
            if target.instance as usize != instance {
                continue;
            }
            let Some(info) = params.iter().find(|p| p.id.0 == target.param) else {
                // The parameter went away with a plugin update. The socket
                // stays in the graph, the same way an unresolved binding stays
                // in the slot table (§8.3).
                continue;
            };
            targets.push((
                crate::slots::SLOT_COUNT + lane,
                ResolvedTarget {
                    instance: target.instance,
                    id: info.id,
                    min: info.min,
                    max: info.max,
                },
            ));
        }
        targets
    }

    /// Enter the processing phase, activating every loaded instance.
    ///
    /// All or nothing: if one instance refuses the configuration, the ones
    /// already activated are wound back rather than left running, because a
    /// half-activated set is a state no later call knows how to handle.
    ///
    /// `io` says how each instance has to be activated — which buses, how wide
    /// (§14.11). It comes from the compiled program rather than from the
    /// plugin, because whether a sidechain is switched on depends on whether
    /// the graph wired anything to it. An instance the program does not mention
    /// is activated with `config` as it stands, which is the pre-M8 shape.
    pub fn activate(
        &mut self,
        config: AudioConfig,
        io: &[InstanceIo],
        graph_params: &[ParamTarget],
    ) -> Result<SubHostProcessors, String> {
        // One event per slot per sub-block is the worst a graph can ask for,
        // plus whatever the DAW sends us. Reserved here because `process` is
        // not allowed to grow it.
        let sub_blocks = config
            .max_block_size
            .div_ceil(crate::schedule::MIN_QUANTUM)
            .max(1) as usize;
        let capacity = crate::schedule::LANES * sub_blocks + INCOMING_EVENT_CAPACITY;

        let mut processors: Vec<Option<SubHostProcessor>> = Vec::new();
        for instance in 0..self.instances.len() {
            if self.at(instance).is_none() {
                processors.push(None);
                continue;
            }
            // Worked out before the plugin is borrowed for activation,
            // because it reads both the slot table and the plugin's own
            // parameter list.
            let targets = self.targets_for(instance, graph_params);
            let Some(loaded) = self.at_mut(instance) else {
                unreachable!("checked just above")
            };
            // What this instance needs, if the graph routes audio to it.
            let config = match io.iter().find(|e| e.instance as usize == instance) {
                Some(entry) => AudioConfig {
                    input_channels: u32::from(entry.input_channels),
                    output_channels: u32::from(entry.output_channels),
                    aux_inputs: plugin_host_api::AuxBuses::new(&entry.aux_inputs),
                    ..config
                },
                None => config,
            };
            match loaded.plugin.activate(config) {
                Ok(processor) => {
                    let latency = loaded.plugin.latency_samples();
                    self.latencies[instance] = latency;
                    processors.push(Some(SubHostProcessor {
                        processor,
                        targets,
                        last_sent: vec![f64::NAN; crate::schedule::LANES],
                        scratch: Vec::with_capacity(capacity),
                    }));
                }
                Err(e) => {
                    let message = e.to_string();
                    self.deactivate(SubHostProcessors {
                        entries: processors,
                    });
                    return Err(message);
                }
            }
        }

        Ok(SubHostProcessors {
            entries: processors,
        })
    }

    pub fn deactivate(&mut self, processors: SubHostProcessors) {
        for (instance, entry) in processors.entries.into_iter().enumerate() {
            let Some(processor) = entry else { continue };
            if let Some(loaded) = self.at_mut(instance) {
                loaded.plugin.deactivate(processor.processor);
            }
        }
    }

    /// Collect everything that goes into the DAW's project file (§8.3).
    pub fn save_state(&self) -> WrapperState {
        let mut state = WrapperState::new(self.slots.to_state());
        for instance in 0..self.instances.len() {
            let Some(loaded) = self.at(instance) else {
                continue;
            };
            // The wrapper's own state is still worth saving even when a plugin
            // will not give up its own: losing the graph and the bindings as
            // well would turn one plugin's failure into a lost project.
            let bytes = match loaded.plugin.save_state() {
                Ok(bytes) => Some(bytes),
                Err(e) => {
                    log::error!("sub-plugin {instance} state could not be saved: {e}");
                    None
                }
            };
            state.set_instance(instance, loaded.reference.clone(), bytes.as_deref());
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
        self.unload_all();

        for entry in state.instances() {
            let reference = &entry.reference;
            let Some(path) = Self::resolve_reference(reference) else {
                problems.push(format!(
                    "{} could not be found; its slot bindings are kept and will \
                     resolve if it is reinstalled",
                    reference.display_name
                ));
                continue;
            };
            if let Err(e) = self.load(entry.instance, &path, Some(&reference.plugin_id)) {
                problems.push(format!("could not load {}: {e}", reference.display_name));
                continue;
            }
            match entry.state_bytes() {
                Some(bytes) => {
                    if let Some(loaded) = self.at_mut(entry.instance)
                        && let Err(e) = loaded.plugin.load_state(&bytes)
                    {
                        problems.push(format!(
                            "{} loaded but its settings did not restore: {e}",
                            reference.display_name
                        ));
                    }
                }
                None => problems.push(format!(
                    "{} loaded but no settings were saved",
                    reference.display_name
                )),
            }
        }

        problems
    }
}

/// How many of the DAW's own events one block is expected to carry.
///
/// Only used to size the merge buffer. Overshooting costs a few kilobytes;
/// undershooting would cost events, so it is generous.
const INCOMING_EVENT_CAPACITY: usize = 1024;

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
    /// `slots` carries the wrapper's slot values in 0..1 at each sub-block
    /// boundary (§9.2) — the DAW's automation, with anything the node graph
    /// drives written over it. Turning those into parameter events is this
    /// function's whole job: which slot is bound to which parameter, what the
    /// parameter's plain range is, and which values are worth sending at all.
    ///
    /// The two streams are merged in offset order. A sub-plugin is entitled to
    /// assume its input events are sorted, and several real ones misbehave
    /// quietly rather than loudly when they are not.
    ///
    /// `chunk` is the part of the block this call covers (§14.9). It is the
    /// whole block unless an audio delay line put the program on sub-block
    /// granularity, in which case this runs once per sub-block and each call
    /// must be handed *its own* events, rebased: a note at sample 40 belongs to
    /// the chunk starting at 32, at offset 8, and to no other. Handing every
    /// chunk the whole block would replay every note once per chunk.
    pub fn process(
        &mut self,
        buffers: &mut AudioBuffers<'_>,
        slots: &SlotSchedule,
        events: &[Event],
        chunk: Range<u32>,
        context: &TimeContext,
        out_events: &mut EventSink,
    ) -> ProcessStatus {
        self.scratch.clear();
        // Everything before the chunk has already been sent, on an earlier
        // call; everything after it belongs to a later one.
        let events = slice(events, &chunk);
        let mut next_note = 0;

        for index in 0..slots.blocks() {
            let offset = slots.offset(index);
            // Rows outside this chunk are another call's business. The last
            // chunk of a block is short whenever the block is not a multiple of
            // the quantum, so `<` on the end is what keeps the boundary row out
            // of both calls' way rather than in both.
            if offset < chunk.start || offset >= chunk.end.max(chunk.start + 1) {
                continue;
            }
            let offset = offset - chunk.start;

            // Anything the DAW sent that lands before this boundary goes first,
            // so the stream stays sorted.
            while next_note < events.len()
                && events[next_note].sample_offset() - chunk.start < offset
            {
                let event = events[next_note];
                push(
                    &mut self.scratch,
                    event.at_offset(event.sample_offset() - chunk.start),
                );
                next_note += 1;
            }

            let values = slots.block(index);
            for &(slot, target) in &self.targets {
                let Some(&normalized) = values.get(slot) else {
                    continue;
                };
                // Unchanged slots cost nothing. Resending would waste the
                // sub-plugin's parameter queue and, worse, retrigger smoothing
                // on plugins that ramp towards every incoming point.
                if self.last_sent[slot] == normalized {
                    continue;
                }
                self.last_sent[slot] = normalized;
                push(
                    &mut self.scratch,
                    Event::Param(ParamEvent::SetValue {
                        id: target.id,
                        target: Target::Global,
                        value: target.to_plain(normalized),
                        sample_offset: offset,
                    }),
                );
            }
        }

        for &event in &events[next_note..] {
            push(
                &mut self.scratch,
                event.at_offset(event.sample_offset() - chunk.start),
            );
        }

        self.processor
            .process(buffers, &self.scratch, context, out_events)
    }

    pub fn reset(&mut self) {
        self.processor.reset();
        // Force a resend: after a reset the sub-plugin's idea of its parameters
        // is no longer something we can assume.
        self.last_sent.iter_mut().for_each(|v| *v = f64::NAN);
    }
}

/// Every instance's audio-thread half, indexed the way the graph indexes them.
///
/// Sparse for the same reason [`SubHost::instances`] is: the graph names a
/// plugin node by index, so an empty slot has to stay empty.
pub struct SubHostProcessors {
    entries: Vec<Option<SubHostProcessor>>,
}

impl SubHostProcessors {
    pub fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }

    pub fn get_mut(&mut self, instance: usize) -> Option<&mut SubHostProcessor> {
        self.entries.get_mut(instance)?.as_mut()
    }

    pub fn reset(&mut self) {
        for processor in self.entries.iter_mut().flatten() {
            processor.reset();
        }
    }

    /// Bind the per-block context that [`AudioNodes`] does not carry.
    ///
    /// The engine calls `process(instance, ...)` with nothing but buffers,
    /// because it does not know that a plugin has events or a transport
    /// position (§7). Everything else one needs is fixed for the whole block,
    /// so it is attached here and the borrow lasts exactly that long.
    pub fn nodes<'a>(
        &'a mut self,
        slots: &'a SlotSchedule,
        events: &'a [Event],
        context: &'a TimeContext,
        out_events: &'a mut EventSink,
    ) -> GraphNodes<'a> {
        GraphNodes {
            processors: self,
            slots,
            events,
            context,
            out_events,
        }
    }
}

/// [`SubHostProcessors`] with one block's worth of context attached.
pub struct GraphNodes<'a> {
    processors: &'a mut SubHostProcessors,
    slots: &'a SlotSchedule,
    events: &'a [Event],
    context: &'a TimeContext,
    out_events: &'a mut EventSink,
}

impl audio_graph_engine::AudioNodes for GraphNodes<'_> {
    fn process(
        &mut self,
        instance: u32,
        notes: audio_graph_engine::NoteSource,
        input: &[f32],
        output: &mut [f32],
        chunk: audio_graph_engine::AudioChunk,
    ) {
        let Some(processor) = self.processors.get_mut(instance as usize) else {
            // A node whose plugin failed to load, or was deleted while the
            // audio thread held this program. Silence is the only honest
            // answer, and passing the input through would be worse: the user
            // would hear the graph working when it is not.
            for ch in 0..chunk.output_channels {
                output[chunk.channel(ch)].fill(0.0);
            }
            return;
        };

        // The input region holds the main bus and then each aux bus, packed
        // (§14.11); `aux_inputs` is what tells the backend where the joins
        // are. Both regions are already packed at `frames`, which is the layout
        // `AudioBuffers` wants, so nothing is repacked here.
        let mut buffers = AudioBuffers::new(
            input,
            output,
            chunk.input_channels as u32,
            chunk.output_channels as u32,
            chunk.frames,
            plugin_host_api::BufferLayout::Planar,
        )
        .with_aux_inputs(chunk.aux_inputs);
        // §14.10. The engine routes a *name*; turning it into events is this
        // side's job. A node with nothing wired to its notes port hears
        // nothing — which is the whole point, since before M8.3 every instance
        // was handed every event and two synths played in unison.
        let events: &[Event] = match notes {
            audio_graph_engine::NoteSource::Daw { bus: 0 } => self.events,
            _ => &[],
        };
        processor.process(
            &mut buffers,
            self.slots,
            events,
            chunk.offset..chunk.offset + chunk.frames,
            self.context,
            self.out_events,
        );
    }
}

/// The events landing inside `chunk`.
///
/// The stream is sorted, so this is a range rather than a filter — which is
/// what makes it free of allocation and of any per-event work at all on the
/// blocks where every event falls in the first chunk.
fn slice<'a>(events: &'a [Event], chunk: &Range<u32>) -> &'a [Event] {
    let start = events.partition_point(|e| e.sample_offset() < chunk.start);
    let end = events.partition_point(|e| e.sample_offset() < chunk.end);
    &events[start..end.max(start)]
}

/// Append, unless the buffer is full.
///
/// Dropping an event is bad; growing a `Vec` inside an audio callback is worse,
/// and the capacity reserved at activate is the worst case plus a wide margin.
fn push(scratch: &mut Vec<Event>, event: Event) {
    if scratch.len() < scratch.capacity() {
        scratch.push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_host_api::{BufferLayout, NoteEvent, ParamFlags};

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

    fn harness(
        targets: Vec<(usize, ResolvedTarget)>,
    ) -> (
        SubHostProcessor,
        std::sync::Arc<std::sync::Mutex<Vec<Event>>>,
    ) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let processor = SubHostProcessor {
            processor: Box::new(Recorder { seen: seen.clone() }),
            targets,
            last_sent: vec![f64::NAN; crate::slots::SLOT_COUNT],
            scratch: Vec::with_capacity(4096),
        };
        (processor, seen)
    }

    fn run(p: &mut SubHostProcessor, values: &[f64]) {
        let mut schedule = SlotSchedule::new(4, 32);
        schedule.begin(4);
        schedule.fill(values);
        run_scheduled(p, &schedule, &[]);
    }

    fn run_scheduled(p: &mut SubHostProcessor, schedule: &SlotSchedule, events: &[Event]) {
        let input = [0.0f32; 8];
        let mut output = [0.0f32; 8];
        let mut buffers = AudioBuffers::new(&input, &mut output, 2, 2, 4, BufferLayout::Planar);
        let mut sink = EventSink::new();
        p.process(
            &mut buffers,
            schedule,
            events,
            0..schedule.frames(),
            &TimeContext::default(),
            &mut sink,
        );
    }

    #[test]
    fn slot_values_reach_the_sub_plugin_in_plain_units() {
        let target = ResolvedTarget {
            instance: 0,
            id: ParamId(9),
            min: 20.0,
            max: 20_000.0,
        };
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
        let target = ResolvedTarget {
            instance: 0,
            id: ParamId(1),
            min: 0.0,
            max: 1.0,
        };
        let (mut p, seen) = harness(vec![(0, target)]);
        let values = vec![0.25; crate::slots::SLOT_COUNT];

        run(&mut p, &values);
        assert_eq!(seen.lock().unwrap().len(), 1, "first block must send");

        run(&mut p, &values);
        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "unchanged slot should send nothing"
        );

        let mut moved = values.clone();
        moved[0] = 0.75;
        run(&mut p, &moved);
        assert_eq!(seen.lock().unwrap().len(), 2, "a change must send again");
    }

    #[test]
    fn reset_forces_the_next_block_to_resend() {
        let target = ResolvedTarget {
            instance: 0,
            id: ParamId(1),
            min: 0.0,
            max: 1.0,
        };
        let (mut p, seen) = harness(vec![(0, target)]);
        let values = vec![0.25; crate::slots::SLOT_COUNT];

        run(&mut p, &values);
        p.reset();
        run(&mut p, &values);
        assert_eq!(
            seen.lock().unwrap().len(),
            2,
            "state after reset cannot be assumed"
        );
    }

    #[test]
    fn unbound_slots_produce_no_events() {
        let (mut p, seen) = harness(Vec::new());
        run(&mut p, &vec![0.5; crate::slots::SLOT_COUNT]);
        assert!(seen.lock().unwrap().is_empty());
    }

    /// §14.9. A program with an audio feedback loop runs once per sub-block,
    /// and each of those calls is a `process` of its own. An event belongs to
    /// exactly one of them, at an offset measured from *that* call's start —
    /// handing every chunk the whole block would sound each note once per
    /// chunk, and at offsets past the end of a 32-sample buffer.
    #[test]
    fn a_chunk_hears_only_its_own_events_rebased() {
        let (mut p, seen) = harness(Vec::new());
        let note = Event::Note(NoteEvent::NoteOn {
            note_id: -1,
            port: 0,
            channel: 0,
            key: 60,
            velocity: 1.0,
            sample_offset: 40,
        });

        let mut schedule = SlotSchedule::new(128, 32);
        schedule.begin(64);

        let input = [0.0f32; 64];
        let mut output = [0.0f32; 64];
        let mut sink = EventSink::new();
        for chunk in [0..32u32, 32..64] {
            let mut buffers =
                AudioBuffers::new(&input, &mut output, 2, 2, 32, BufferLayout::Planar);
            p.process(
                &mut buffers,
                &schedule,
                &[note],
                chunk,
                &TimeContext::default(),
                &mut sink,
            );
        }

        let notes: Vec<u32> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, Event::Note(_)))
            .map(|e| e.sample_offset())
            .collect();
        assert_eq!(notes, vec![8], "once, in the second chunk, at offset 8");
    }

    /// The parameter side of the same cut: a slot boundary belongs to the chunk
    /// that contains it, and its offset is rebased too.
    #[test]
    fn a_chunk_sends_only_its_own_slot_boundaries() {
        let target = ResolvedTarget {
            instance: 0,
            id: ParamId(3),
            min: 0.0,
            max: 1.0,
        };
        let (mut p, seen) = harness(vec![(0, target)]);

        let mut schedule = SlotSchedule::new(128, 32);
        let blocks = schedule.begin(128);
        for i in 0..blocks {
            schedule.block_mut(i)[0] = i as f64 / 4.0;
        }

        let input = [0.0f32; 64];
        let mut output = [0.0f32; 64];
        let mut sink = EventSink::new();
        let mut buffers = AudioBuffers::new(&input, &mut output, 2, 2, 32, BufferLayout::Planar);
        p.process(
            &mut buffers,
            &schedule,
            &[],
            64..96,
            &TimeContext::default(),
            &mut sink,
        );

        let offsets: Vec<u32> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.sample_offset())
            .collect();
        assert_eq!(offsets, vec![0], "the boundary at 64, seen from 64");
    }

    #[test]
    fn a_moving_slot_is_sent_once_per_sub_block_with_an_offset() {
        // The point of §9.2: a value that changes within a block reaches the
        // sub-plugin as several timed events, not as one at offset zero.
        let target = ResolvedTarget {
            instance: 0,
            id: ParamId(3),
            min: 0.0,
            max: 1.0,
        };
        let (mut p, seen) = harness(vec![(0, target)]);

        let mut schedule = SlotSchedule::new(128, 32);
        let blocks = schedule.begin(128);
        assert_eq!(blocks, 4);
        for i in 0..blocks {
            schedule.block_mut(i)[0] = i as f64 / 4.0;
        }
        run_scheduled(&mut p, &schedule, &[]);

        let events = seen.lock().unwrap().clone();
        let offsets: Vec<u32> = events.iter().map(|e| e.sample_offset()).collect();
        assert_eq!(offsets, vec![0, 32, 64, 96]);
    }

    #[test]
    fn a_slot_that_does_not_move_within_a_block_still_sends_once() {
        let target = ResolvedTarget {
            instance: 0,
            id: ParamId(3),
            min: 0.0,
            max: 1.0,
        };
        let (mut p, seen) = harness(vec![(0, target)]);

        let mut schedule = SlotSchedule::new(128, 32);
        schedule.begin(128);
        schedule.fill(&vec![0.5; crate::slots::SLOT_COUNT]);
        run_scheduled(&mut p, &schedule, &[]);

        assert_eq!(
            seen.lock().unwrap().len(),
            1,
            "four identical sub-blocks are one event"
        );
    }

    #[test]
    fn the_merged_stream_stays_sorted_by_offset() {
        let target = ResolvedTarget {
            instance: 0,
            id: ParamId(3),
            min: 0.0,
            max: 1.0,
        };
        let (mut p, seen) = harness(vec![(0, target)]);

        let mut schedule = SlotSchedule::new(128, 32);
        let blocks = schedule.begin(128);
        for i in 0..blocks {
            schedule.block_mut(i)[0] = i as f64 / 4.0;
        }
        let notes = [
            Event::Note(plugin_host_api::NoteEvent::NoteOn {
                note_id: 1,
                port: 0,
                channel: 0,
                key: 60,
                velocity: 1.0,
                sample_offset: 40,
            }),
            Event::Note(plugin_host_api::NoteEvent::NoteOff {
                note_id: 1,
                port: 0,
                channel: 0,
                key: 60,
                velocity: 0.0,
                sample_offset: 100,
            }),
        ];
        run_scheduled(&mut p, &schedule, &notes);

        let events = seen.lock().unwrap().clone();
        let offsets: Vec<u32> = events.iter().map(|e| e.sample_offset()).collect();
        assert_eq!(offsets, vec![0, 32, 40, 64, 96, 100]);
        assert!(offsets.windows(2).all(|w| w[0] <= w[1]));
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
        table.bind(5, 0, "CID", &param);
        let targets = table.active_targets(0);
        assert_eq!(
            targets,
            vec![(
                5,
                ResolvedTarget {
                    instance: 0,
                    id: ParamId(42),
                    min: 0.0,
                    max: 10.0
                }
            )]
        );
    }

    /// §14.10, the other half: the engine names a source, and this is where the
    /// name becomes events. Before M8.3 both instances were handed the same
    /// list, so a second synth played along whatever the graph said.
    #[test]
    fn only_the_instance_the_graph_wired_hears_the_daws_notes() {
        use audio_graph_engine::{AudioChunk, AudioNodes, NoteSource};
        use plugin_host_api::NoteEvent;

        let (wired, wired_saw) = harness(Vec::new());
        let (idle, idle_saw) = harness(Vec::new());
        let mut processors = SubHostProcessors {
            entries: vec![Some(wired), Some(idle)],
        };

        let note = Event::Note(NoteEvent::NoteOn {
            note_id: -1,
            port: 0,
            channel: 0,
            key: 60,
            velocity: 1.0,
            sample_offset: 0,
        });
        let schedule = SlotSchedule::new(4, 32);
        let mut sink = EventSink::new();
        let context = TimeContext::default();
        let incoming = [note];
        let mut nodes = processors.nodes(&schedule, &incoming, &context, &mut sink);

        let chunk = AudioChunk {
            input_channels: 2,
            output_channels: 2,
            aux_inputs: Default::default(),
            frames: 4,
            offset: 0,
        };
        let input = [0.0f32; 8];
        let mut output = [0.0f32; 8];
        nodes.process(0, NoteSource::Daw { bus: 0 }, &input, &mut output, chunk);
        nodes.process(1, NoteSource::None, &input, &mut output, chunk);

        assert_eq!(
            wired_saw.lock().unwrap().len(),
            1,
            "wired node gets the note"
        );
        assert!(
            idle_saw.lock().unwrap().is_empty(),
            "an unwired notes port must mean silence, not everything"
        );
    }
}
