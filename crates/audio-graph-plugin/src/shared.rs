//! Shared state accessible by both the editor UI and the audio processing thread.
//!
//! State is partitioned by thread access patterns to prevent audio thread blocking:
//!
//! - [`MainState`] — UI state, sub-plugin host controllers, and graph structure.
//!   Accessed solely on the main thread without mutex synchronization.
//! - [`AudioState`] — Active audio processors. Protected by a mutex that the audio
//!   thread accesses via `try_lock()`, while the main thread locks it only during
//!   plugin load, unload, or reset.
//! - Compiled [`Program`] — Passed to the audio thread via lock-free [`Handoff`],
//!   enabling real-time graph recompilation and hot-swapping without lock contention.

use std::array;
use std::cell::{RefCell, RefMut};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::config::SLOT_COUNT;
use crate::state::WrapperState;
use audio_graph_engine::{Graph, Handoff, Program, compile};
use audio_graph_engine::{NodeId, NodeKind, Plugin, PluginPorts};
use parking_lot::Mutex;
use plugin_host::{AudioConfig, MainThread};
use subhost_adapter::{DEFAULT_QUANTUM, InstanceIo, ParamTarget, SubHost, SubHostProcessors};

use crate::params::WrapperParams;

/// Everything reached from the main thread, and from nowhere else.
pub struct MainState {
    pub host: SubHost,
    /// Remembered so the editor can activate a newly loaded sub-plugin without
    /// waiting for the DAW to call `activate` again.
    pub config: Option<AudioConfig>,
    pub graph: Graph,
    /// Why the graph on screen is not the graph being heard.
    ///
    /// A cycle or a duplicate output is an ordinary thing to have halfway
    /// through an edit; the last program that compiled keeps running and the
    /// editor says why.
    pub compile_error: Option<String>,
    /// Bus configurations required for each sub-plugin instance under the current program.
    ///
    /// Maintained on the main thread for reactivation when routing changes.
    pub instance_io: Vec<InstanceIo>,
    /// Which sub-plugin parameter each graph-driven lane targets.
    pub graph_params: Vec<ParamTarget>,
    /// Tracked delay ring buffer allocations (node ID and size in samples) sent to the audio thread.
    ///
    /// Retained across recompilations to prevent redundant memory reallocations when delay lengths are unchanged.
    pub sized_rings: Vec<(NodeId, usize)>,
}

/// The live processor, and the only thing the audio thread ever waits on.
pub struct AudioState {
    /// `Some` between `activate` and `deactivate`, whether or not a sub-plugin
    /// is loaded — an empty wrapper still runs, it just passes audio through.
    pub processor: Option<SubHostProcessors>,
}

/// The handle both halves of the plugin hold.
///
/// `main` is declared before `audio`, so Rust drops the sub-plugins *first* —
/// which is exactly backwards, because a processor holds an interface pointer
/// into the plugin that produced it. [`Drop`] below hands the processors back
/// before anything is released; the field order is left alone because relying
/// on it would be a rule nothing states.
pub struct Shared {
    main: MainThread<RefCell<MainState>>,
    audio: Mutex<AudioState>,
    programs: Handoff<Program>,
    /// Sub-block modulation quantum in samples.
    ///
    /// Stored as a standalone atomic rather than in `MainState` because the audio
    /// thread reads it every block and must not have to acquire a lock to do so.
    quantum: AtomicU32,
    /// Host sample rate used for GUI calculations (e.g. delay times).
    sample_rate: AtomicU32,
    /// What each slot is actually worth after the graph has had its say.
    ///
    /// Written by the audio thread once per block and read by the editor. Two
    /// relaxed stores per driven slot, no ordering with anything, and the only
    /// consequence of a stale read is a meter a frame behind — which is a
    /// meter. Without it the editor cannot show that an LFO is doing anything,
    /// because the DAW's parameter value does not move when the graph is what
    /// is driving the slot.
    live: [AtomicU32; SLOT_COUNT],
    params: Arc<WrapperParams>,
    /// Bumped whenever something the editor displays has changed shape — a
    /// different sub-plugin, a different set of bindings.
    ///
    /// The editor draws from a cached snapshot rather than rebuilding a
    /// two-thousand-entry parameter list sixty times a second, and this is how
    /// it knows the snapshot is stale.
    generation: AtomicU64,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // Hand every processor back before its plugin is released. A DAW always
        // calls `deactivate` first and this never fires there — but a panic
        // anywhere between activate and deactivate would otherwise turn into an
        // access violation during unwinding, which is a much worse thing to
        // debug than the panic that caused it.
        self.suspend();
    }
}

impl Shared {
    pub fn new(host: SubHost, params: Arc<WrapperParams>) -> Arc<Shared> {
        Arc::new(Shared {
            main: MainThread::new(RefCell::new(MainState {
                host,
                config: None,
                graph: Graph::default_patch(),
                compile_error: None,
                instance_io: Vec::new(),
                graph_params: Vec::new(),
                sized_rings: Vec::new(),
            })),
            audio: Mutex::new(AudioState { processor: None }),
            programs: Handoff::new(),
            quantum: AtomicU32::new(DEFAULT_QUANTUM),
            // Until the DAW says otherwise. A wrong rate here only makes the
            // floor shown in the editor wrong, never the audio.
            sample_rate: AtomicU32::new(48_000f32.to_bits()),
            live: array::from_fn(|_| AtomicU32::new(0)),
            params,
            generation: AtomicU64::new(0),
        })
    }

    /// # Panics
    /// If called from any thread but the one that created the plugin.
    pub fn main(&self) -> RefMut<'_, MainState> {
        self.main.get().borrow_mut()
    }

    /// Whether this thread is the one the plugin was created on.
    ///
    /// For callers that have to *ask* rather than assert. The periodic tick is
    /// posted through nice-plug's `execute_gui`, and one backend — VST3 on
    /// Linux — runs those on a worker thread rather than a main thread,
    /// because Linux has no main thread to speak of. Panicking there would
    /// turn a tick we simply cannot deliver into a crash.
    pub fn on_main_thread(&self) -> bool {
        self.main.is_owner()
    }

    /// Main-thread access that declines rather than panicking when the state
    /// is already borrowed further up the stack.
    ///
    /// Ticking resizes windows, resizing dispatches messages, and a message
    /// can land back inside something that is already holding this. The tick
    /// is periodic, so skipping one costs a frame and nothing else.
    ///
    /// # Panics
    /// If called from any thread but the one that created the plugin.
    pub fn try_main(&self) -> Option<RefMut<'_, MainState>> {
        self.main.get().try_borrow_mut().ok()
    }

    /// Audio-thread access to the processor. Declines rather than waiting.
    pub fn try_audio(&self) -> Option<parking_lot::MutexGuard<'_, AudioState>> {
        self.audio.try_lock()
    }

    /// Blocking access, for the rare heavy operations that must not be skipped:
    /// starting a sub-plugin, stopping one, resetting after a transport jump.
    /// None of them happen while audio is flowing normally.
    pub fn audio(&self) -> parking_lot::MutexGuard<'_, AudioState> {
        self.audio.lock()
    }

    pub fn programs(&self) -> &Handoff<Program> {
        &self.programs
    }

    pub fn params(&self) -> &Arc<WrapperParams> {
        &self.params
    }

    pub fn quantum(&self) -> u32 {
        self.quantum.load(Ordering::Relaxed)
    }

    pub fn set_quantum(&self, quantum: u32) {
        self.quantum.store(quantum, Ordering::Relaxed);
    }

    pub fn sample_rate(&self) -> f32 {
        f32::from_bits(self.sample_rate.load(Ordering::Relaxed))
    }

    pub fn set_sample_rate(&self, rate: f32) {
        self.sample_rate.store(rate.to_bits(), Ordering::Relaxed);
    }

    /// Report the slot values the sub-plugin is actually being driven with.
    /// Audio thread; lock-free and allocation-free.
    pub fn report_slots(&self, values: &[f64]) {
        for (cell, &value) in self.live.iter().zip(values) {
            cell.store((value as f32).to_bits(), Ordering::Relaxed);
        }
    }

    /// The same values, for the editor's meters.
    pub fn live_slots(&self) -> [f32; SLOT_COUNT] {
        array::from_fn(|i| f32::from_bits(self.live[i].load(Ordering::Relaxed)))
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Tell the editor its cached view of the sub-plugin is out of date.
    pub fn changed(&self) {
        self.generation.fetch_add(1, Ordering::Relaxed);
    }

    /// Compile the current graph and hand it to the audio thread.
    ///
    /// Called after every edit. A graph that will not compile leaves the last
    /// working program running and records why, because the alternative —
    /// silence, or the DAW's raw automation reappearing — would make the
    /// editor's own error message the second thing the user noticed.
    pub fn publish_graph(&self) {
        let mut state = self.main();
        match compile(&state.graph, SLOT_COUNT) {
            Ok(mut program) => {
                state.compile_error = None;
                // Delay ring buffers are allocated on the main thread and transferred inside the program.
                // `sized_rings` tracks previous allocations to avoid redundant allocations when unchanged.
                state.sized_rings =
                    program.size_rings(f64::from(self.sample_rate()), &state.sized_rings);
                // A graph edit can change which buses a sub-plugin needs —
                // wiring a sidechain is exactly that — and a bus cannot be
                // switched on while the plugin is active. Whether the change
                // has to be acted on is `reactivate`'s decision; recording it
                // is this one's.
                let changed = state.instance_io != program.instances
                    || state.graph_params != program.param_targets;
                state.instance_io = program.instances.clone();
                state.graph_params = program.param_targets.clone();
                self.programs.send(Box::new(program));
                drop(state);
                if changed && let Err(e) = self.rebind() {
                    log::warn!("audio-graph: re-activating for new buses: {e}");
                }
            }
            Err(e) => state.compile_error = Some(e.to_string()),
        }
    }

    /// Free anything the audio thread has handed back. Main thread, called from
    /// the editor's tick so a plugin with its window shut still tidies up.
    pub fn reclaim(&self) {
        self.programs.reclaim();
    }

    /// Swap in a different sub-plugin while the DAW is running.
    pub fn load(&self, path: &Path) -> Result<(), String> {
        self.load_into(0, path)
    }

    /// Load a plugin into one instance slot and configure the node with its ports.
    ///
    /// The node is added first and the plugin arrives afterwards, which is why
    /// this takes a node id: a plugin takes hundreds of milliseconds to load,
    /// and a canvas that froze until it had finished would be worse than one
    /// where the sockets appear a moment later. A node whose plugin fails to
    /// load simply keeps no sockets, and says so.
    pub fn load_into(&self, instance: usize, path: &Path) -> Result<(), String> {
        self.suspend();
        let result = self.main().host.load(instance, path, None);
        let resumed = self.resume();
        result?;
        resumed
    }

    /// Ensure patches without an explicit graph configuration receive a default
    /// audio pass-through patch.
    pub fn adopt_default_patch(&self) {
        {
            let mut state = self.main();
            // Empty, or still untouched: `restore_state` adopts before the
            // development override has had a chance to load anything, so the
            // patch this finds the second time round is the one it just drew.
            if !state.graph.is_empty() && state.graph != Graph::default_patch() {
                return;
            }
            state.graph = Graph::default_patch();
        }
        // If a sub-plugin is already loaded, wire it inline between audio in and out.
        if self.main().host.is_loaded(0) {
            let node = self.main().graph.add(
                NodeKind::Plugin(Plugin {
                    instance: 0,
                    ports: PluginPorts::default(),
                }),
                [210.0, 80.0],
            );
            // Sockets before links: `discover_ports` prunes, and a link into a
            // socket the node does not have yet is exactly what it prunes.
            self.discover_ports(node);
            let mut state = self.main();
            let (input, output) = (state.graph.nodes[0].id, state.graph.nodes[1].id);
            state.graph.links.clear();
            state.graph.connect(input, 0, node, 0);
            state.graph.connect(node, 0, output, 0);
            // Out of the way of the plugin we just slid in.
            state.graph.node_mut(output).unwrap().pos = [520.0, 80.0];
            drop(state);
        }
        self.publish_graph();
    }

    /// Re-read one plugin node's sockets from the plugin itself.
    ///
    /// Called after a load, and after the plugin says its I/O changed. Links to
    /// sockets that no longer exist are dropped by `prune`, which is the same
    /// rule a patch reopened against a newer plugin follows.
    pub fn discover_ports(&self, node: NodeId) {
        let mut state = self.main();
        // Before anything is read off the node: a patch older than
        // `audio_out_shown` has no picks to preserve, and settling what it
        // meant is what turns "every bus" into the handful it wired.
        state.graph.migrate_plugin_outputs();
        let Some(instance) = state.graph.node(node).and_then(|n| match n.kind {
            NodeKind::Plugin(Plugin { instance, .. }) => Some(instance),
            _ => None,
        }) else {
            return;
        };
        let layout = state.host.io_layout(instance);
        let latency = state.host.sub_latency(instance);
        let discovered = PluginPorts::from_layout(&layout, latency);
        let Some(node) = state.graph.nodes.iter_mut().find(|n| n.id == node) else {
            return;
        };
        if let NodeKind::Plugin(Plugin { ports, .. }) = &mut node.kind {
            // Parameter sockets are user-configured: discovery updates audio and note buses
            // while preserving existing parameter socket bindings. Which output buses have
            // sockets is also preserved once configured; initial discovery defaults to the main bus.
            let params = std::mem::take(&mut ports.params);
            let shown = (!ports.audio_out_shown.is_empty()).then(|| ports.audio_out_shown.clone());
            *ports = discovered;
            ports.params = params;
            if let Some(shown) = shown {
                ports.audio_out_shown = shown;
                // Every pick pointed at a bus the reloaded plugin no longer
                // has. Silently ending up with no way out of the node would be
                // worse than falling back to the main bus.
                if ports.shown_outputs().is_empty() && !ports.audio_out.is_empty() {
                    ports.audio_out_shown = vec![0];
                }
            }
        }
        state.graph.prune();
        drop(state);
        self.publish_graph();
    }

    pub fn unload(&self) {
        self.unload_instance(0);
    }

    pub fn unload_instance(&self, instance: usize) {
        self.suspend();
        self.main().host.unload(instance);
        let _ = self.resume();
    }

    /// Re-activate after something the processor caches has changed — the slot
    /// bindings, which are read once at activate.
    pub fn rebind(&self) -> Result<(), String> {
        self.suspend();
        self.resume()
    }

    /// Stop the sub-plugin's processing, if it is running.
    fn suspend(&self) {
        let processor = self.audio().processor.take();
        if let Some(processor) = processor {
            self.main().host.deactivate(processor);
        }
    }

    /// Start it again under the configuration the DAW last gave us.
    ///
    /// A failure here is reported but not fatal: the wrapper falls back to
    /// passing audio through, which is much better than the DAW deciding the
    /// whole track is broken.
    fn resume(&self) -> Result<(), String> {
        let mut state = self.main();
        if !state.host.any_loaded() {
            return Ok(());
        }
        let Some(config) = state.config else {
            return Ok(());
        };
        let io = state.instance_io.clone();
        let graph_params = state.graph_params.clone();
        let processor = state.host.activate(config, &io, &graph_params)?;
        drop(state);
        self.audio().processor = Some(processor);
        Ok(())
    }

    /// Serialise the sub-plugin, the slot table and the graph into the
    /// persisted field.
    ///
    /// Called after every edit made from the editor, so whenever the DAW
    /// decides to save the project there is something current waiting for it.
    pub fn store_state(&self) {
        let mut state = self.main();
        let mut blob = WrapperState::default();
        blob.set_sub_host_state(state.host.save_state());
        blob.version = crate::state::STATE_VERSION;
        blob.graph = serde_json::to_value(&state.graph).ok();
        blob.sub_block = self.quantum();
        drop(state);
        self.write_state(&blob);
    }

    fn write_state(&self, state: &WrapperState) {
        match serde_json::to_string(state) {
            Ok(json) => *self.params.state.0.write().unwrap() = json,
            Err(e) => log::warn!("audio-graph: wrapper state unwritable: {e}"),
        }
    }
}
