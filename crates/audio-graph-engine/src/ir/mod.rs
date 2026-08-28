//! Compiled intermediate representation: a flat sequence of instructions over register files and audio buffers.
//!
//! A [`Program`] represents the immutable, pre-compiled execution plan for the audio thread.
//! It contains no dynamic memory allocations, lookups, or references to the mutable edit graph.
//! Dynamic execution state (such as LFO phases and delay line contents) is maintained by the [`Engine`][crate::Engine].
//!
//! This module is decoupled from the edit graph types (`graph`, `nodes`, `port`), ensuring the compiled
//! program remains a self-contained value.

mod audio_op;
mod op;

pub use audio_op::{AudioOp, Buf, Chunking, MixIn, NoteRoute};

pub use op::{ExprSource, MathOp, Op, Operand, RateSpec, Reg, Waveform};
use subhost_adapter::{InstanceIo, ParamTarget};

/// Unique identifier for a node, persistent across graph recompilations.
///
/// Used by [`Program`] and [`Engine`][crate::Engine] to match dynamic state
/// (e.g. LFO phases, delay lines, latches) across program updates.
pub type NodeId = u32;

/// Maximum number of plugin parameters a single graph can drive directly.
pub const MAX_GRAPH_PARAMS: usize = 64;

/// Ceilings for preallocated engine resources to prevent audio-thread allocations.
pub const MAX_REGISTERS: usize = 256;
pub const MAX_LFOS: usize = 64;

/// Maximum number of latches (key switches) supported per program.
pub const MAX_LATCHES: usize = 64;
pub const MAX_DELAY_LINES: usize = 16;

/// Maximum delay line depth for parameter delays, measured in sub-blocks.
pub const MAX_DELAY_TAPS: usize = 4096;

/// Maximum number of audio delay lines supported per program.
pub const MAX_AUDIO_DELAY_LINES: usize = 8;
/// Maximum audio delay duration in seconds.
pub const MAX_AUDIO_DELAY_SECONDS: f64 = 10.0;

/// Maximum number of audio-rate control lanes (e.g. dynamic delay times or gains).
pub const MAX_AUDIO_LANES: usize = 16;

/// Maximum number of latency-compensated signal branches and max sample delay.
pub const MAX_COMPENSATORS: usize = 8;
pub const MAX_COMPENSATION: usize = 32_768;

/// Ceiling on audio buffer pool allocation.
pub const MAX_BUFFERS: usize = 64;

/// Maximum number of channels in a standard audio bus (stereo).
pub const MAX_CHANNELS: usize = 2;

/// Maximum number of channels across all buses (main and aux) packed into a single buffer.
pub const MAX_BUFFER_CHANNELS: usize = MAX_CHANNELS * (1 + MAX_AUX_BUSES);

pub use plugin_host::MAX_AUX_BUSES;

/// A compiled execution program representing an audio and control graph.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    /// Topologically ordered scalar operations. Every `Op` reads only registers already written.
    pub ops: Vec<Op>,
    pub registers: usize,
    /// Driven lane index and source register mappings. Sorted by lane.
    pub outputs: Vec<(u16, Reg)>,
    /// Sample capacity per channel for each audio delay ring.
    pub audio_ring_len: Vec<usize>,
    /// Audio delay ring buffers allocated on the main thread and passed to the engine.
    pub audio_rings: Vec<Vec<f32>>,
    /// Maximum delay duration in seconds per audio delay line.
    pub audio_ring_seconds: Vec<f64>,
    /// Node ID associated with each audio delay line for state preservation across program swaps.
    pub audio_delay_nodes: Vec<NodeId>,
    /// Node ID associated with each parameter delay line for state preservation across program swaps.
    pub delay_nodes: Vec<NodeId>,
    /// Audio processing operations in topological execution order.
    pub audio_ops: Vec<AudioOp>,
    /// Plugin parameter mapping for graph-driven lanes.
    pub param_targets: Vec<ParamTarget>,
    /// Base lane index for audio-specific lanes (e.g. delay times or gains).
    pub audio_lane_base: u16,
    /// Audio I/O configuration required for each plugin instance.
    pub instances: Vec<InstanceIo>,
    /// Channel width of each buffer in the audio pool.
    pub buffers: Vec<u16>,
    /// Execution granularity for audio operations (sub-block vs whole block).
    pub chunking: Chunking,
    /// Total graph latency in samples after delay compensation.
    pub latency: u32,
    /// Node ID associated with each latch state for state preservation across program swaps.
    pub latch_nodes: Vec<NodeId>,
    /// Node ID associated with each LFO state for phase preservation across program swaps.
    pub lfo_nodes: Vec<NodeId>,
}

impl Program {
    /// Returns an empty program that performs no operations.
    pub fn empty() -> Program {
        Program {
            ops: Vec::new(),
            registers: 0,
            outputs: Vec::new(),
            audio_ops: Vec::new(),
            param_targets: Vec::new(),
            audio_lane_base: 0,
            instances: Vec::new(),
            buffers: Vec::new(),
            chunking: Chunking::WholeBlock,
            latency: 0,
            delay_nodes: Vec::new(),
            audio_delay_nodes: Vec::new(),
            audio_ring_len: Vec::new(),
            audio_rings: Vec::new(),
            audio_ring_seconds: Vec::new(),
            lfo_nodes: Vec::new(),
            latch_nodes: Vec::new(),
        }
    }

    /// Sizes and allocates audio delay ring buffers on the main thread based on sample rate.
    ///
    /// Reuses existing allocations from `previous` if the length requirements match.
    /// Returns the active `(NodeId, usize)` pairings.
    pub fn size_rings(
        &mut self,
        sample_rate: f64,
        previous: &[(NodeId, usize)],
    ) -> Vec<(NodeId, usize)> {
        let ceiling = (MAX_AUDIO_DELAY_SECONDS * sample_rate.max(1.0)) as usize;
        self.audio_ring_len = self
            .audio_ring_seconds
            .iter()
            // Extra samples added for Hermite interpolation lookahead past fractional read pointers.
            .map(|&s| ((s.max(0.0) * sample_rate).ceil() as usize + 4).clamp(64, ceiling))
            .collect();
        let want: Vec<(NodeId, usize)> = self
            .audio_delay_nodes
            .iter()
            .copied()
            .zip(self.audio_ring_len.iter().copied())
            .collect();
        self.audio_rings = want
            .iter()
            .map(|entry| {
                if previous.contains(entry) {
                    Vec::new()
                } else {
                    vec![0.0; MAX_CHANNELS * entry.1]
                }
            })
            .collect();
        want
    }

    /// Returns true if the program writes to the specified lane.
    pub fn drives_lane(&self, lane: usize) -> bool {
        u16::try_from(lane).is_ok_and(|l| self.outputs.iter().any(|&(o, _)| o == l))
    }

    /// Returns true if running this program produces no observable outputs or audio operations.
    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty() && self.audio_ops.is_empty()
    }
}
