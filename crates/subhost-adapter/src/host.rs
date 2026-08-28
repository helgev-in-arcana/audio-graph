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
    NoteEvent, ParamEvent, ParamId, ParamInfo, Plugin, ProcessStatus, SubPluginMain,
    SubPluginProcessor, Target, TimeContext,
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
    /// Empty entries are preserved so instance indices remain stable.
    instances: Vec<Option<MainThread<Loaded>>>,
    slots: SlotTable,
    context: Arc<dyn HostContext>,
    /// Cached latency in samples for each instance from its last activation.
    latencies: Vec<u32>,
}

/// Configuration limits and buffer sizing parameters for a sub-host.
///
/// Used to preallocate instance tables and event buffers during activation
/// to avoid allocations on the real-time audio thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubHostConfig {
    /// Maximum number of sub-plugin instances that may be hosted.
    pub max_instances: usize,
    /// Number of parameter slots published to the host DAW.
    pub slot_count: usize,
    /// Number of values carried per sub-block in the [`SlotSchedule`] (slots plus direct parameter lanes).
    pub lanes: usize,
}

/// A loaded sub-plugin instance and its reference metadata.
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

    /// Upper bound on active instance indices (length of the sparse instance list).
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

    /// Returns the cached latency in samples for all instances, indexed by instance ID.
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
    /// Returns default (all false) capabilities if no plugin is loaded at this index.
    pub fn capabilities(&self, instance: usize) -> plugin_host::Capabilities {
        self.at(instance)
            .map_or_else(Default::default, |l| l.plugin.capabilities())
    }

    /// Restores an opaque state blob to the specified sub-plugin instance.
    pub fn load_sub_state(&mut self, instance: usize, blob: &[u8]) -> Result<(), String> {
        let loaded = self.at_mut(instance).ok_or("no sub-plugin loaded")?;
        loaded.plugin.load_state(blob).map_err(|e| e.to_string())?;
        // Process any main-thread tasks or callbacks queued by the plugin during state loading.
        loaded.plugin.tick();
        Ok(())
    }

    /// Returns the audio bus layout and note support for the sub-plugin at `instance`.
    /// Returns default/empty layout if no plugin is loaded.
    pub fn io_layout(&self, instance: usize) -> plugin_host::IoLayout {
        self.at(instance)
            .map(|l| SubPluginMain::io_layout(&l.plugin))
            .unwrap_or_default()
    }

    /// Returns the lowest unused instance index below `max_instances`.
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
    /// If a plugin was previously loaded at this instance, it is replaced. Existing
    /// slot bindings for this instance are preserved and re-resolved against the new plugin.
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

        // Unload existing instance only after the new plugin loads successfully.
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
        // Dropping the Loaded struct tears down the editor before the plugin instance.
        if let Some(slot) = self.instances.get_mut(instance) {
            *slot = None;
        }
        if let Some(latency) = self.latencies.get_mut(instance) {
            *latency = 0;
        }
        // Clear resolved parameter targets for this instance while preserving configured bindings.
        self.slots.unresolve(instance as u32);
    }

    pub fn unload_all(&mut self) {
        self.instances.clear();
        self.latencies.clear();
        self.slots.unresolve_all();
    }

    /// Attempts to resolve a sub-plugin reference to an existing file path.
    ///
    /// Uses the plugin ID and format as the primary identifier, falling back
    /// to the path hint if available.
    pub fn resolve_reference(reference: &SubPluginRef) -> Option<PathBuf> {
        // Return None if the format tag is unrecognized.
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

    /// Opens the sub-plugin's editor GUI in a separate window.
    ///
    /// `owner` is the native window handle (e.g. host DAW window) that the editor
    /// should be parented to or float above, or null for a standalone window.
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

    /// Drives main-thread processing and UI callbacks for all loaded sub-plugins.
    ///
    /// Should be called regularly from the host's main/UI thread to allow plugins
    /// to process timers, deferred state updates, and main-thread callbacks.
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

    /// Resolves all parameter targets (from slot automation and direct parameter lanes)
    /// for a given instance into `(lane_index, ResolvedTarget)` pairs.
    fn targets_for(&self, instance: usize, direct: &[ParamTarget]) -> Vec<(usize, ResolvedTarget)> {
        // Retrieve only slot targets bound to this specific instance.
        let mut targets = self.slots.active_targets(instance as u32);
        let params = self.params(instance);
        for (lane, target) in direct.iter().enumerate() {
            if target.instance as usize != instance {
                continue;
            }
            let Some(info) = params.iter().find(|p| p.id.0 == target.param) else {
                // Skip direct targets whose parameter ID is not present in the loaded plugin.
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
    /// If any instance fails to activate, all previously activated instances in this
    /// call are deactivated and an error is returned.
    ///
    /// `io` specifies per-instance audio bus configurations (main and aux channels);
    /// instances omitted from `io` use the default `config`.
    pub fn activate(
        &mut self,
        config: AudioConfig,
        io: &[InstanceIo],
        direct: &[ParamTarget],
    ) -> Result<SubHostProcessors, String> {
        // Preallocate event buffer capacity for the maximum expected parameter and MIDI events.
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
            // Resolve targets before borrowing the plugin for activation.
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
                        gated: Vec::new(),
                    });
                    return Err(message);
                }
            }
        }

        Ok(SubHostProcessors {
            entries: processors,
            gated: Vec::with_capacity(capacity),
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

    /// Serializes sub-host state, including slot configuration and opaque state from all loaded sub-plugins.
    ///
    /// Calls `tick` on each plugin prior to saving to ensure any deferred parameter or state edits are flushed.
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
            // Continue saving remaining state even if an individual plugin fails to serialize its state.
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
    /// Returns a list of diagnostic messages for any plugins or states that could not be fully restored.
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
                        // Process any deferred callbacks or initialization queued during state restoration.
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
const INCOMING_EVENT_CAPACITY: usize = 1024;

/// Audio-thread processor for a single sub-plugin instance.
pub struct SubHostProcessor {
    processor: Box<dyn SubPluginProcessor>,
    /// Parameter targets and their corresponding schedule lane indices.
    targets: Vec<(usize, ResolvedTarget)>,
    /// Cached normalized values previously sent to sub-plugin parameters to deduplicate events.
    /// Initialized to `f64::NAN` so the initial values are always dispatched.
    last_sent: Vec<f64>,
    /// Preallocated scratch buffer for merged parameter and MIDI events.
    scratch: Vec<Event>,
}

impl SubHostProcessor {
    /// Processes an audio buffer through the sub-plugin for the specified sample chunk.
    ///
    /// Merges incoming host events and parameter automation values from `slots` into
    /// a sample-accurate, offset-sorted event stream dispatched to the sub-plugin.
    /// Events and slot boundaries falling within `chunk` are rebased to chunk-relative offsets.
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
        // Restrict events to those occurring within this chunk range.
        let events = slice(events, &chunk);
        let mut next_note = 0;

        for index in 0..slots.blocks() {
            let offset = slots.offset(index);
            // Skip sub-block boundaries outside the current chunk range.
            if offset < chunk.start || offset >= chunk.end.max(chunk.start + 1) {
                continue;
            }
            let offset = offset - chunk.start;

            // Insert incoming host events preceding this sub-block boundary to maintain time ordering.
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
                // Skip parameters whose values have not changed to avoid redundant event generation.
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
        // Invalidate cached parameter values so the next process call re-sends all parameters.
        self.last_sent.iter_mut().for_each(|v| *v = f64::NAN);
    }
}

/// Collection of audio-thread processors for all loaded sub-plugin instances.
///
/// Entries are indexed by instance ID to match the sparse layout of [`SubHost`].
pub struct SubHostProcessors {
    entries: Vec<Option<SubHostProcessor>>,
    /// Preallocated event buffer for filtering note streams (e.g. note-off only for closed gates).
    gated: Vec<Event>,
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

    /// Binds block-level context (slot schedule, incoming events, transport context)
    /// to produce a [`BoundInstances`] processor for the duration of a block.
    pub fn bind<'a>(
        &'a mut self,
        slots: &'a SlotSchedule,
        events: &'a [Event],
        context: &'a TimeContext,
        out_events: &'a mut EventSink,
    ) -> BoundInstances<'a> {
        BoundInstances {
            processors: self,
            slots,
            events,
            context,
            out_events,
        }
    }
}

/// Audio-thread sub-plugin processors bound to a block's schedule and event context.
pub struct BoundInstances<'a> {
    processors: &'a mut SubHostProcessors,
    slots: &'a SlotSchedule,
    events: &'a [Event],
    context: &'a TimeContext,
    out_events: &'a mut EventSink,
}

impl crate::instances::AudioInstances for BoundInstances<'_> {
    fn process(
        &mut self,
        instance: u32,
        notes: crate::instances::NoteStream,
        input: &[f32],
        output: &mut [f32],
        chunk: crate::instances::AudioChunk,
    ) {
        // Destructure to borrow entries and gated scratch buffer independently.
        let SubHostProcessors { entries, gated } = &mut *self.processors;
        let Some(processor) = entries.get_mut(instance as usize).and_then(Option::as_mut) else {
            // Output silence if the instance has no loaded processor.
            for ch in 0..chunk.output_channels {
                output[chunk.channel(ch)].fill(0.0);
            }
            return;
        };

        // Input and output slices contain planar channels for main and aux buses.
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
        // Filter note events according to the instance's NoteStream configuration.
        // When note-ons are gated off, note-offs still pass through to prevent hung notes.
        // Muted keys are filtered out entirely.
        let events: &[Event] = match notes.source {
            crate::instances::NoteSource::Daw { bus: 0 } if notes.mute == 0 => self.events,
            source @ (crate::instances::NoteSource::Daw { bus: 0 }
            | crate::instances::NoteSource::DawReleases { bus: 0 }) => {
                let shut = matches!(source, crate::instances::NoteSource::DawReleases { .. });
                gated.clear();
                for event in self.events {
                    if shut && matches!(event, Event::Note(NoteEvent::NoteOn { .. })) {
                        continue;
                    }
                    if muted(event, notes.mute) {
                        continue;
                    }
                    if gated.len() == gated.capacity() {
                        break;
                    }
                    gated.push(*event);
                }
                gated.as_slice()
            }
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

/// Returns `true` if the event is a note event whose MIDI key bit is set in `mask`.
/// Non-note events (such as CCs) are not muted.
fn muted(event: &Event, mask: u128) -> bool {
    let Event::Note(note) = event else {
        return false;
    };
    match note.key() {
        Some(key) if (0..128).contains(&key) => mask & (1u128 << key) != 0,
        _ => false,
    }
}

/// Returns the slice of events falling within the sample offset range of `chunk`.
fn slice<'a>(events: &'a [Event], chunk: &Range<u32>) -> &'a [Event] {
    let start = events.partition_point(|e| e.sample_offset() < chunk.start);
    let end = events.partition_point(|e| e.sample_offset() < chunk.end);
    &events[start..end.max(start)]
}

/// Appends an event to the scratch buffer if capacity allows without reallocating.
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
            note_id: -1,
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
                note_id: 1,
                port: 0,
                channel: 0,
                key: 60,
                velocity: 1.0,
                sample_offset: 40,
            }),
            Event::Note(plugin_host::NoteEvent::NoteOff {
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

    /// Verifies that only instances routed to a note source receive note events.
    #[test]
    fn only_the_instance_the_graph_wired_hears_the_daws_notes() {
        use crate::instances::{AudioChunk, AudioInstances, NoteSource, NoteStream};
        use plugin_host::NoteEvent;

        let (wired, wired_saw) = harness(Vec::new());
        let (idle, idle_saw) = harness(Vec::new());
        let mut processors = SubHostProcessors {
            gated: Vec::with_capacity(64),
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
        let schedule = SlotSchedule::new(LANES, 4, 32);
        let mut sink = EventSink::new();
        let context = TimeContext::default();
        let incoming = [note];
        let mut running = processors.bind(&schedule, &incoming, &context, &mut sink);

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
        let daw = NoteStream::from_source(NoteSource::Daw { bus: 0 });
        running.process(0, daw, &input, &mut output, chunk);
        running.process(1, NoteStream::default(), &input, &mut output, chunk);

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

    /// Verifies that a gated note stream filters out note-on events while delivering note-offs.
    #[test]
    fn a_shut_note_gate_still_delivers_the_releases() {
        use crate::instances::{AudioChunk, AudioInstances, NoteSource, NoteStream};
        use plugin_host::NoteEvent;

        let (gated_node, saw) = harness(Vec::new());
        let mut processors = SubHostProcessors {
            gated: Vec::with_capacity(64),
            entries: vec![Some(gated_node)],
        };

        let incoming = [
            Event::Note(NoteEvent::NoteOn {
                note_id: -1,
                port: 0,
                channel: 0,
                key: 60,
                velocity: 1.0,
                sample_offset: 0,
            }),
            Event::Note(NoteEvent::NoteOff {
                note_id: -1,
                port: 0,
                channel: 0,
                key: 55,
                velocity: 0.0,
                sample_offset: 1,
            }),
        ];
        let schedule = SlotSchedule::new(LANES, 4, 32);
        let mut sink = EventSink::new();
        let context = TimeContext::default();
        let mut running = processors.bind(&schedule, &incoming, &context, &mut sink);

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
        running.process(
            0,
            NoteStream::from_source(NoteSource::DawReleases { bus: 0 }),
            &input,
            &mut output,
            chunk,
        );

        let seen = saw.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "only the release got through");
        assert!(matches!(
            seen[0],
            Event::Note(NoteEvent::NoteOff { key: 55, .. })
        ));
    }
}
