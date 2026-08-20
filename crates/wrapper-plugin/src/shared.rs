//! State the editor and the audio path both need to reach.
//!
//! The arrangement here is the answer to one question: what may the audio
//! thread be made to wait for?
//!
//! Until M5 the answer was "one lock around everything", and the audio thread
//! declined it rather than waiting. That was honest for what it covered — the
//! editor only ever took the lock to *load* a sub-plugin, which is rare, slow
//! and audibly disruptive whatever we do. But the editor also took it sixty
//! times a second to redraw and to tick the sub-plugin's window, and each of
//! those was a chance for the audio thread to lose a block for no reason at
//! all. A graph editor makes that far worse: dragging an LFO rate recompiles
//! continuously, and an audio path that drops a block per edit would make the
//! engine's own bugs indistinguishable from lock contention.
//!
//! So the state is split by how often it is touched rather than by what it is:
//!
//! - [`MainState`] — the sub-plugin's controller half, the graph, the editor's
//!   own bookkeeping. Reached constantly, and only ever from the main thread,
//!   so it needs no lock at all. `MainThread` turns that from a comment into a
//!   runtime check.
//! - [`AudioState`] — the live processor. Behind a mutex the audio thread only
//!   ever *tries*, and which the main thread takes solely to start or stop a
//!   sub-plugin. Swapping a plugin mid-playback glitches, which is the honest
//!   cost of doing it at all, and nothing else contends.
//! - The compiled [`Program`] — published through a [`Handoff`], which the
//!   audio thread reads without any lock whatsoever (§9.1). This is the path
//!   every graph edit takes, so it is the one that had to be free.

use std::array;
use std::cell::{RefCell, RefMut};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use parking_lot::Mutex;
use plugin_host_api::AudioConfig;
use subhost_adapter::{
    DEFAULT_QUANTUM, MainThread, SLOT_COUNT, SubHost, SubHostProcessors, WrapperState,
};
use wrapper_engine::{Graph, Handoff, Program, compile};

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
}

/// The live processor, and the only thing the audio thread ever waits on.
pub struct AudioState {
    /// `Some` between `activate` and `deactivate`, whether or not a sub-plugin
    /// is loaded — an empty wrapper still runs, it just passes audio through.
    pub processor: Option<SubHostProcessors>,
}

/// The handle both halves of the plugin hold.
pub struct Shared {
    main: MainThread<RefCell<MainState>>,
    audio: Mutex<AudioState>,
    programs: Handoff<Program>,
    /// Sub-block size in samples (§9.2). An atomic rather than part of
    /// `MainState` because the audio thread reads it every block and must not
    /// have to ask anybody's permission.
    quantum: AtomicU32,
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

impl Shared {
    pub fn new(host: SubHost, params: Arc<WrapperParams>) -> Arc<Shared> {
        Arc::new(Shared {
            main: MainThread::new(RefCell::new(MainState {
                host,
                config: None,
                graph: Graph::new(),
                compile_error: None,
            })),
            audio: Mutex::new(AudioState { processor: None }),
            programs: Handoff::new(),
            quantum: AtomicU32::new(DEFAULT_QUANTUM),
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
            Ok(program) => {
                state.compile_error = None;
                self.programs.send(Box::new(program));
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
        self.suspend();
        self.main().host.load(0, path, None)?;
        self.resume()
    }

    pub fn unload(&self) {
        self.suspend();
        self.main().host.unload(0);
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
        if !state.host.is_loaded(0) {
            return Ok(());
        }
        let Some(config) = state.config else {
            return Ok(());
        };
        let processor = state.host.activate(config)?;
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
        let state = self.main();
        let mut blob = state.host.save_state();
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
