//! Which part of a program has to run a sub-block at a time.
//!
//! Links may not form a cycle — [`visit`][super::visit] refuses one — so the
//! only way a value can reach itself is through a delay line, which is a pair
//! of nodes sharing a number rather than an edge. That makes the question here
//! a small one: everything a delay line's two ends can reach between them is a
//! strongly connected component of the graph you get by drawing the line back
//! in, and nothing else is.
//!
//! It matters because a delay is at least one chunk long. A whole-block chunk
//! puts the floor at the DAW's block — ten milliseconds at 48 kHz, which is
//! not a delay anybody asked for — so the ops on either end of a line have to
//! run at the quantum instead. That is a reason to call *those* ops more
//! often. It has never been a reason to call a synth in another corner of the
//! patch sixteen times for one block, which is what a program-wide answer did.

use crate::compile::{Line, NO_WRITER};
use crate::graph::{Graph, NodeId};
use crate::nodes::NodeKind;
use crate::port::PortType;

/// Where a node runs relative to the program's delay lines.
///
/// The three are a valid order to run them in: no edge ever points from a
/// later one to an earlier one. See [`places`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    /// Upstream of a line, or in a program that has none — which is where
    /// every node is until someone draws one.
    Before,
    /// On a path between the two ends of a line, or an end itself. The part
    /// that runs a sub-block at a time.
    Looped,
    /// Everything else: downstream of a line, or beside it.
    After,
}

/// The places in the order their ops run.
pub(crate) const RUN_ORDER: [Place; 3] = [Place::Before, Place::Looped, Place::After];

/// One [`Place`] per entry of `order`.
///
/// `Looped` is the set of nodes lying between the ends of some closed audio
/// line — reachable from an end and reaching an end. For a feedback loop that
/// is the whole loop; for a plain delay, where the read cannot reach the
/// write, it is just the two ends, and everything they are wired to keeps
/// running once a block.
///
/// `Before` is what reaches that set without being in it, and `After` is the
/// rest. Nothing in `Looped` points into `Before`: an edge from a looped node
/// to a node that reaches a looped node would put its own target in the set.
/// Nothing in `After` points into either, because a node that reaches the set
/// is in `Before` by definition.
pub(crate) fn places(graph: &Graph, order: &[NodeId], lines: &[Line]) -> Vec<Place> {
    let n = order.len();
    let at = |id: NodeId| order.iter().position(|&node| node == id);

    let mut forward = vec![Vec::new(); n];
    let mut backward = vec![Vec::new(); n];
    for link in &graph.links {
        if let (Some(from), Some(to)) = (at(link.from), at(link.to)) {
            forward[from].push(to);
            backward[to].push(from);
        }
    }

    // Both ends of every audio line that has both. A line missing one end can
    // carry nothing back: an unread write is dropped on the floor, and an
    // unwritten read is silence whenever it is asked.
    let mut ends = Vec::new();
    for line in lines {
        if !matches!(line.ty, PortType::Audio { .. }) || line.writer == NO_WRITER {
            continue;
        }
        let readers: Vec<usize> = order
            .iter()
            .enumerate()
            .filter(|&(_, &id)| {
                matches!(graph.node(id).map(|node| &node.kind),
                    Some(NodeKind::DelayRead(read)) if read.line == line.id)
            })
            .map(|(index, _)| index)
            .collect();
        if readers.is_empty() {
            continue;
        }
        ends.extend(readers);
        ends.extend(at(line.writer));
    }

    if ends.is_empty() {
        return vec![Place::Before; n];
    }

    let downstream = spread(&ends, &forward, n);
    let upstream = spread(&ends, &backward, n);
    let looped: Vec<usize> = (0..n).filter(|&i| downstream[i] && upstream[i]).collect();
    let feeds = spread(&looped, &backward, n);

    let mut places = vec![Place::After; n];
    for index in 0..n {
        places[index] = if downstream[index] && upstream[index] {
            Place::Looped
        } else if feeds[index] {
            Place::Before
        } else {
            Place::After
        };
    }
    places
}

/// Everything `from` reaches along `edges`, including `from` itself.
fn spread(from: &[usize], edges: &[Vec<usize>], n: usize) -> Vec<bool> {
    let mut seen = vec![false; n];
    let mut pending: Vec<usize> = Vec::new();
    for &start in from {
        if !seen[start] {
            seen[start] = true;
            pending.push(start);
        }
    }
    while let Some(node) = pending.pop() {
        for &next in &edges[node] {
            if !seen[next] {
                seen[next] = true;
                pending.push(next);
            }
        }
    }
    seen
}
