// ============================================================================
//
// HUMAN REVIEW REQUIRED: THIS FILE HAS NOT BEEN REVIEWED BY A HUMAN.
//
// ============================================================================

//! Host context implementation provided by the wrapper to hosted sub-plugins.

use std::sync::atomic::{AtomicBool, Ordering};

use plugin_host::{ParamId, RestartReason};
use subhost_adapter::{InstanceId, SubHostContext};

/// The child notifications the wrapper combines for its parent DAW.
///
/// Requests from the sub-plugin are recorded rather than acted on inline: they
/// arrive on the sub-plugin's schedule, and the wrapper can only pass them
/// upward at points the DAW allows (activate, or the next process block).
#[derive(Default)]
pub struct WrapperHostContext {
    latency_changed: AtomicBool,
}

impl WrapperHostContext {
    pub fn new() -> WrapperHostContext {
        WrapperHostContext::default()
    }

    /// Whether some sub-plugin has said its latency moved since the last ask.
    ///
    /// The parent needs the graph's total, so any child's change requests a fresh calculation.
    pub fn take_latency_change(&self) -> bool {
        self.latency_changed.swap(false, Ordering::AcqRel)
    }
}

impl SubHostContext for WrapperHostContext {
    fn host_name(&self) -> &str {
        // The sub-plugin sees the wrapper, not the DAW. Some plugins branch on
        // the host name, and claiming to be the DAW would be a lie that shows
        // up as wrong behaviour rather than a nicety.
        "Audio Graph"
    }

    fn request_restart(&self, source: InstanceId, reason: RestartReason) {
        // Nothing to record: the backend marks the child's metadata as stale,
        // and the tick's `refresh_metadata` is what acts on it.
        log::debug!("sub-plugin {source:?} requested restart: {reason:?}");
    }

    fn latency_changed(&self, _source: InstanceId, _samples: u32) {
        self.latency_changed.store(true, Ordering::Release);
    }

    fn param_edited(&self, source: InstanceId, id: ParamId, plain: f64) {
        // Parameter edits from the sub-plugin GUI are not forwarded upstream
        // because the wrapper graph acts as the authoritative source of parameter
        // values. Logged for diagnostic purposes.
        log::trace!(
            "sub-plugin {source:?} edited param {} to {plain} (not forwarded)",
            id.0
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SOURCE: InstanceId = InstanceId {
        index: 0,
        generation: 1,
    };

    #[test]
    fn latency_changes_are_taken_once() {
        let ctx = WrapperHostContext::new();
        assert!(!ctx.take_latency_change());

        ctx.latency_changed(SOURCE, 256);
        assert!(ctx.take_latency_change());
        // Taken means taken: recompiling the graph and telling the DAW again
        // for a change already acted on would restart processing for nothing.
        assert!(!ctx.take_latency_change());
    }
}
