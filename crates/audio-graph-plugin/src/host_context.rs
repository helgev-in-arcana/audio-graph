//! What the wrapper offers the sub-plugin as its host (ARCHITECTURE.md §7).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use plugin_host::{HostContext, ParamId, RestartReason};

/// The `HostContext` the wrapper injects into the backend.
///
/// Requests from the sub-plugin are recorded rather than acted on inline: they
/// arrive on the sub-plugin's schedule, and the wrapper can only pass them
/// upward at points the DAW allows (activate, or the next process block).
#[derive(Default)]
pub struct WrapperHostContext {
    latency: AtomicU32,
    /// Set when the sub-plugin asked for anything that needs the DAW's
    /// attention, so the wrapper can check cheaply once per block.
    restart_pending: AtomicBool,
    latency_changed: AtomicBool,
}

impl WrapperHostContext {
    pub fn new() -> WrapperHostContext {
        WrapperHostContext::default()
    }

    pub fn latency(&self) -> u32 {
        self.latency.load(Ordering::Relaxed)
    }

    /// Take the pending latency change, if any.
    pub fn take_latency_change(&self) -> Option<u32> {
        self.latency_changed
            .swap(false, Ordering::AcqRel)
            .then(|| self.latency.load(Ordering::Acquire))
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

    fn latency_changed(&self, samples: u32) {
        self.latency.store(samples, Ordering::Release);
        self.latency_changed.store(true, Ordering::Release);
    }

    fn param_edited(&self, id: ParamId, plain: f64) {
        // Swallowed on purpose (§7.5). In Drive mode the wrapper is the sole
        // authority for parameter values, so a notification that the
        // sub-plugin's own GUI moved a knob has nothing to tell the DAW —
        // forwarding it would create automation the user never asked for.
        // Logged rather than dropped silently, because it is genuinely useful
        // when working out why a value did not stick.
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
        assert_eq!(ctx.take_latency_change(), None);

        ctx.latency_changed(256);
        assert_eq!(ctx.take_latency_change(), Some(256));
        // Taken means taken: reporting the same change twice would make the
        // DAW restart processing for no reason.
        assert_eq!(ctx.take_latency_change(), None);
        assert_eq!(ctx.latency(), 256);
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
