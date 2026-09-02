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
//! often. It is never a reason to call a synth in another corner of the patch
//! sixteen times for one block, which is what a program-wide answer would do.
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

/// Which loop a node belongs to, if any.
///
/// A node is inside a loop when it lies between the two ends of *one* line:
/// reachable from an end of that line and reaching an end of it. Asking the
/// question of every line's ends at once, as one set, answers yes for anything
/// on a path from one loop to another — so a chorus between two feedback
/// delays would be called sixteen times a block for belonging to neither.
///
/// Loops sharing a node are one component, because a value in either reaches
/// itself through the other and no member can be said to come first.
type Component = Option<usize>;

/// Cuts the graph into stages.
///
/// A node's stage is the pair `(level, inside a loop)`. The level is how many
/// boundaries the longest path to it crosses, and a boundary is either a
/// change of rate — audio into something that makes anything else — or a step
/// into or out of a loop, which has to be a stage of its own because a stage
/// runs at one granularity.
///
/// Two nodes at one level with an edge between them are on the same side of
/// that pair by construction: an edge crossing it would have been a boundary
/// and put them on different levels. So sorting by the pair gives an order
/// every edge points forwards along, whichever way the second half is read.
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

    let components = components(graph, order, lines, &forward, &backward, &at);
    let levels = levels(&components, &forward, &cuts, n);

    // One stage per distinct key, in the order they run.
    let keys: Vec<(u16, bool)> = (0..n)
        .map(|index| (levels[index], components[index].is_some()))
        .collect();
    let mut distinct = keys.clone();
    distinct.sort_unstable();
    distinct.dedup();

    Plan {
        stages: distinct
            .iter()
            .map(|&(_, looped)| {
                if looped {
                    Chunking::SubBlock
                } else {
                    Chunking::WholeBlock
                }
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

/// Which loop each node belongs to. See [`Component`].
fn components(
    graph: &Graph,
    order: &[NodeId],
    lines: &[Line],
    forward: &[Vec<usize>],
    backward: &[Vec<usize>],
    at: &impl Fn(NodeId) -> Option<usize>,
) -> Vec<Component> {
    let n = order.len();

    // One region per audio line that has both its ends. A line missing one can
    // carry nothing back: an unread write is dropped on the floor, and an
    // unwritten read is silence whenever it is asked.
    let mut regions: Vec<Vec<bool>> = Vec::new();
    for line in lines {
        if !matches!(line.ty, PortType::Audio { .. }) || line.writer == NO_WRITER {
            continue;
        }
        let mut ends: Vec<usize> = order
            .iter()
            .enumerate()
            .filter(|&(_, &id)| {
                matches!(graph.node(id).map(|node| &node.kind),
                    Some(NodeKind::DelayRead(read)) if read.line == line.id)
            })
            .map(|(index, _)| index)
            .collect();
        if ends.is_empty() {
            continue;
        }
        ends.extend(at(line.writer));

        let downstream = spread(&ends, forward, n);
        let upstream = spread(&ends, backward, n);
        regions.push(
            (0..n)
                .map(|index| downstream[index] && upstream[index])
                .collect(),
        );
    }

    // Two lines sharing a node are one loop: a value in either comes back to
    // itself through the other, so neither can be said to run first.
    let mut merged = true;
    while merged {
        merged = false;
        for a in 0..regions.len() {
            for b in (a + 1)..regions.len() {
                if (0..n).any(|index| regions[a][index] && regions[b][index]) {
                    let other = regions.swap_remove(b);
                    for (into, from) in regions[a].iter_mut().zip(other) {
                        *into |= from;
                    }
                    merged = true;
                    break;
                }
            }
            if merged {
                break;
            }
        }
    }

    let mut components = vec![None; n];
    for (which, region) in regions.iter().enumerate() {
        for (index, &inside) in region.iter().enumerate() {
            if inside {
                components[index] = Some(which);
            }
        }
    }
    components
}

/// How many boundaries the longest path to each node crosses.
///
/// A boundary is a change of rate, or a step into or out of a loop — the
/// second because a stage runs at one granularity, so a loop cannot share one
/// with what feeds it or what it feeds.
///
/// Everything inside one loop ends up on the same level, because a loop runs
/// as a piece: a value in it reaches itself, so no member can be said to come
/// before another. Levelling them by hand is what makes that true — a member
/// with an upstream neighbour outside the loop would otherwise sit a level
/// above one without. Relaxed to a fixed point rather than walked in order,
/// because collapsing the loops that way is what makes an order exist at all.
fn levels(
    components: &[Component],
    forward: &[Vec<usize>],
    cuts: &[Vec<bool>],
    n: usize,
) -> Vec<u16> {
    let loops = components
        .iter()
        .flatten()
        .copied()
        .max()
        .map_or(0, |m| m + 1);
    let mut levels = vec![0u16; n];
    for _ in 0..=n {
        let mut moved = false;
        for from in 0..n {
            for (&next, &cut) in forward[from].iter().zip(&cuts[from]) {
                let boundary = cut || components[from] != components[next];
                let want = levels[from] + u16::from(boundary);
                if levels[next] < want {
                    levels[next] = want;
                    moved = true;
                }
            }
        }
        for which in 0..loops {
            let inside = (0..n)
                .filter(|&index| components[index] == Some(which))
                .map(|index| levels[index])
                .max()
                .unwrap_or(0);
            for (index, level) in levels.iter_mut().enumerate() {
                if components[index] == Some(which) && *level < inside {
                    *level = inside;
                    moved = true;
                }
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
