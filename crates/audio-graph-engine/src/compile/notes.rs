//! Note pipeline compilation pass.
//!
//! Runs *before* the parameter and audio halves, because both of them need to
//! be able to name a note buffer: a node that reads a controller off the
//! stream is a parameter op, and a plugin that hears the stream is an audio
//! op. Which buffer leaves which socket is pure topology — it depends on what
//! is wired to what and on what each node says it drops — so it can be settled
//! first.
//!
//! What cannot be settled first is the *lane numbers*: a gate's condition and a
//! generator's value are registers the parameter half has yet to allocate. Ops
//! that need one are emitted with a hole and listed in [`Notes::pending`], and
//! [`resolve_lanes`] fills them in once the parameter half has run. That keeps
//! the circularity — gates need lanes, lanes need the parameter pass, the
//! parameter pass needs buffers — down to one deferred field rather than a
//! second traversal.

use crate::compile::stages::Plan;
use crate::graph::{Graph, NodeId};
use crate::ir::{
    ALL_CHANNELS, ALL_CONTROLLERS, MAX_NOTE_BUFS, MAX_NOTE_EMITS, NoteBuf, NoteOp, Span,
};
use crate::nodes::NodeKind;

use super::CompileError;

/// Offset added to an output socket index when looking up a lane the parameter
/// half filed against an *output* rather than an input.
///
/// Mirrors the constant in `cx`, and for the same reason: a lane is keyed by
/// `(node, socket)` and every other user of one means an input socket by it.
const OUTPUT_SOCKET: u8 = 128;

/// Which lane an op is still waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wants {
    /// A gate's open/shut condition, filed against the node's output socket.
    Gate,
    /// A generator's value, filed against the node's input socket 0.
    Value,
}

/// An op whose lane number the parameter half has yet to book.
struct Pending {
    op: usize,
    node: NodeId,
    port: u8,
    wants: Wants,
}

/// The note half of a program, before lane numbers are known.
pub(crate) struct Notes {
    pub ops: Vec<NoteOp>,
    pub bufs: u16,
    /// Which stage each op belongs to, and the spans that fall out of it once
    /// `resolve_lanes` has dropped the ops whose lane never materialised.
    stages: Vec<usize>,
    pub spans: Vec<Span>,
    /// Output socket → the note buffer leaving it.
    ///
    /// A filter that drops nothing binds its input's buffer rather than one of
    /// its own, so several sockets can name the same buffer. Nothing writes a
    /// buffer twice, so the sharing is free.
    outputs: Vec<((NodeId, u8), NoteBuf)>,
    pending: Vec<Pending>,
    states: u16,
}

impl Notes {
    /// The note buffer wired into `node`'s input `port`.
    ///
    /// `None` when nothing is connected, which is the answer that makes an
    /// unwired instrument silent rather than making it play whatever the DAW
    /// happened to send.
    pub(crate) fn source_of(&self, graph: &Graph, node: NodeId, port: u8) -> Option<NoteBuf> {
        let (from, from_port) = graph.source_of(node, port)?;
        self.outputs
            .iter()
            .find(|&&(socket, _)| socket == (from, from_port))
            .map(|&(_, buf)| buf)
    }
}

/// Walk the order and lay out the note half.
pub(crate) fn compile_notes(
    graph: &Graph,
    order: &[NodeId],
    plan: &Plan,
) -> Result<Notes, CompileError> {
    let mut notes = Notes {
        ops: Vec::new(),
        bufs: 0,
        stages: Vec::new(),
        spans: Vec::new(),
        outputs: Vec::new(),
        pending: Vec::new(),
        states: 0,
    };

    // Once per stage rather than once through: a stage's ops have to be
    // contiguous for the engine to slice rather than filter, and walking the
    // order this way gets that without a sort. It stays a topological order
    // because no edge points from a later stage to an earlier one.
    for stage in 0..plan.stages.len() {
        for (index, &id) in order.iter().enumerate() {
            if plan.of[index] != stage {
                continue;
            }
            let node = graph.node(id).expect("ordering only contains real nodes");
            route(graph, order, &mut notes, id, &node.kind)?;
            notes.stages.resize(notes.ops.len(), stage);
        }
    }
    Ok(notes)
}

/// Fill in the lanes the parameter half booked, now that it has run.
///
/// An op whose lane never materialised is dropped rather than left with a
/// hole: for a generator that means the node had no value wired and there is
/// nothing to send, and for a gate it would mean a program the engine should
/// not have been handed.
pub(crate) fn resolve_lanes(notes: &mut Notes, stages: usize, lanes: &[((NodeId, u8), u16)]) {
    let mut drop = Vec::new();
    for pending in &notes.pending {
        let socket = match pending.wants {
            Wants::Gate => (pending.node, OUTPUT_SOCKET + pending.port),
            Wants::Value => (pending.node, 0),
        };
        let found = lanes
            .iter()
            .find(|&&(key, _)| key == socket)
            .map(|&(_, lane)| lane);
        match (found, &mut notes.ops[pending.op]) {
            (Some(lane), NoteOp::Filter { gate, .. }) => *gate = Some(lane),
            (Some(lane), NoteOp::Emit { lane: slot, .. }) => *slot = lane,
            (None, _) => drop.push(pending.op),
            _ => {}
        }
    }
    // Removed back to front so the earlier indices stay valid. A dropped op's
    // buffer stays allocated and simply never gets written, which reads as an
    // empty stream — the same thing an unwired socket produces.
    drop.sort_unstable();
    drop.dedup();
    for op in drop.into_iter().rev() {
        notes.ops.remove(op);
        notes.stages.remove(op);
    }

    // The spans are worked out here rather than while emitting, because the
    // removal above moves everything after a dropped op. A stage nothing
    // landed in gets an empty span where it would have started, so the spans
    // stay one per stage however many of those there are.
    let mut spans = Vec::with_capacity(stages);
    let mut at = 0usize;
    for stage in 0..stages {
        let len = notes.stages[at..]
            .iter()
            .take_while(|&&which| which == stage)
            .count();
        spans.push(Span {
            start: at as u32,
            end: (at + len) as u32,
        });
        at += len;
    }
    notes.spans = spans;
}

/// One node's share of the note half.
///
/// Generic over the node kinds rather than written into each of them: what a
/// note node contributes is entirely described by the questions it already
/// answers — where notes come from, which input an output hands on, what it
/// drops on the way, and whether it makes controllers of its own. Topological
/// order guarantees the buffer feeding a socket is bound before anything reads
/// it.
fn route(
    graph: &Graph,
    order: &[NodeId],
    notes: &mut Notes,
    id: NodeId,
    kind: &NodeKind,
) -> Result<(), CompileError> {
    if let Some(bus) = kind.note_source() {
        let out = alloc_buf(notes)?;
        notes.ops.push(NoteOp::Input { out, bus });
        notes.outputs.push(((id, 0), out));
        return Ok(());
    }

    for port in 0..kind.output_ports().len() as u8 {
        // A socket nobody reads gets no buffer. A key switch offers one output
        // per destination and a patch usually leaves some of them empty;
        // filling those would spend the pool on streams no plugin will ever be
        // handed.
        if readers_of(graph, order, id, port) == 0 {
            continue;
        }

        let a = kind
            .note_passthrough(port)
            .and_then(|input| notes.source_of(graph, id, input));

        if let Some((channel, cc)) = kind.note_emits(port) {
            let out = alloc_buf(notes)?;
            let state = alloc_state(notes)?;
            notes.ops.push(NoteOp::Emit {
                a,
                out,
                // Filled in by `resolve_lanes`.
                lane: 0,
                state,
                channel,
                cc,
            });
            notes.pending.push(Pending {
                op: notes.ops.len() - 1,
                node: id,
                port,
                wants: Wants::Value,
            });
            notes.outputs.push(((id, port), out));
            continue;
        }

        let Some(a) = a else {
            // Nothing wired upstream. Leaving the socket unbound is what makes
            // an instrument behind it silent.
            continue;
        };
        let gated = kind.note_gated(port);
        let mute = kind.note_mute(port);
        let channels = kind.note_channels(port);
        let controllers = kind.note_controllers(port);
        // An open filter that drops nothing is not worth a buffer or a copy;
        // the socket simply carries what came in.
        let passes_everything =
            !gated && mute == 0 && channels == ALL_CHANNELS && controllers == ALL_CONTROLLERS;
        let out = if passes_everything {
            a
        } else {
            let out = alloc_buf(notes)?;
            notes.ops.push(NoteOp::Filter {
                a,
                out,
                gate: None,
                mute,
                channels,
                controllers,
            });
            if gated {
                notes.pending.push(Pending {
                    op: notes.ops.len() - 1,
                    node: id,
                    port,
                    wants: Wants::Gate,
                });
            }
            out
        };
        notes.outputs.push(((id, port), out));
    }
    Ok(())
}

/// How many links leave `(node, port)` for a node the program actually runs.
fn readers_of(graph: &Graph, order: &[NodeId], node: NodeId, port: u8) -> usize {
    graph
        .links
        .iter()
        .filter(|l| l.from == node && l.from_port == port && order.contains(&l.to))
        .count()
}

fn alloc_buf(notes: &mut Notes) -> Result<NoteBuf, CompileError> {
    if usize::from(notes.bufs) >= MAX_NOTE_BUFS {
        return Err(CompileError::TooLarge {
            what: "note buffers",
            limit: MAX_NOTE_BUFS,
        });
    }
    let buf = notes.bufs;
    notes.bufs += 1;
    Ok(buf)
}

fn alloc_state(notes: &mut Notes) -> Result<u16, CompileError> {
    if usize::from(notes.states) >= MAX_NOTE_EMITS {
        return Err(CompileError::TooLarge {
            what: "nodes that generate controllers",
            limit: MAX_NOTE_EMITS,
        });
    }
    let state = notes.states;
    notes.states += 1;
    Ok(state)
}
