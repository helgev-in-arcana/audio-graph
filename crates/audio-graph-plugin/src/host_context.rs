//! Host context implementation provided by the wrapper to hosted sub-plugins.

use std::sync::atomic::{AtomicBool, Ordering};

use plugin_host::{HostContext, ParamId, RestartReason};

/// The `HostContext` the wrapper injects into the backend.
///
/// Requests from the sub-plugin are recorded rather than acted on inline: they
/// arrive on the sub-plugin's schedule, and the wrapper can only pass them
/// upward at points the DAW allows (activate, or the next process block).
#[derive(Default)]
pub struct WrapperHostContext {
    /// Set when the sub-plugin asked for anything that needs the DAW's
    /// attention, so the wrapper can check cheaply once per block.
    restart_pending: AtomicBool,
    latency_changed: AtomicBool,
}

impl WrapperHostContext {
    pub fn new() -> WrapperHostContext {
        WrapperHostContext::default()
    }

    /// Whether some sub-plugin has said its latency moved since the last ask.
    ///
    /// A flag rather than the number, because every instance the wrapper hosts
    /// reports through this one context: the samples one of them named say
    /// nothing about which one named them, and the answer the DAW wants is the
    /// graph's anyway. What this is for is knowing when to go and look.
    pub fn take_latency_change(&self) -> bool {
        self.latency_changed.swap(false, Ordering::AcqRel)
    }

    pub fn take_restart_request(&self) -> bool {
        self.restart_pending.swap(false, Ordering::AcqRel)
    }
}

impl HostContext for WrapperHostContext {
    fn host_name(&self) -> &str {
        // The sub-plugin sees the wrapper, not the DAW. Some plugins branch on
        // the host name, and claiming to be the DAW would be a lie that shows
        // up as wrong behaviour rather than a nicety.
        "Audio Graph"
    }

    fn request_restart(&self, reason: RestartReason) {
        log::debug!("sub-plugin requested restart: {reason:?}");
        self.restart_pending.store(true, Ordering::Release);
    }

    fn latency_changed(&self, _samples: u32) {
        self.latency_changed.store(true, Ordering::Release);
    }

    fn param_edited(&self, id: ParamId, plain: f64) {
        // Parameter edits from the sub-plugin GUI are not forwarded upstream
        // because the wrapper graph acts as the authoritative source of parameter
        // values. Logged for diagnostic purposes.
        log::trace!(
            "sub-plugin edited param {} to {plain} (not forwarded)",
            id.0
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_changes_are_taken_once() {
        let ctx = WrapperHostContext::new();
        assert!(!ctx.take_latency_change());

        ctx.latency_changed(256);
        assert!(ctx.take_latency_change());
        // Taken means taken: recompiling the graph and telling the DAW again
        // for a change already acted on would restart processing for nothing.
        assert!(!ctx.take_latency_change());
    }

    #[test]
    fn restart_requests_collapse_until_taken() {
        let ctx = WrapperHostContext::new();
        ctx.request_restart(RestartReason::ParamValues);
        ctx.request_restart(RestartReason::ParamTitles);
        assert!(ctx.take_restart_request());
        assert!(!ctx.take_restart_request());
    }
}
