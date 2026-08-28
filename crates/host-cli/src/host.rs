//! Implementation of [`HostContext`] for the CLI harness.
//!
//! Provides a host context that records notifications (such as restart requests,
//! latency changes, and parameter edits) into an in-memory log for diagnostic inspection.

use std::sync::Mutex;

use plugin_host::{HostContext, ParamId, RestartReason};

#[derive(Default)]
pub struct CliHost {
    log: Mutex<Vec<String>>,
}

impl CliHost {
    pub fn new() -> CliHost {
        CliHost::default()
    }

    /// Returns and clears all recorded host notifications.
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
