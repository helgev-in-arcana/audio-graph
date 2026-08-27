//! The ceilings this wrapper is built with.
//!
//! They live here rather than in `subhost-adapter` because they are AudioGraph's
//! numbers, not facts about hosting a plugin inside another one: the adapter
//! takes all three as a [`SubHostConfig`][subhost_adapter::SubHostConfig] and
//! never names one itself.

use subhost_adapter::SubHostConfig;

/// How many slots the wrapper publishes to the DAW (ARCHITECTURE.md §4.6).
///
/// Fixed because VST3 cannot add parameters at runtime. CLAP can, and will, but
/// nothing below this line sees anything but an abstract slot table.
pub const SLOT_COUNT: usize = 32;

/// How many plugin nodes one patch may hold (§4.4).
///
/// A ceiling rather than guidance: the graph names an instance by index and the
/// buffer pool is sized at activate, so the number has to be known before the
/// user starts drawing.
pub const MAX_INSTANCES: usize = 16;

/// How many values one sub-block of the schedule carries (§4.6).
///
/// The DAW's slots, then one lane per sub-plugin parameter the graph drives
/// directly, then one per audio-side control it automates — a delay time or a
/// Mix gain. One buffer rather than three because they are produced by the same
/// evaluator pass and consumed by the same merge: the evaluator writes a lane
/// exactly the way it writes a slot, and nothing below the compiler has to know
/// which is which.
///
/// The ranges are disjoint and fixed, which is what lets each consumer read only
/// its own: the sub-plugin adapter never sees a delay time or a gain, and the
/// audio half never sees a parameter.
pub const LANES: usize =
    SLOT_COUNT + audio_graph_engine::MAX_GRAPH_PARAMS + audio_graph_engine::MAX_AUDIO_LANES;

/// What every `SubHost` in this wrapper is built with.
pub const SUB_HOST: SubHostConfig = SubHostConfig {
    max_instances: MAX_INSTANCES,
    slot_count: SLOT_COUNT,
    lanes: LANES,
};
