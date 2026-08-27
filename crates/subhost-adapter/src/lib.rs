//! Everything specific to hosting a plugin from *inside* another plugin.
//!
//! Defined by subtraction: if a standalone offline renderer or a plugin scanner
//! would still need a piece of code, it belongs in `plugin-host`, not here.
//! What is left is the nesting itself — forwarding the DAW's transport down,
//! combining latency on the way up, publishing slots the DAW can automate and
//! binding them to the sub-plugin's own parameters, nesting one plugin's state
//! inside another's, and deciding what to do with the sub-plugin's edit
//! notifications.
//!
//! Defined by subtraction the other way too: nothing here knows what AudioGraph
//! is. The wrapper above decides how many slots to publish, how many lanes a
//! sub-block carries and what its saved document looks like, and hands those in
//! ([`SubHostConfig`], [`SlotSchedule`], [`SubHostState`]); a different wrapper
//! — a chain, a rack, a bare pair of plugins — makes different choices and gets
//! the same crate. [`AudioNodes`] is where that line is drawn: a caller schedules
//! audio and says which instance hears what, and everything crossing back is a
//! flat slice or a `Copy` value.

mod host;
mod nodes;
mod schedule;
mod slots;
mod state;

pub use host::{
    GraphNodes, SubHost, SubHostConfig, SubHostProcessor, SubHostProcessors, SubPluginRef,
};
pub use nodes::{AudioChunk, AudioNodes, InstanceIo, NoNodes, NoteSource, NoteStream, ParamTarget};
pub use schedule::{DEFAULT_QUANTUM, MIN_QUANTUM, QUANTUM_CHOICES, SlotSchedule};
pub use slots::{Binding, ResolvedTarget, Slot, SlotTable};
pub use state::{InstanceState, SubHostState, base64_decode, base64_encode};

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
