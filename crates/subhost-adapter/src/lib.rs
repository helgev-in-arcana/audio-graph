//! Hosting sub-plugins inside another plugin.
//!
//! Provides the sub-hosting adapter layer: forwarding transport, combining
//! latency, publishing automatable parameter slots and binding them to
//! sub-plugin parameters, managing nested sub-plugin state, and scheduling
//! audio processing across multiple sub-plugin instances.

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
/// Combines the sub-plugin's reported latency with any additional latency
/// introduced by the wrapper so the host DAW can perform latency compensation.
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
        // Prevent arithmetic overflow if a sub-plugin reports an extreme latency value.
        let report = LatencyReport {
            sub_plugin: u32::MAX,
            wrapper: 64,
        };
        assert_eq!(report.total(), u32::MAX);
    }
}
