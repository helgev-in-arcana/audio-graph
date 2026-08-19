//! State the editor and the audio path both need to reach.
//!
//! Until now the sub-plugin lived inside [`Wrapper`][crate::Wrapper] by value,
//! which was fine while the only thing that could load one was `activate`. An
//! editor changes that: the user picks a plugin while the DAW is running, so
//! the load has to happen from the UI thread, against a sub-plugin the audio
//! thread is in the middle of using.
//!
//! The arrangement here is deliberately the simple one. Both sides go through a
//! mutex; the audio thread only ever *tries* to take it and passes audio
//! through when it can't. That makes a sub-plugin swap glitch — the load takes
//! long enough to drop buffers — and it is the honest cost of doing the swap
//! at all. What it does not do is allocate, block, or hand the audio thread a
//! processor whose plugin has been unloaded underneath it.
//!
//! §9 will want a proper hand-off (build the new processor, publish it with one
//! atomic swap, drop the old one on the main thread) once the node graph makes
//! swapping routine. This is the seam that becomes that.

use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use plugin_host_api::AudioConfig;
use subhost_adapter::{SubHost, SubHostProcessor, WrapperState};

use crate::params::WrapperParams;

/// Everything about the sub-plugin, behind one lock.
pub struct SubState {
    pub host: SubHost,
    /// `Some` between `activate` and `deactivate`, whether or not a sub-plugin
    /// is loaded — an empty wrapper still runs, it just passes audio through.
    pub processor: Option<SubHostProcessor>,
    /// Remembered so the editor can re-activate a newly loaded sub-plugin
    /// without waiting for the DAW to call `activate` again.
    pub config: Option<AudioConfig>,
}

impl SubState {
    /// Stop the sub-plugin's processing, if it is running.
    fn suspend(&mut self) {
        if let Some(processor) = self.processor.take() {
            self.host.deactivate(processor);
        }
    }

    /// Start it again under the configuration the DAW last gave us.
    ///
    /// A failure here is reported but not fatal: the wrapper falls back to
    /// passing audio through, which is much better than the DAW deciding the
    /// whole track is broken.
    fn resume(&mut self) -> Result<(), String> {
        if !self.host.is_loaded() {
            return Ok(());
        }
        let Some(config) = self.config else { return Ok(()) };
        match self.host.activate(config) {
            Ok(processor) => {
                self.processor = Some(processor);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Swap in a different sub-plugin while the DAW is running.
    pub fn load(&mut self, path: &Path) -> Result<(), String> {
        self.suspend();
        self.host.load(path, None)?;
        self.resume()
    }

    pub fn unload(&mut self) {
        self.suspend();
        self.host.unload();
    }

    /// Re-activate after something changed that the processor caches — the slot
    /// bindings, which are read once at activate.
    pub fn rebind(&mut self) -> Result<(), String> {
        self.suspend();
        self.resume()
    }
}

/// The handle both halves of the plugin hold.
pub struct Shared {
    state: Mutex<SubState>,
    params: Arc<WrapperParams>,
}

impl Shared {
    pub fn new(host: SubHost, params: Arc<WrapperParams>) -> Arc<Shared> {
        Arc::new(Shared {
            state: Mutex::new(SubState { host, processor: None, config: None }),
            params,
        })
    }

    pub fn params(&self) -> &Arc<WrapperParams> {
        &self.params
    }

    /// Main-thread access. Blocks; never call this from `process`.
    pub fn lock(&self) -> parking_lot::MutexGuard<'_, SubState> {
        self.state.lock()
    }

    /// Audio-thread access. Declines rather than waiting.
    pub fn try_lock(&self) -> Option<parking_lot::MutexGuard<'_, SubState>> {
        self.state.try_lock()
    }

    /// Serialise the current sub-plugin and slot table into the persisted field.
    ///
    /// Called after every edit made from the editor, so whenever the DAW decides
    /// to save the project there is something current waiting for it.
    pub fn store_state(&self) {
        let state = self.state.lock().host.save_state();
        self.write_state(&state);
    }

    fn write_state(&self, state: &WrapperState) {
        match serde_json::to_string(state) {
            Ok(json) => *self.params.state.0.write().unwrap() = json,
            Err(e) => log::warn!("audio-graph: wrapper state unwritable: {e}"),
        }
    }
}
