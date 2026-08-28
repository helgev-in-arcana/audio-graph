//! Hosting sub-plugins inside another plugin.
//!
//! Provides the sub-hosting adapter layer: forwarding transport, combining
//! latency, publishing automatable parameter slots and binding them to
//! sub-plugin parameters, managing nested sub-plugin state, and scheduling
//! audio processing across multiple sub-plugin instances.
//!
//! The scope is defined by subtraction in both directions.
//!
//! Downward: if a standalone offline renderer or a plugin scanner would still
//! need a piece of code, it belongs in `plugin-host`, not here. What is left is
//! the nesting itself.
//!
//! Upward: nothing here knows what AudioGraph is. The wrapper above decides how
//! many slots to publish, how many lanes a sub-block carries and what its saved
//! document looks like, and hands those in ([`SubHostConfig`],
//! [`SlotSchedule`], [`SubHostState`]); a different wrapper — a chain, a rack,
//! a bare pair of plugins — makes different choices and gets the same crate.
//! [`AudioInstances`] is where that line is drawn.
//!
//! See `README.md` in this crate for the invariants that boundary depends on.

mod host;
mod instances;
mod schedule;
mod slots;
mod state;

pub use host::{
    BoundInstances, SubHost, SubHostConfig, SubHostProcessor, SubHostProcessors, SubPluginRef,
};
pub use instances::{
    AudioChunk, AudioInstances, InstanceIo, NoInstances, NoteSource, NoteStream, ParamTarget,
};
pub use schedule::{DEFAULT_QUANTUM, MIN_QUANTUM, QUANTUM_CHOICES, SlotSchedule};
pub use slots::{Binding, ResolvedTarget, Slot, SlotTable};
pub use state::{InstanceState, SubHostState, base64_decode, base64_encode};

/// Latency reported by the wrapper to the host DAW.
///
/// Combines the sub-plugin's reported latency with any additional latency the
/// wrapper adds. A change from below has to be propagated upward rather than
/// absorbed: a host that is not told will leave the track misaligned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LatencyReport {
    pub sub_plugin: u32,
    pub wrapper: u32,
}

impl LatencyReport {
    pub fn total(&self) -> u32 {
        self.sub_plugin.saturating_add(self.wrapper)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_is_the_sum_of_both_stages() {
        let report = LatencyReport {
            sub_plugin: 512,
            wrapper: 64,
        };
        assert_eq!(report.total(), 576);
    }

    #[test]
    fn latency_saturates_rather_than_wrapping() {
        // A plugin reporting a nonsense latency should not make the wrapper
        // report a tiny one, which is what a wrapping add would do.
        let report = LatencyReport {
            sub_plugin: u32::MAX,
            wrapper: 64,
        };
        assert_eq!(report.total(), u32::MAX);
    }
}
