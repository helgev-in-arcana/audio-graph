//! Both halves of a delay line (§14.4).
//!
//! One node in the user's head, two in the graph. The split is what keeps a
//! cycle out of the topological sort: the halves are paired by `line`, never
//! by an edge, so the compiler walks a graph that is still acyclic even when
//! the signal is not. That is ADR-8, and it is the reason these two live in
//! one file — they are the only pair in the node set that has to agree about
//! anything.

use serde::{Deserialize, Serialize};

use crate::graph::LineId;
use crate::port::PortType;

/// The writing half of a delay line (§14.4).
///
/// Has an input and no output, so a graph that goes through a delay has no
/// cycle for the topological sort to find. That is the whole mechanism: the
/// two halves are paired by `line`, never by an edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelayWrite {
    pub line: LineId,
    pub ty: PortType,
}

/// The reading half of a delay line (§14.4).
///
/// Has an output and no input. Several reads may share one line — that is a
/// multi-tap delay, and it falls out for free.
///
/// `time` is in seconds and is clamped at run time to the floor of §14.4;
/// the compiler cannot do the clamping itself because the floor depends on
/// the sample rate and the sub-block size, neither of which it knows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DelayRead {
    pub line: LineId,
    pub ty: PortType,
    /// Longest delay this line will ever be asked for. Not automatable: the
    /// ring is allocated for it at activate, and §9.1 forbids allocating in
    /// `process`.
    pub max_time: f64,
    pub time: f64,
}
