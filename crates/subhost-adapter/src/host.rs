//! Sub-plugin lifecycle and hosting abstraction.
//!
//! Separates main-thread operations ([`SubHost`] for loading, parameter binding,
//! state persistence, and editor management) from real-time audio-thread processing
//! ([`SubHostProcessor`] and [`SubHostProcessors`]).

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::schedule::SlotSchedule;
use plugin_host::{
    AudioBuffers, AudioConfig, ClassInfo, Event, EventSink, Format, HostContext, MainThread,
    ParamEvent, ParamId, ParamInfo, Plugin, ProcessStatus, SubPluginMain, SubPluginProcessor,
    Target, TimeContext,
};

use crate::instances::{InstanceIo, ParamTarget};
use crate::slots::{ResolvedTarget, SlotTable};
use crate::state::{InstanceState, SubHostState};

pub use crate::state::SubPluginRef;

/// Main-thread manager for loaded sub-plugin instances and their parameter slot bindings.
///
/// Plugin objects are restricted to the main thread and wrapped in [`MainThread`].
pub struct SubHost {
    config: SubHostConfig,
    /// Sparse list of loaded sub-plugin instances indexed by instance ID.
    /// Empty entries stay empty rather than being closed up: callers name an
    /// instance by index, so renumbering would repoint every one of them.
    instances: Vec<Option<MainThread<Loaded>>>,
    slots: SlotTable,
    context: Arc<dyn HostContext>,
    /// Latency in samples for each instance as of its last activation, cached
    /// so the DAW can be answered without touching a plugin. Callers that run
    /// instances in parallel need these to line the paths up.
    latencies: Vec<u32>,
}

/// Configuration limits and buffer sizing parameters for a sub-host.
///
/// These are ceilings rather than guidance: everything below is preallocated.
/// The instance table and the event buffers are sized at activate, and
/// `process` may not grow either, because it runs on the audio thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubHostConfig {
    /// Maximum number of sub-plugin instances that may be hosted.
    pub max_instances: usize,
    /// Number of parameter slots published to the host DAW.
    pub slot_count: usize,
    /// Number of values carried per sub-block in the [`SlotSchedule`]: the
    /// slots plus whatever else the caller packs alongside them.
    pub lanes: usize,
}

/// A loaded sub-plugin instance and its reference metadata.
///
/// Teardown order — editor before instance, instance before module — lives
/// inside `plugin_host::Plugin`, which owns all three, so dropping this struct
/// is enough to get it right.
struct Loaded {
    plugin: Plugin,
    reference: SubPluginRef,
}

impl SubHost {
    pub fn new(context: Arc<dyn HostContext>, config: SubHostConfig) -> SubHost {
        SubHost {
            instances: Vec::new(),
            slots: SlotTable::new(config.slot_count),
            config,
            context,
            latencies: Vec::new(),
        }
    }

    pub fn config(&self) -> SubHostConfig {
        self.config
    }

    /// Upper bound on active instance indices: the length of the sparse
    /// instance list, not a count of what is loaded — the middle may be empty.
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

    /// Expands the instance table if needed to accommodate `instance`.
    fn reserve(&mut self, instance: usize) -> Result<(), String> {
        if instance >= self.config.max_instances {
            let max = self.config.max_instances;
            return Err(format!("at most {max} sub-plugins"));
        }
        if self.instances.len() <= instance {
            self.instances.resize_with(instance + 1, || None);
            self.latencies.resize(instance + 1, 0);
        }
        Ok(())
    }

    /// Returns the cached latency in samples for all instances, indexed by
    /// instance ID.
    ///
    /// A caller that runs instances in parallel needs these to line the paths
    /// up; one that runs them in a loop needs them to know how short the loop
    /// may be.
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

    /// Returns `true` if at least one sub-plugin instance is loaded.
    pub fn any_loaded(&self) -> bool {
        self.instances.iter().any(Option::is_some)
    }

    pub fn sub_latency(&self, instance: usize) -> u32 {
        self.latencies.get(instance).copied().unwrap_or(0)
    }

    /// Returns the reference metadata for the loaded sub-plugin at `instance`.
    pub fn reference(&self, instance: usize) -> Option<&SubPluginRef> {
        self.at(instance).map(|l| &l.reference)
    }

    /// Returns the capabilities of the loaded sub-plugin at `instance`.
    ///
    /// Defaults to all-false when nothing is loaded, which is also the honest
    /// answer: a graph cannot send per-voice modulation to a plugin that is
    /// not there.
    pub fn capabilities(&self, instance: usize) -> plugin_host::Capabilities {
        self.at(instance)
            .map_or_else(Default::default, |l| l.plugin.capabilities())
    }

    /// Restores an opaque state blob to the specified sub-plugin instance.
    ///
    /// For a harness that wants a plugin to open with a particular patch
    /// already in it; a DAW does this through the normal state path instead.
    pub fn load_sub_state(&mut self, instance: usize, blob: &[u8]) -> Result<(), String> {
        let loaded = self.at_mut(instance).ok_or("no sub-plugin loaded")?;
        loaded.plugin.load_state(blob).map_err(|e| e.to_string())?;
        // A plugin may finish taking the blob on a main-thread callback, and
        // until it runs it still reports its old values. Same rule as in
        // `save_state`.
        loaded.plugin.tick();
        Ok(())
    }

    /// Returns the audio bus layout and note support for the sub-plugin at
    /// `instance`, for building a node's sockets. Empty when nothing is
    /// loaded, which draws as a node with no sockets rather than as an error.
    pub fn io_layout(&self, instance: usize) -> plugin_host::IoLayout {
        self.at(instance)
            .map(|l| SubPluginMain::io_layout(&l.plugin))
            .unwrap_or_default()
    }

    /// Returns the lowest unused instance index below `max_instances`.
    ///
    /// Reused rather than always-increasing, so that dropping one sub-plugin
    /// and adding another does not walk off the end of `max_instances`.
    pub fn free_instance(&self) -> Option<usize> {
        (0..self.config.max_instances).find(|&i| !self.is_loaded(i))
    }

    pub fn params(&self, instance: usize) -> &[ParamInfo] {
        match self.at(instance) {
            Some(l) => SubPluginMain::params(&l.plugin),
            None => &[],
        }
    }

    /// Loads a sub-plugin into the given instance slot.
    ///
    /// If a plugin was previously loaded at this instance, it is replaced.
    /// Existing slot bindings are preserved and re-resolved against the new
    /// plugin, so swapping a plugin for a newer version of itself keeps every
    /// mapping.
    ///
    /// The format follows from the path: `plugin-host` reads it off the
    /// extension, so a `.clap` and a `.vst3` arrive here the same way and
    /// nothing below this line branches on which it was.
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

        // Only after the new one is known good, so a failed load leaves the
        // user with what they had rather than with nothing.
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
        // Dropping the entry tears down editor, instance and module in that
        // order — see the note on `Loaded`.
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

    /// Attempts to resolve a sub-plugin reference to an existing file path.
    ///
    /// Projects move between machines, and a plugin folder that differs by
    /// one directory should not cost the user their patch: the class id is the
    /// authority and the recorded path is only a hint.
    pub fn resolve_reference(reference: &SubPluginRef) -> Option<PathBuf> {
        // An unrecognised format tag is "not found" rather than an error,
        // which is what the caller already handles: a reference saved before
        // CLAP existed has no format tag worth trusting, and one saved by a
        // newer build might name a format this build does not have.
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

    /// Opens the sub-plugin's editor GUI in a separate top-level window.
    ///
    /// Not composited into the wrapper's own editor: a native child window
    /// cannot be composited over a GPU surface, so a separate window is the
    /// only workable arrangement.
    ///
    /// `owner` is the window the sub-plugin's editor should float above: the
    /// DAW's root window when running as a plugin, null when standalone. An
    /// ownerless window is a peer of the DAW's, so clicking in the DAW would
    /// bury it — which is what this argument exists to prevent.
    pub fn open_editor(
        &mut self,
        instance: usize,
        owner: *mut std::ffi::c_void,
    ) -> Result<(), String> {
        let loaded = self.at_mut(instance).ok_or("no sub-plugin loaded")?;
        loaded.plugin.open_editor(owner)
    }

    /// Closes the editor window for the sub-plugin at `instance`.
    pub fn close_editor(&mut self, instance: usize) {
        if let Some(loaded) = self.at_mut(instance) {
            loaded.plugin.close_editor();
        }
    }

    /// Closes all open sub-plugin editor windows.
    pub fn close_all_editors(&mut self) {
        for instance in 0..self.instances.len() {
            self.close_editor(instance);
        }
    }

    pub fn editor_is_open(&self, instance: usize) -> bool {
        self.at(instance).is_some_and(|l| l.plugin.editor_is_open())
    }

    /// Drives main-thread processing and UI callbacks for all loaded
    /// sub-plugins.
    ///
    /// Call from the host's UI thread every frame, whether or not any editor
    /// is open: a plugin's main-thread callbacks and timers run through here
    /// too, and one starved of them stalls. `save_state`, `load_state` and
    /// `load_sub_state` additionally tick around the plugin themselves, since
    /// a callback missed there costs data rather than responsiveness.
    ///
    /// One platform is still short: on VST3 under Linux the underlying host
    /// posts these onto a worker thread rather than a main thread, so the tick
    /// declines to do anything. CLAP under Linux goes through
    /// `request_callback()` and is fine.
    pub fn tick_editors(&mut self) {
        for instance in 0..self.instances.len() {
            if let Some(loaded) = self.at_mut(instance) {
                loaded.plugin.tick();
            }
        }
    }

    /// Binds a slot index to a parameter on the loaded sub-plugin at `instance`.
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

    /// Resolves all parameter targets for a given instance into
    /// `(lane_index, ResolvedTarget)` pairs.
    ///
    /// Two sources, one shape: the slot table is the DAW's automation lanes,
    /// and the lanes past it are whatever else the caller drives directly. The
    /// merge in `SubHostProcessor::process` does not care which is which.
    fn targets_for(&self, instance: usize, direct: &[ParamTarget]) -> Vec<(usize, ResolvedTarget)> {
        // Only the slots bound against *this* instance. Handing every
        // instance the whole table would make one slot drive the same
        // parameter on every copy.
        let mut targets = self.slots.active_targets(instance as u32);
        let params = self.params(instance);
        for (lane, target) in direct.iter().enumerate() {
            if target.instance as usize != instance {
                continue;
            }
            let Some(info) = params.iter().find(|p| p.id.0 == target.param) else {
                // The parameter went away with a plugin update. The socket
                // stays in the graph, the same way an unresolved binding
                // stays in the slot table.
                continue;
            };
            targets.push((
                self.slots.count() + lane,
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

    /// Activates all loaded sub-plugins for audio processing.
    ///
    /// All or nothing: if one instance refuses the configuration, the ones
    /// already activated are wound back rather than left running, because a
    /// half-activated set is a state no later call knows how to handle.
    ///
    /// `io` specifies per-instance audio bus configurations (main and aux
    /// channels). It comes from the caller rather than from the plugin,
    /// because whether a sidechain is switched on depends on whether anything
    /// was wired to it. An instance `io` does not mention is activated with
    /// `config` as it stands.
    pub fn activate(
        &mut self,
        config: AudioConfig,
        io: &[InstanceIo],
        direct: &[ParamTarget],
    ) -> Result<SubHostProcessors, String> {
        // One event per lane per sub-block is the worst a caller can ask for,
        // plus whatever the DAW sends us. Reserved here because `process` is
        // not allowed to grow it.
        let sub_blocks = config
            .max_block_size
            .div_ceil(crate::schedule::MIN_QUANTUM)
            .max(1) as usize;
        let capacity = self.config.lanes * sub_blocks + INCOMING_EVENT_CAPACITY;

        let mut processors: Vec<Option<SubHostProcessor>> = Vec::new();
        for instance in 0..self.instances.len() {
            if self.at(instance).is_none() {
                processors.push(None);
                continue;
            }
            // Before the plugin is borrowed for activation, because this
            // reads both the slot table and the plugin's parameter list.
            let targets = self.targets_for(instance, direct);
            let Some(loaded) = self.at_mut(instance) else {
                unreachable!("checked just above")
            };
            // Apply per-instance bus configuration overrides if specified.
            let config = match io.iter().find(|e| e.instance as usize == instance) {
                Some(entry) => AudioConfig {
                    input_channels: u32::from(entry.input_channels),
                    output_channels: u32::from(entry.output_channels),
                    aux_inputs: plugin_host::AuxBuses::new(&entry.aux_inputs),
                    aux_outputs: plugin_host::AuxBuses::new(&entry.aux_outputs),
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
                        last_sent: vec![f64::NAN; self.config.lanes],
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

    /// Serializes sub-host state, including slot configuration and opaque
    /// state from all loaded sub-plugins.
    ///
    /// Takes `&mut self` so it can tick first. A plugin is entitled to fold
    /// recent edits into what it serialises on one of its own main-thread
    /// callbacks rather than immediately; without the tick, such a plugin
    /// saves the values it held before the last edit.
    pub fn save_state(&mut self) -> SubHostState {
        let mut state = SubHostState {
            slots: self.slots.to_state(),
            instances: Vec::new(),
        };
        for instance in 0..self.instances.len() {
            if let Some(loaded) = self.at_mut(instance) {
                loaded.plugin.tick();
            }
            let Some(loaded) = self.at(instance) else {
                continue;
            };
            // The wrapper's own state is still worth saving when a plugin
            // will not give up its own: losing the graph and the bindings as
            // well would turn one plugin's failure into a lost project.
            let bytes = match loaded.plugin.save_state() {
                Ok(bytes) => Some(bytes),
                Err(e) => {
                    log::error!("sub-plugin {instance} state could not be saved: {e}");
                    None
                }
            };
            state.instances.push(InstanceState {
                instance,
                reference: loaded.reference.clone(),
                state: bytes.as_deref().map(crate::state::base64_encode),
            });
        }
        state
    }

    /// Restores sub-host state, attempting to locate and reload each saved sub-plugin instance.
    ///
    /// Returns a list of diagnostic messages rather than an error: a missing
    /// sub-plugin must not stop the rest of the patch from loading.
    pub fn load_state(&mut self, state: &SubHostState) -> Vec<String> {
        let mut problems = Vec::new();
        self.slots.load_state(state.slots.clone());
        self.unload_all();

        for entry in &state.instances {
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
                    if let Some(loaded) = self.at_mut(entry.instance) {
                        if let Err(e) = loaded.plugin.load_state(&bytes) {
                            problems.push(format!(
                                "{} loaded but its settings did not restore: {e}",
                                reference.display_name
                            ));
                        }
                        // As in `load_sub_state`. This is the path a project
                        // open takes, and it runs with the wrapper's own
                        // editor closed, so nothing else would tick these.
                        loaded.plugin.tick();
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

/// Default incoming event capacity used to preallocate event scratch buffers.
///
/// Overshooting costs a few kilobytes; undershooting would cost events, so it
/// is generous.
const INCOMING_EVENT_CAPACITY: usize = 1024;

/// Audio-thread processor for a single sub-plugin instance.
pub struct SubHostProcessor {
    processor: Box<dyn SubPluginProcessor>,
    /// Parameter targets and their schedule lane indices, captured at
    /// activate so the audio thread never walks the slot table.
    targets: Vec<(usize, ResolvedTarget)>,
    /// Cached normalized values previously sent to sub-plugin parameters to deduplicate events.
    /// Initialized to `f64::NAN` so the initial values are always dispatched.
    last_sent: Vec<f64>,
    /// Reused event buffer. Sized at activate; `process` must not allocate.
    scratch: Vec<Event>,
}

impl SubHostProcessor {
    /// Processes an audio buffer through the sub-plugin for the specified sample chunk.
    ///
    /// Merges incoming host events and parameter automation values from
    /// `slots` into a sample-accurate event stream dispatched to the
    /// sub-plugin. The two streams are merged in offset order because a
    /// sub-plugin is entitled to assume its input events are sorted, and
    /// several real ones misbehave quietly rather than loudly when they
    /// are not.
    ///
    /// `chunk` is the part of the block this call covers: the whole block
    /// unless an audio delay line put the program on sub-block granularity, in
    /// which case this runs once per sub-block and each call must be handed
    /// *its own* events, rebased — a note at sample 40 belongs to the chunk
    /// starting at 32, at offset 8, and to no other. Handing every chunk the
    /// whole block would replay every note once per chunk.
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
        // Everything before the chunk was sent on an earlier call;
        // everything after it belongs to a later one.
        let events = slice(events, &chunk);
        let mut next_note = 0;

        for index in 0..slots.blocks() {
            let offset = slots.offset(index);
            // Rows outside this chunk are another call's business. The last
            // chunk of a block is short whenever the block is not a multiple
            // of the quantum, so `<` on the end is what keeps the boundary row
            // out of both calls' way rather than in both.
            if offset < chunk.start || offset >= chunk.end.max(chunk.start + 1) {
                continue;
            }
            let offset = offset - chunk.start;

            // Anything the DAW sent that lands before this boundary goes
            // first, so the stream stays sorted.
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
                // Resending would waste the sub-plugin's parameter queue
                // and, worse, retrigger smoothing on plugins that ramp
                // towards every incoming point.
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
        // After a reset the sub-plugin's idea of its parameters is no longer
        // something we can assume, so force a resend.
        self.last_sent.iter_mut().for_each(|v| *v = f64::NAN);
    }
}

/// Collection of audio-thread processors for all loaded sub-plugin instances.
///
/// Entries are indexed by instance ID to match the sparse layout of
/// [`SubHost`], and stay sparse for the same reason: the caller names an
/// instance by index.
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

    /// Binds block-level context (slot schedule, transport context) to produce
    /// a [`BoundInstances`] processor for the duration of a block.
    ///
    /// The DAW's note stream is not among it: the graph decides what each
    /// instance hears and hands that over per call, so there is nothing here
    /// for a whole block to share.
    ///
    /// A caller runs `process(instance, ...)` with nothing but buffers,
    /// because it has no reason to know that a plugin has events or a
    /// transport position. Everything else is fixed for the whole block, so it
    /// is attached here and the borrow lasts exactly that long.
    pub fn bind<'a>(
        &'a mut self,
        slots: &'a SlotSchedule,
        context: &'a TimeContext,
        out_events: &'a mut EventSink,
    ) -> BoundInstances<'a> {
        BoundInstances {
            processors: self,
            slots,
            context,
            out_events,
        }
    }
}

/// Audio-thread sub-plugin processors bound to a block's schedule and event context.
pub struct BoundInstances<'a> {
    processors: &'a mut SubHostProcessors,
    slots: &'a SlotSchedule,
    context: &'a TimeContext,
    out_events: &'a mut EventSink,
}

impl crate::instances::AudioInstances for BoundInstances<'_> {
    fn process(
        &mut self,
        instance: u32,
        notes: &[Event],
        input: &[f32],
        output: &mut [f32],
        chunk: crate::instances::AudioChunk,
    ) {
        let Some(processor) = self
            .processors
            .entries
            .get_mut(instance as usize)
            .and_then(Option::as_mut)
        else {
            // A node whose plugin failed to load, or was deleted while the
            // audio thread held this program. Silence is the only honest
            // answer; passing the input through would let the user hear the
            // graph working when it is not.
            for ch in 0..chunk.output_channels {
                output[chunk.channel(ch)].fill(0.0);
            }
            return;
        };

        // Both regions hold the main bus and then each aux bus, packed at
        // `frames`, which is the layout `AudioBuffers` wants; `aux_inputs`
        // tells the backend where the joins are. Nothing is repacked here.
        let mut buffers = AudioBuffers::new(
            input,
            output,
            chunk.input_channels as u32,
            chunk.output_channels as u32,
            chunk.frames,
            plugin_host::BufferLayout::Planar,
        )
        .with_aux_inputs(chunk.aux_inputs)
        .with_aux_outputs(chunk.aux_outputs);
        processor.process(
            &mut buffers,
            self.slots,
            notes,
            chunk.offset..chunk.offset + chunk.frames,
            self.context,
            self.out_events,
        );
    }
}

/// Returns the slice of events falling within the sample offset range of
/// `chunk`.
///
/// The stream is sorted, so this is a range rather than a filter — which is
/// what makes it free of allocation, and free of any per-event work at all on
/// the blocks where every event falls in the first chunk.
fn slice<'a>(events: &'a [Event], chunk: &Range<u32>) -> &'a [Event] {
    let start = events.partition_point(|e| e.sample_offset() < chunk.start);
    let end = events.partition_point(|e| e.sample_offset() < chunk.end);
    &events[start..end.max(start)]
}

/// Appends an event to the scratch buffer unless it is full.
///
/// Dropping an event is bad; growing a `Vec` inside an audio callback is
/// worse, and the capacity reserved at activate is the worst case plus a wide
/// margin.
fn push(scratch: &mut Vec<Event>, event: Event) {
    if scratch.len() < scratch.capacity() {
        scratch.push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_host::{BufferLayout, NoteEvent, ParamFlags};

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

    /// Test configuration constants for slot and lane counts.
    const SLOTS: usize = 32;
    const LANES: usize = SLOTS + 64 + 16;

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
            last_sent: vec![f64::NAN; LANES],
            scratch: Vec::with_capacity(4096),
        };
        (processor, seen)
    }

    fn run(p: &mut SubHostProcessor, values: &[f64]) {
        let mut schedule = SlotSchedule::new(LANES, 4, 32);
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
        let mut values = vec![0.0; SLOTS];
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
        // Unchanged slot values should not produce redundant parameter events.
        let target = ResolvedTarget {
            instance: 0,
            id: ParamId(1),
            min: 0.0,
            max: 1.0,
        };
        let (mut p, seen) = harness(vec![(0, target)]);
        let values = vec![0.25; SLOTS];

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
        let values = vec![0.25; SLOTS];

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
        run(&mut p, &vec![0.5; SLOTS]);
        assert!(seen.lock().unwrap().is_empty());
    }

    /// Verifies that events are routed only to the sub-block chunk containing their timestamp,
    /// with offsets rebased relative to chunk start.
    #[test]
    fn a_chunk_hears_only_its_own_events_rebased() {
        let (mut p, seen) = harness(Vec::new());
        let note = Event::Note(NoteEvent::NoteOn {
            note_id: None,
            port: 0,
            channel: 0,
            key: 60,
            velocity: 1.0,
            sample_offset: 40,
        });

        let mut schedule = SlotSchedule::new(LANES, 128, 32);
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

    /// Verifies that slot automation boundaries are dispatched to the containing chunk with rebased offsets.
    #[test]
    fn a_chunk_sends_only_its_own_slot_boundaries() {
        let target = ResolvedTarget {
            instance: 0,
            id: ParamId(3),
            min: 0.0,
            max: 1.0,
        };
        let (mut p, seen) = harness(vec![(0, target)]);

        let mut schedule = SlotSchedule::new(LANES, 128, 32);
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
        // Sub-block automation changes are dispatched as sample-accurate events across the block.
        let target = ResolvedTarget {
            instance: 0,
            id: ParamId(3),
            min: 0.0,
            max: 1.0,
        };
        let (mut p, seen) = harness(vec![(0, target)]);

        let mut schedule = SlotSchedule::new(LANES, 128, 32);
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

        let mut schedule = SlotSchedule::new(LANES, 128, 32);
        schedule.begin(128);
        schedule.fill(&vec![0.5; SLOTS]);
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

        let mut schedule = SlotSchedule::new(LANES, 128, 32);
        let blocks = schedule.begin(128);
        for i in 0..blocks {
            schedule.block_mut(i)[0] = i as f64 / 4.0;
        }
        let notes = [
            Event::Note(plugin_host::NoteEvent::NoteOn {
                note_id: Some(1),
                port: 0,
                channel: 0,
                key: 60,
                velocity: 1.0,
                sample_offset: 40,
            }),
            Event::Note(plugin_host::NoteEvent::NoteOff {
                note_id: Some(1),
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
        // Bindings match by parameter ID rather than position.
        let mut table = SlotTable::new(SLOTS);
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

    /// Notes are the caller's decision by the time they get here, so the only
    /// thing this side owes is delivering exactly what it was handed — and
    /// nothing to an instance handed nothing. Which events an instance should
    /// hear is the engine's business, and tested there.
    #[test]
    fn an_instance_hears_what_it_was_handed_and_no_more() {
        use crate::instances::{AudioChunk, AudioInstances};
        use plugin_host::NoteEvent;

        let (wired, wired_saw) = harness(Vec::new());
        let (idle, idle_saw) = harness(Vec::new());
        let mut processors = SubHostProcessors {
            entries: vec![Some(wired), Some(idle)],
        };

        let note = Event::Note(NoteEvent::NoteOn {
            note_id: None,
            port: 0,
            channel: 0,
            key: 60,
            velocity: 1.0,
            sample_offset: 0,
        });
        let schedule = SlotSchedule::new(LANES, 4, 32);
        let mut sink = EventSink::new();
        let context = TimeContext::default();
        let mut running = processors.bind(&schedule, &context, &mut sink);

        let chunk = AudioChunk {
            input_channels: 2,
            output_channels: 2,
            aux_inputs: Default::default(),
            aux_outputs: Default::default(),
            frames: 4,
            offset: 0,
        };
        let input = [0.0f32; 8];
        let mut output = [0.0f32; 8];
        running.process(0, &[note], &input, &mut output, chunk);
        running.process(1, &[], &input, &mut output, chunk);

        assert_eq!(wired_saw.lock().unwrap().len(), 1);
        assert!(
            idle_saw.lock().unwrap().is_empty(),
            "an instance handed nothing must hear nothing"
        );
    }
}
