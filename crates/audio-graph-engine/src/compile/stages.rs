//! Where a program has to be cut, and what each piece runs at.
//!
//! A stage is a run of ops that execute together over the whole DAW block:
//! parameters first, then notes, then audio. Two things ask for a cut between
//! one stage and the next.
//!
//! A delay line is at least one chunk long, so the ops on either end of one
//! have to run at the quantum — a whole-block chunk would put the floor at ten
//! milliseconds. Links may not form a cycle ([`visit`][super::visit] refuses
//! one), so the only way a value reaches itself is through a line, and finding
//! what runs inside a loop is two reachability sweeps from the line's ends
//! rather than anything cleverer. That is a reason to call *those* ops more
//! often. It has never been a reason to call a synth in another corner of the
//! patch sixteen times for one block, which is what a program-wide answer did.
//!
//! And a parameter read off audio cannot be worked out until that audio
//! exists. That cut is the whole point of an envelope follower: the stage
//! holding it runs after the stage that made the sound it is measuring, which
//! is what lets a sidechain be drawn on the canvas rather than wired behind
//! it.

use crate::compile::{Line, NO_WRITER};
use crate::graph::{Graph, NodeId};
use crate::ir::Chunking;
use crate::nodes::NodeKind;
use crate::port::PortType;

/// How a program's nodes divide into stages.
pub(crate) struct Plan {
    /// What each stage runs at, in the order they run.
    pub stages: Vec<Chunking>,
    /// Which stage each entry of `order` belongs to.
    pub of: Vec<usize>,
}

/// Where a node sits relative to the program's delay lines.
///
/// A valid order to run them in: no edge points from a later one to an earlier
/// one. `Looped` holds the nodes between the two ends of some closed audio
/// line — reachable from an end and reaching an end. For a feedback loop that
/// is the whole loop; for a plain delay, where the read cannot reach the
/// write, it is just the two ends, and everything they are wired to keeps
/// running once a block.
///
/// Nothing in `Looped` points into `Before`: an edge from a looped node to a
/// node that reaches a looped node would put its own target in the set.
/// Nothing in `After` points into either, because a node that reaches the set
/// is in `Before` by definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Place {
    Before,
    Looped,
    After,
}

/// Cuts the graph into stages.
///
/// A node's stage is the pair `(level, place)`: how many times the signal has
/// changed rate on the longest path to it, and where it sits relative to the
/// loops. Each respects the topological order on its own, so the pair does
/// too, and sorting by it gives an order every edge points forwards along.
pub(crate) fn plan(graph: &Graph, order: &[NodeId], lines: &[Line]) -> Plan {
    let n = order.len();
    let at = |id: NodeId| order.iter().position(|&node| node == id);

    let mut forward = vec![Vec::new(); n];
    let mut backward = vec![Vec::new(); n];
    // Whether crossing an edge changes the rate the signal is carried at,
    // parallel to `forward` so the two walk together.
    let mut cuts = vec![Vec::new(); n];
    for link in &graph.links {
        let (Some(from), Some(to)) = (at(link.from), at(link.to)) else {
            continue;
        };
        forward[from].push(to);
        cuts[from].push(rate_cut(graph, link.from, link.from_port, link.to));
        backward[to].push(from);
    }

    let looped = looped(graph, order, lines, &forward, &backward, &at);
    let places = places(&looped, &backward, n);
    let levels = levels(&looped, &forward, &cuts, n);

    // One stage per distinct key, in the order they run.
    let keys: Vec<(u16, Place)> = (0..n).map(|i| (levels[i], places[i])).collect();
    let mut distinct = keys.clone();
    distinct.sort_unstable();
    distinct.dedup();

    Plan {
        stages: distinct
            .iter()
            .map(|&(_, place)| match place {
                Place::Looped => Chunking::SubBlock,
                _ => Chunking::WholeBlock,
            })
            .collect(),
        of: keys
            .iter()
            .map(|key| distinct.iter().position(|at| at == key).unwrap_or(0))
            .collect(),
    }
}

/// Whether a value crossing this link changes rate.
///
/// Audio into anything that produces something other than audio: how loud a
/// signal is cannot be known before the signal is, so the reader has to wait
/// for a stage boundary. The other direction never waits — a parameter is a
/// value at a sub-block boundary, and every boundary of the block is settled
/// before the audio of the stage it belongs to runs.
fn rate_cut(graph: &Graph, from: NodeId, from_port: u8, to: NodeId) -> bool {
    let carries_audio = graph
        .node(from)
        .and_then(|node| {
            node.kind
                .output_ports()
                .get(from_port as usize)
                .map(|p| p.ty)
        })
        .is_some_and(|ty| matches!(ty, PortType::Audio { .. }));
    let makes_something_else = graph.node(to).is_some_and(|node| {
        node.kind
            .output_ports()
            .iter()
            .any(|port| !matches!(port.ty, PortType::Audio { .. }))
    });
    carries_audio && makes_something_else
}

/// The nodes lying between the two ends of some closed audio line.
fn looped(
    graph: &Graph,
    order: &[NodeId],
    lines: &[Line],
    forward: &[Vec<usize>],
    backward: &[Vec<usize>],
    at: &impl Fn(NodeId) -> Option<usize>,
) -> Vec<bool> {
    let n = order.len();

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
        return vec![false; n];
    }

    let downstream = spread(&ends, forward, n);
    let upstream = spread(&ends, backward, n);
    (0..n).map(|i| downstream[i] && upstream[i]).collect()
}

/// `Before` for what feeds the loops, `After` for the rest.
fn places(looped: &[bool], backward: &[Vec<usize>], n: usize) -> Vec<Place> {
    let seeds: Vec<usize> = (0..n).filter(|&i| looped[i]).collect();
    if seeds.is_empty() {
        return vec![Place::Before; n];
    }
    let feeds = spread(&seeds, backward, n);
    (0..n)
        .map(|index| {
            if looped[index] {
                Place::Looped
            } else if feeds[index] {
                Place::Before
            } else {
                Place::After
            }
        })
        .collect()
}

/// How many rate changes the longest path to each node crosses.
///
/// Everything inside a loop shares one level, because a loop runs as a piece:
/// a value in it reaches itself, so no member can be said to come before
/// another. Relaxed to a fixed point rather than walked in order, because
/// collapsing the loop that way is what makes an order exist at all.
fn levels(looped: &[bool], forward: &[Vec<usize>], cuts: &[Vec<bool>], n: usize) -> Vec<u16> {
    let mut levels = vec![0u16; n];
    for _ in 0..=n {
        let mut moved = false;
        for from in 0..n {
            for (&next, &cut) in forward[from].iter().zip(&cuts[from]) {
                let want = levels[from] + u16::from(cut);
                if levels[next] < want {
                    levels[next] = want;
                    moved = true;
                }
            }
        }
        let inside = (0..n)
            .filter(|&index| looped[index])
            .map(|index| levels[index])
            .max()
            .unwrap_or(0);
        for (index, level) in levels.iter_mut().enumerate() {
            if looped[index] && *level < inside {
                *level = inside;
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
    levels
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
