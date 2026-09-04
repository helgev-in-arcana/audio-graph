//! Shared state accessible by both the editor UI and the audio processing thread.
//!
//! The arrangement here is the answer to one question: what may the audio
//! thread be made to wait for?
//!
//! One lock around everything is honest for loading a sub-plugin, which is rare,
//! slow and audibly disruptive whatever we do. But the editor also touches this
//! state sixty times a second to redraw and to tick the sub-plugin's window, and
//! each of those would be a chance for the audio thread to lose a block for no
//! reason at all. A graph editor makes it worse: dragging an LFO rate recompiles
//! continuously, and an audio path that drops a block per edit would make the
//! engine's own bugs indistinguishable from lock contention.
//!
//! So the state is split by how often it is touched rather than by what it is:
//!
//! - [`MainState`] — the sub-plugin host and what activating it needs. Reached
//!   constantly, and only ever from the main thread, so it needs no lock at
//!   all. `MainThread` turns that from a comment into a runtime check.
//! - [`Patch`] — the graph the user is editing. Behind a mutex rather than in
//!   `MainState` because the editor draws it and the editor does not always run
//!   on the main thread: baseview gives it one of its own on X11. Nothing here
//!   is a plugin object, so there is no thread affinity to lose, and the audio
//!   thread never touches it.
//! - [`AudioState`] — the live processors, behind a mutex the audio thread only
//!   ever *tries*, and which the main thread takes solely to start or stop a
//!   sub-plugin. Swapping a plugin mid-playback glitches, which is the honest
//!   cost of doing it at all, and nothing else contends.
//! - The compiled [`Program`] — published through a [`Handoff`], which the audio
//!   thread reads without any lock whatsoever. This is the path every graph edit
//!   takes, so it is the one that had to be free.

use std::array;
use std::cell::{RefCell, RefMut};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::config::SLOT_COUNT;
use crate::state::WrapperState;
use audio_graph_engine::{Graph, Handoff, Program, compile};
use audio_graph_engine::{NodeId, NodeKind, Plugin, PluginPorts};
use parking_lot::Mutex;
use plugin_host::{AudioConfig, MainThread};
use subhost_adapter::{DEFAULT_QUANTUM, InstanceIo, ParamTarget, SubHost, SubHostProcessors};

use crate::params::WrapperParams;
use crate::view::View;

/// Everything reached from the main thread, and from nowhere else.
pub struct MainState {
    pub host: SubHost,
    /// Remembered so the editor can activate a newly loaded sub-plugin without
    /// waiting for the DAW to call `activate` again.
    pub config: Option<AudioConfig>,
    /// Bus configurations required for each sub-plugin instance under the current program.
    ///
    /// Kept here rather than read back off the engine because the engine lives
    /// on the audio side of the wrapper, and this is needed on the main thread
    /// every time something is re-activated.
    pub instance_io: Vec<InstanceIo>,
    /// Which sub-plugin parameter each graph-driven lane drives. Read at
    /// activate, like the slot bindings, so a new socket only reaches audio
    /// after a restart.
    pub graph_params: Vec<ParamTarget>,
    /// Tracked delay ring buffer allocations (node ID and size in samples) sent to the audio thread.
    ///
    /// Only so the next publish can tell whether anything changed. A recompile
    /// happens on every drag of every control, and allocating 700 kB each time
    /// to replace a ring with an identical one would be silly.
    pub sized_rings: Vec<(NodeId, usize)>,
}

/// The graph being edited, reachable from whichever thread the editor is on.
pub struct Patch {
    pub graph: Graph,
    /// Why the graph on screen is not the graph being heard.
    ///
    /// A cycle or a duplicate output is an ordinary thing to have halfway
    /// through an edit; the last program that compiled keeps running and the
    /// editor says why.
    pub compile_error: Option<String>,
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
    patch: Mutex<Patch>,
    audio: Mutex<AudioState>,
    programs: Handoff<Program>,
    /// Sub-block modulation quantum in samples.
    ///
    /// Stored as a standalone atomic rather than in `MainState` because the audio
    /// thread reads it every block and must not have to acquire a lock to do so.
    quantum: AtomicU32,
    /// What the DAW is running at, so the editor can show a delay's floor in
    /// seconds. Bits of an `f32`, the same trick `live` uses.
    sample_rate: AtomicU32,
    /// What the last graph to compile costs the DAW in samples.
    ///
    /// Kept here rather than read off the engine, because the engine only ever
    /// receives a program on the audio thread and the number is wanted on the
    /// main one, at the moment the DAW asks what to align the track by.
    latency: AtomicU32,
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
    /// The blob this instance last wrote into the persisted field.
    ///
    /// `None` until it has written one. It is what tells the wrapper's own
    /// bookkeeping apart from a project or a preset the DAW has put there,
    /// which is the only kind that has to be read back in.
    last_written: Mutex<Option<String>>,
    /// Bumped whenever something the editor displays has changed shape — a
    /// different sub-plugin, a different set of bindings.
    ///
    /// The editor draws from a cached snapshot rather than rebuilding a
    /// two-thousand-entry parameter list sixty times a second, and this is how
    /// it knows the snapshot is stale.
    generation: AtomicU64,
    /// What the editor should draw, rebuilt on the main thread.
    ///
    /// Published rather than read directly because everything in it comes from
    /// the sub-plugin host, which only the main thread may touch.
    view: Mutex<View>,
    /// Work the editor asked for and may not carry out itself.
    ///
    /// Used where the editor has a thread of its own — see the editor's
    /// `dispatch`. Drained by the tick, which is on the host's main thread.
    posted: Mutex<Vec<Task>>,
    /// Whether the editor's window is open, which is what decides how often
    /// the tick has anything to do.
    editor_open: AtomicBool,
}

/// One piece of main-thread work handed over by the editor.
type Task = Box<dyn FnOnce(&Arc<Shared>) + Send>;

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
                instance_io: Vec::new(),
                graph_params: Vec::new(),
                sized_rings: Vec::new(),
            })),
            patch: Mutex::new(Patch {
                graph: Graph::default_patch(),
                compile_error: None,
            }),
            audio: Mutex::new(AudioState { processor: None }),
            programs: Handoff::new(),
            quantum: AtomicU32::new(DEFAULT_QUANTUM),
            // Until the DAW says otherwise. A wrong rate here only makes the
            // floor shown in the editor wrong, never the audio.
            sample_rate: AtomicU32::new(48_000f32.to_bits()),
            latency: AtomicU32::new(0),
            live: array::from_fn(|_| AtomicU32::new(0)),
            params,
            last_written: Mutex::new(None),
            generation: AtomicU64::new(0),
            view: Mutex::new(View::default()),
            posted: Mutex::new(Vec::new()),
            editor_open: AtomicBool::new(false),
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

    /// The graph being edited.
    ///
    /// Any thread. Held only for as long as it takes to read or edit — the
    /// editor takes it for a frame, the main thread to compile.
    ///
    /// # Never together with the others
    /// No two of `main`, `patch` and `audio` are ever held at once anywhere in
    /// this crate; where a function needs two of them it takes them in
    /// sequence, and several say so at the point where it would have been
    /// easier not to. That is a stronger property than a lock order and a
    /// cheaper one to check, so it is the one to keep.
    pub fn patch(&self) -> parking_lot::MutexGuard<'_, Patch> {
        self.patch.lock()
    }

    /// What the editor should draw.
    ///
    /// `None` while the main thread is rebuilding it: a frame that cannot have
    /// it draws the previous snapshot, which looks like nothing happened, as
    /// opposed to blanking the window for one frame.
    pub(crate) fn try_view(&self) -> Option<parking_lot::MutexGuard<'_, View>> {
        self.view.try_lock()
    }

    /// Rebuild what the editor draws. Main thread only, from the tick.
    ///
    /// Everything in the snapshot comes from the sub-plugin host, which is why
    /// the editor cannot simply read it: on X11 the editor is not on the thread
    /// the host is bound to.
    /// The one place two of this type's locks are held at once — `main` and
    /// then `view`. Both are taken with `try_`, so the pair cannot wait on
    /// anything and a turn that cannot have them is simply skipped; the tick
    /// comes round again.
    pub(crate) fn publish_view(&self) {
        let Some(state) = self.try_main() else { return };
        let Some(mut view) = self.view.try_lock() else {
            return;
        };
        view.rebuild(&state.host, self.generation());
    }

    /// Hand `task` to the main thread, to run on its next tick.
    ///
    /// The route out of a draw callback, and the only one. A GUI toolkit's draw
    /// callback is not a safe place to load a plugin or touch a window — those
    /// calls dispatch messages, and the message lands back inside the toolkit
    /// while it is still in the middle of the frame that started it, which
    /// egui-baseview answers with a `RefCell` violation inside a callback that
    /// cannot unwind. So the draw callback may only *record* what the user
    /// asked for, and this is where the record goes.
    ///
    /// It is also the route across threads, which is the same route because
    /// baseview does not always give the editor the host's thread. The queue
    /// belongs to the plugin instance rather than to the editor, so an editor
    /// closing cannot cancel what it just posted.
    pub fn post_main(&self, task: impl FnOnce(&Arc<Shared>) + Send + 'static) {
        self.posted.lock().push(Box::new(task));
    }

    /// Run everything the editor posted. Main thread only, from the tick.
    pub fn run_posted(self: &Arc<Shared>) {
        // The guard is dropped before a single task runs, and has to be: a task
        // is free to post another, and that would deadlock against a lock still
        // held here. Taking the queue whole rather than popping one at a time
        // also settles what a re-entrant call sees — an empty queue, so no task
        // can be started twice or lost.
        let tasks = {
            let mut posted = self.posted.lock();
            std::mem::take(&mut *posted)
        };
        for task in tasks {
            task(self);
        }
    }

    /// Whether the editor's window is open.
    pub fn editor_open(&self) -> bool {
        self.editor_open.load(Ordering::Relaxed)
    }

    pub fn set_editor_open(&self, open: bool) {
        self.editor_open.store(open, Ordering::Relaxed);
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

    /// What to tell the DAW the track has to be pulled forward by.
    ///
    /// The whole answer: a plugin's own latency reaches this through the node
    /// that names it, and the compiler lines the parallel paths up against the
    /// longest of them. A plugin nothing routes through contributes nothing,
    /// which is why the plugins are not asked directly.
    pub fn latency(&self) -> u32 {
        self.latency.load(Ordering::Relaxed)
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

    /// Compile the current graph, hand it to the audio thread, and re-activate
    /// the sub-plugins if their buses moved with it.
    ///
    /// Called after every edit. A graph that will not compile leaves the last
    /// working program running and records why, because the alternative —
    /// silence, or the DAW's raw automation reappearing — would make the
    /// editor's own error message the second thing the user noticed.
    pub fn publish_graph(&self) {
        if self.send_program()
            && let Err(e) = self.rebind()
        {
            log::warn!("audio-graph: re-activating for new buses: {e}");
        }
    }

    /// Compile the current graph and hand it to the audio thread, with every
    /// delay line's ring allocated afresh.
    ///
    /// For an activation, which is the one moment `sized_rings` cannot be
    /// trusted: it records what the audio thread was last handed, and a
    /// program waiting in the handoff when the DAW deactivates us is dropped
    /// unread. A line the cache calls unchanged would then arrive with no ring
    /// at all, and a delay with no buffer to read is a delay that repeats
    /// nothing.
    pub(crate) fn send_fresh_program(&self) {
        self.main().sized_rings.clear();
        self.send_program();
    }

    /// Compile the current graph and hand it to the audio thread.
    ///
    /// Returns whether the sub-plugins' buses or parameter targets moved with
    /// it. Acting on that is [`Shared::rebind`]'s job, and a caller that is
    /// about to activate them itself has nothing to do about it. `false` also
    /// when the graph would not compile: nothing was sent, so nothing has to
    /// be activated against it.
    fn send_program(&self) -> bool {
        // Copied, then compiled with nothing held. The editor draws under this
        // same lock on its own thread, so holding it across a compile would
        // cost it a frame every time the user drags a control — which is
        // exactly when compiles happen.
        let graph = self.patch().graph.clone();
        let compiled = compile(&graph, SLOT_COUNT);

        // One guard for the answer, so that the error and the graph it is about
        // cannot be separated by an edit landing in between.
        let mut program = {
            let mut patch = self.patch();
            match compiled {
                Ok(program) => {
                    patch.compile_error = None;
                    program
                }
                Err(e) => {
                    patch.compile_error = Some(e.to_string());
                    return false;
                }
            }
        };

        self.latency.store(program.latency, Ordering::Relaxed);

        let mut state = self.main();
        // The delay rings are allocated here, on the main thread, and ride over
        // inside the program, because the audio thread may not allocate.
        // `sized_rings` remembers what was sent last time so an unchanged line
        // is handed nothing rather than a fresh copy of what it already has.
        state.sized_rings = program.size_rings(f64::from(self.sample_rate()), &state.sized_rings);
        // A graph edit can change which buses a sub-plugin needs — wiring a
        // sidechain is exactly that — and a bus cannot be switched on while the
        // plugin is active. Whether the change has to be acted on is
        // `rebind`'s decision; recording it is this one's.
        let changed =
            state.instance_io != program.instances || state.graph_params != program.param_targets;
        state.instance_io = program.instances.clone();
        state.graph_params = program.param_targets.clone();
        self.programs.send(Box::new(program));
        changed
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

    /// Give a patch that has no graph the one it was implicitly running.
    ///
    /// Patches saved when the wrapper passed audio through by itself relied on
    /// "no graph" meaning "input to output", and one sub-plugin with no graph
    /// meaning "through that plugin". Those implicit paths are gone, so such a
    /// project would reopen silent unless the routing it was already getting is
    /// drawn for it.
    ///
    /// Ensure patches without an explicit graph configuration receive a default
    /// audio pass-through patch.
    pub fn adopt_default_patch(&self) {
        {
            let mut patch = self.patch();
            // Empty, or still untouched: `restore_state` adopts before the
            // development override has had a chance to load anything, so the
            // patch this finds the second time round is the one it just drew.
            if !patch.graph.is_empty() && patch.graph != Graph::default_patch() {
                return;
            }
            patch.graph = Graph::default_patch();
        }
        // Read out rather than tested in place, so that the borrow is plainly
        // over before the patch lock below is taken.
        let loaded = self.main().host.is_loaded(0);
        // If a sub-plugin is already loaded, wire it inline between audio in and out.
        if loaded {
            let node = self.patch().graph.add(
                NodeKind::Plugin(Plugin {
                    instance: 0,
                    ports: PluginPorts::default(),
                }),
                [210.0, 80.0],
            );
            // Sockets before links: `discover_ports` prunes, and a link into a
            // socket the node does not have yet is exactly what it prunes.
            self.discover_ports(node);
            let mut patch = self.patch();
            let graph = &mut patch.graph;
            let (input, output) = (graph.nodes[0].id, graph.nodes[1].id);
            graph.links.clear();
            graph.connect(input, 0, node, 0);
            graph.connect(node, 0, output, 0);
            // Out of the way of the plugin we just slid in.
            graph.node_mut(output).unwrap().pos = [520.0, 80.0];
            drop(patch);
        }
        self.publish_graph();
    }

    /// Bring every plugin node's recorded latency up to date with the plugin
    /// it names, and say whether any of them moved.
    ///
    /// A plugin only answers for its latency once it has been activated, so
    /// the number a node was drawn with can predate the only moment the plugin
    /// could have been asked — and a plugin is free to change its mind
    /// afterwards, which is why the plugins are asked again rather than the
    /// cache being trusted. Only the latency is re-read; the sockets are
    /// [`Shared::discover_ports`]' business, and re-reading those would prune
    /// links against a layout the user has not asked about.
    pub fn refresh_latencies(&self) -> bool {
        // Read out and copied, so the host and the patch are never locked at
        // once for a question that needs one of them at a time. At most
        // `max_instances` numbers.
        let latencies: Vec<u32> = {
            let mut state = self.main();
            state.host.reread_latencies();
            state.host.latencies().to_vec()
        };
        let mut patch = self.patch();
        let mut moved = false;
        for node in &mut patch.graph.nodes {
            if let NodeKind::Plugin(Plugin { instance, ports }) = &mut node.kind {
                let latency = latencies.get(*instance).copied().unwrap_or(0);
                if ports.latency != latency {
                    ports.latency = latency;
                    moved = true;
                }
            }
        }
        moved
    }

    /// Re-read one plugin node's sockets from the plugin itself.
    ///
    /// Called after a load, and after the plugin says its I/O changed. Links to
    /// sockets that no longer exist are dropped by `prune`, which is the same
    /// rule a patch reopened against a newer plugin follows.
    pub fn discover_ports(&self, node: NodeId) {
        let instance = {
            let mut patch = self.patch();
            // Before anything is read off the node: a patch older than
            // `audio_out_shown` has no picks to preserve, and settling what it
            // meant is what turns "every bus" into the handful it wired.
            patch.graph.migrate_plugin_outputs();
            match patch.graph.node(node).map(|n| &n.kind) {
                Some(NodeKind::Plugin(Plugin { instance, .. })) => *instance,
                _ => return,
            }
        };

        // Asked of the host before the patch is taken again, so the two locks
        // are never held at once for a question that needs only one of them.
        let (layout, latency) = {
            let state = self.main();
            (
                state.host.io_layout(instance),
                state.host.sub_latency(instance),
            )
        };
        let discovered = PluginPorts::from_layout(&layout, latency);

        let mut patch = self.patch();
        let Some(node) = patch.graph.nodes.iter_mut().find(|n| n.id == node) else {
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
        patch.graph.prune();
        drop(patch);
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
        let graph = serde_json::to_value(&self.patch().graph).ok();
        let mut state = self.main();
        let mut blob = WrapperState::default();
        blob.set_sub_host_state(state.host.save_state());
        blob.version = crate::state::STATE_VERSION;
        blob.graph = graph;
        blob.sub_block = self.quantum();
        drop(state);
        self.write_state(&blob);
    }

    fn write_state(&self, state: &WrapperState) {
        match serde_json::to_string(state) {
            Ok(json) => {
                // Two stores rather than one lock held across both:
                // `state_is_unseen` takes the same pair in the other order,
                // and the editor stores state from whichever thread its
                // button was pressed on.
                *self.params.state.0.write().unwrap() = json.clone();
                *self.last_written.lock() = Some(json);
            }
            Err(e) => log::warn!("audio-graph: wrapper state unwritable: {e}"),
        }
    }

    /// Whether the persisted blob is one this instance did not write.
    ///
    /// A DAW hands a project or a preset over by writing that field, which it
    /// may do before the first activation or in the middle of a session —
    /// nice-plug answers the second by activating again rather than
    /// deactivating first, so an activation is where both of them arrive.
    /// Everything the wrapper stores goes through `write_state`, so a blob
    /// that is not the one recorded there came from outside and is the user's
    /// project rather than our own last word on it.
    pub(crate) fn state_is_unseen(&self) -> bool {
        let json = self.params.state.0.read().unwrap().clone();
        self.last_written.lock().as_deref() != Some(json.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SUB_HOST;
    use plugin_host::{HostContext, RestartReason};

    struct SilentHost;

    impl HostContext for SilentHost {
        fn host_name(&self) -> &str {
            "shared tests"
        }
        fn request_restart(&self, _reason: RestartReason) {}
        fn latency_changed(&self, _samples: u32) {}
        fn param_edited(&self, _id: plugin_host::ParamId, _value: f64) {}
    }

    fn shared() -> Arc<Shared> {
        Shared::new(
            SubHost::new(Arc::new(SilentHost), SUB_HOST),
            WrapperParams::new(),
        )
    }

    /// The wrapper's own last word on the state is not mistaken for the DAW's.
    ///
    /// The editor stores state after every change it makes, so an activation
    /// that read all of those back in would reload every sub-plugin each time
    /// the DAW so much as changed its block size.
    #[test]
    fn only_a_blob_the_wrapper_did_not_write_counts_as_unseen() {
        let shared = shared();
        // Nothing written yet, so whatever is in the field belongs to the DAW
        // — the empty string included, which is what a fresh instance carries
        // and what has to be answered with the defaults.
        assert!(shared.state_is_unseen());

        shared.store_state();
        assert!(
            !shared.state_is_unseen(),
            "the wrapper reading its own bookkeeping back in reloads every \
             sub-plugin for nothing"
        );

        *shared.params().state.0.write().expect("not poisoned") = String::from("{}");
        assert!(
            shared.state_is_unseen(),
            "a blob written from outside is the user's project and has to be read"
        );
    }
}
