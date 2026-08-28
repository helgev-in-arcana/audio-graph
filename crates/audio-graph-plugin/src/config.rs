//! Capacity limits and configuration constants for the wrapper plugin.
//!
//! These constants define AudioGraph's limits and are passed to
//! [`SubHostConfig`][subhost_adapter::SubHostConfig].

use subhost_adapter::SubHostConfig;

/// Number of parameter slots the wrapper publishes to the host DAW.
///
/// Fixed at compile time because formats such as VST3 cannot add parameters at runtime.
/// Host automation drives these abstract slots, which the user can bind to sub-plugin parameters.
pub const SLOT_COUNT: usize = 32;

/// Maximum number of plugin nodes (sub-plugin instances) a single patch may hold.
///
/// Serves as a fixed ceiling so that instance indexing and buffer pools can be
/// sized during activation before nodes are created.
pub const MAX_INSTANCES: usize = 16;

/// Total number of automation and control lanes carried per schedule sub-block.
///
/// Includes DAW parameter slots, graph-driven sub-plugin parameter lanes, and
/// audio-side control lanes (such as delay times or mix gains). Storing all lanes
/// in a single contiguous buffer allows a unified evaluation pass with disjoint,
/// fixed index ranges for each consumer.
pub const LANES: usize =
    SLOT_COUNT + audio_graph_engine::MAX_GRAPH_PARAMS + audio_graph_engine::MAX_AUDIO_LANES;

/// Configuration used to initialize each [`SubHost`][subhost_adapter::SubHost] in this wrapper.
pub const SUB_HOST: SubHostConfig = SubHostConfig {
    max_instances: MAX_INSTANCES,
    slot_count: SLOT_COUNT,
    lanes: LANES,
};
