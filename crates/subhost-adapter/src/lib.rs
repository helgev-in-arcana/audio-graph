//! Everything specific to hosting a plugin from *inside* another plugin.
//!
//! ARCHITECTURE.md §7 defines this crate by subtraction: if a standalone
//! offline renderer or a plugin scanner would still need a piece of code, it
//! belongs in `vst3-host`, not here. What is left is the nesting itself —
//! forwarding the DAW's transport down, combining latency on the way up,
//! nesting one plugin's state inside another's, and deciding what to do with
//! the sub-plugin's own edit notifications.

mod host;
mod schedule;
mod slots;
mod state;

pub use host::{
    GraphNodes, SubHost, SubHostConfig, SubHostProcessor, SubHostProcessors, SubPluginRef,
};
pub use schedule::{DEFAULT_QUANTUM, MIN_QUANTUM, QUANTUM_CHOICES, SlotSchedule};
pub use slots::{Binding, ResolvedTarget, Slot, SlotTable};
pub use state::{InstanceState, WrapperState};

/// How the wrapper reports its own latency to the DAW.
///
/// §7.4: the DAW must be told the sub-plugin's latency plus whatever the
/// wrapper adds, and a change from below has to be propagated upward rather
/// than absorbed — a host that is not told will leave the track misaligned.
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
