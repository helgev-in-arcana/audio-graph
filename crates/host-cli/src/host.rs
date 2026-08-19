//! The `HostContext` the CLI injects into the backend.
//!
//! `vst3-host` never builds one of these itself (§7), so this is what a
//! standalone renderer supplies; `subhost-adapter` will later supply a
//! different one that forwards to the DAW. Both are the same type to the
//! backend, which is the point of the injection.

use std::sync::Mutex;

use plugin_host_api::{HostContext, ParamId, RestartReason};

#[derive(Default)]
pub struct CliHost {
    log: Mutex<Vec<String>>,
}

impl CliHost {
    pub fn new() -> CliHost {
        CliHost::default()
    }

    /// Everything the plugin asked of the host during the run.
    ///
    /// Recorded rather than acted on: a renderer has no editor to restart and
    /// no DAW to notify, and seeing *what was requested* is the diagnostic the
    /// harness exists to provide.
    pub fn take_log(&self) -> Vec<String> {
        std::mem::take(&mut self.log.lock().unwrap())
    }

    fn record(&self, message: String) {
        self.log.lock().unwrap().push(message);
    }
}

impl HostContext for CliHost {
    fn host_name(&self) -> &str {
        "audio-graph host-cli"
    }

    fn request_restart(&self, reason: RestartReason) {
        self.record(format!("restart requested: {reason:?}"));
    }

    fn latency_changed(&self, samples: u32) {
        self.record(format!("latency: {samples} samples"));
    }

    fn param_edited(&self, id: ParamId, plain: f64) {
        self.record(format!("plugin edited param {} to {plain}", id.0));
    }
}
