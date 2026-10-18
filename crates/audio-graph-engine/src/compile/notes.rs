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
    ALL_CHANNELS, ALL_CONTROLLERS, CustomNote, KernelKind, MAX_MERGE_INPUTS, MAX_NOTE_BUFS,
    MAX_NOTE_EMITS, MAX_TREMOLOS, NoteBuf, NoteKernel, NoteOp, NoteParamBinding, NoteStream,
    NoteStreamKind, Span,
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

struct PendingCustom {
    custom: usize,
    input: usize,
    node: NodeId,
    port: u8,
}

/// The note half of a program, before lane numbers are known.
pub(crate) struct Notes {
    pub ops: Vec<NoteOp>,
    pub custom_notes: Vec<CustomNote>,
    pub bufs: u16,
    pub streams: Vec<NoteStream>,
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
    pending_custom: Vec<PendingCustom>,
    states: u16,
    /// Tremolo index → the MIDI node whose clock and held notes it carries.
    pub tremolo_nodes: Vec<NodeId>,
}

pub(crate) struct NoteCx<'a> {
    graph: &'a Graph,
    notes: &'a mut Notes,
    node: NodeId,
    port: u8,
}

impl NoteCx<'_> {
    pub(crate) fn input(&self, port: u8) -> Option<NoteBuf> {
        self.notes.source_of(self.graph, self.node, port)
    }

    pub(crate) fn emit_custom<K: NoteKernel>(
        &mut self,
        input: NoteBuf,
        parameters: &[(u8, f64)],
        kernel: K,
    ) -> Result<(), CompileError> {
        // State follows the node, so two processors could not be distinguished
        // when their order changes during recompilation.
        assert!(
            self.notes
                .custom_notes
                .iter()
                .all(|custom| custom.node != self.node),
            "a node registers one custom note processor"
        );
        let out = alloc_buf(
            self.notes,
            NoteStream {
                node: self.node,
                port: self.port,
                source: Some(input),
                kind: NoteStreamKind::Custom(KernelKind::of::<K>()),
            },
        )?;
        let index = self.notes.custom_notes.len();
        let bindings = parameters
            .iter()
            .enumerate()
            .map(|(at, &(port, fallback))| {
                self.notes.pending_custom.push(PendingCustom {
                    custom: index,
                    input: at,
                    node: self.node,
                    port,
                });
                NoteParamBinding {
                    lane: None,
                    fallback,
                }
            })
            .collect();
        self.notes
            .custom_notes
            .push(CustomNote::new(self.node, bindings, kernel));
        self.notes.ops.push(NoteOp::Custom {
            a: input,
            out,
            index: index as u16,
        });
        self.notes.outputs.push(((self.node, self.port), out));
        Ok(())
    }
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
        custom_notes: Vec::new(),
        bufs: 0,
        streams: Vec::new(),
        stages: Vec::new(),
        spans: Vec::new(),
        outputs: Vec::new(),
        pending: Vec::new(),
        pending_custom: Vec::new(),
        states: 0,
        tremolo_nodes: Vec::new(),
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
    for pending in &notes.pending_custom {
        notes.custom_notes[pending.custom].parameters[pending.input].lane = lanes
            .iter()
            .find(|&&(socket, _)| socket == (pending.node, pending.port))
            .map(|&(_, lane)| lane);
    }
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
        let out = match notes.ops.remove(op) {
            NoteOp::Input { out, .. }
            | NoteOp::Filter { out, .. }
            | NoteOp::Emit { out, .. }
            | NoteOp::Merge { out, .. }
            | NoteOp::Custom { out, .. }
            | NoteOp::Tremolo { out, .. } => out,
        };
        notes.streams[usize::from(out)].kind = NoteStreamKind::Empty;
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
/// Core routing shares filters, merges and generators; custom processors bind
/// their own output through `NoteCx`. Topological order guarantees the buffer
/// feeding a socket is bound before anything reads it.
fn route(
    graph: &Graph,
    order: &[NodeId],
    notes: &mut Notes,
    id: NodeId,
    kind: &NodeKind,
) -> Result<(), CompileError> {
    if let Some(bus) = kind.note_source() {
        let out = alloc_buf(
            notes,
            NoteStream {
                node: id,
                port: 0,
                source: None,
                kind: NoteStreamKind::Input(bus),
            },
        )?;
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

        let mut cx = NoteCx {
            graph,
            notes,
            node: id,
            port,
        };
        if kind.compile_note(port, &mut cx)? {
            continue;
        }
        if let Some(tremolo) = kind.note_tremolo(port) {
            let Some(a) = notes.source_of(graph, id, tremolo.input) else {
                continue;
            };
            if notes.tremolo_nodes.len() >= MAX_TREMOLOS {
                return Err(CompileError::TooLarge {
                    what: "tremolos",
                    limit: MAX_TREMOLOS,
                });
            }
            let state = notes.tremolo_nodes.len() as u16;
            notes.tremolo_nodes.push(id);
            let out = alloc_buf(
                notes,
                NoteStream {
                    node: id,
                    port,
                    source: Some(a),
                    kind: NoteStreamKind::Tremolo,
                },
            )?;
            notes.ops.push(NoteOp::Tremolo {
                a,
                out,
                state,
                spec: tremolo.spec,
                mute: tremolo.mute,
            });
            notes.outputs.push(((id, port), out));
            continue;
        }

        let merged = kind.note_merge(port);
        if !merged.is_empty() {
            let mut wired: Vec<NoteBuf> = merged
                .iter()
                .filter_map(|&input| notes.source_of(graph, id, input))
                .collect();
            // One stream wired twice is still one stream.
            wired.dedup();
            match wired.len() {
                0 => {}
                // Nothing to join: the socket carries the one stream, the way
                // an open filter does.
                1 => notes.outputs.push(((id, port), wired[0])),
                _ => {
                    let count = wired.len().min(MAX_MERGE_INPUTS);
                    let mut inputs = [0; MAX_MERGE_INPUTS];
                    inputs[..count].copy_from_slice(&wired[..count]);
                    let out = alloc_buf(
                        notes,
                        NoteStream {
                            node: id,
                            port,
                            source: None,
                            kind: NoteStreamKind::Merge { count: count as u8 },
                        },
                    )?;
                    notes.ops.push(NoteOp::Merge {
                        inputs,
                        count: count as u8,
                        out,
                    });
                    notes.outputs.push(((id, port), out));
                }
            }
            continue;
        }

        let a = kind
            .note_passthrough(port)
            .and_then(|input| notes.source_of(graph, id, input));

        if let Some((channel, cc)) = kind.note_emits(port) {
            let out = alloc_buf(
                notes,
                NoteStream {
                    node: id,
                    port,
                    source: a,
                    kind: NoteStreamKind::Emit { channel, cc },
                },
            )?;
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
            let out = alloc_buf(
                notes,
                NoteStream {
                    node: id,
                    port,
                    source: Some(a),
                    kind: NoteStreamKind::Filter {
                        gated,
                        mute,
                        channels,
                        controllers,
                    },
                },
            )?;
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

fn alloc_buf(notes: &mut Notes, stream: NoteStream) -> Result<NoteBuf, CompileError> {
    if usize::from(notes.bufs) >= MAX_NOTE_BUFS {
        return Err(CompileError::TooLarge {
            what: "note buffers",
            limit: MAX_NOTE_BUFS,
        });
    }
    let buf = notes.bufs;
    notes.bufs += 1;
    notes.streams.push(stream);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Chunking, Stateful};
    use crate::nodes::{Beats, Constant, MidiDelay, Plugin, PluginPorts};

    fn delay_graph(wired: bool, read: bool) -> (Graph, NodeId, NodeId) {
        let mut graph = Graph::new();
        let source = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let delay = graph.add(
            NodeKind::MidiDelay(MidiDelay {
                seconds: 0.25,
                beats: Beats::new(1, 2),
                sync: false,
            }),
            [0.0, 0.0],
        );
        let sink = graph.add(
            NodeKind::Plugin(Plugin {
                instance: 0,
                ports: PluginPorts {
                    accepts_notes: true,
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        );
        graph.node_mut(delay).unwrap().always_on = true;
        if wired {
            graph.connect(source, 0, delay, 0);
        }
        if read {
            graph.connect(delay, 0, sink, 0);
        }
        (graph, source, delay)
    }

    /// An unwired or unread stream never prepares a stateful processor.
    #[test]
    fn an_unused_custom_note_output_has_no_runtime() {
        for (wired, read) in [(false, true), (true, false)] {
            let (graph, _, _) = delay_graph(wired, read);
            let program = crate::compile(&graph, 0).unwrap();
            assert!(program.custom_notes.is_empty());
            assert!(
                !program
                    .note_ops
                    .iter()
                    .any(|op| matches!(op, NoteOp::Custom { .. }))
            );
        }
    }

    /// Custom inputs resolve the parameter pass's lanes and retain unwired defaults.
    #[test]
    fn custom_note_parameters_bind_after_the_parameter_pass() {
        let (mut graph, _, delay) = delay_graph(true, true);
        let fallback = crate::compile(&graph, 0).unwrap();
        assert_eq!(fallback.custom_notes[0].parameters[0].lane, None);
        assert_eq!(fallback.custom_notes[0].parameters[0].fallback, 0.25);

        let time = graph.add(NodeKind::Constant(Constant { value: 0.125 }), [0.0, 0.0]);
        graph.connect(time, 0, delay, 1);
        let program = crate::compile(&graph, 0).unwrap();
        let lane = program.custom_notes[0].parameters[0].lane.unwrap();
        let register = program
            .output_registers
            .iter()
            .find(|(socket, _)| *socket == (time, 0))
            .unwrap()
            .1;
        assert!(program.outputs.contains(&(lane, register)));
        assert!(program.stages.iter().any(|stage| {
            program.note_ops[stage.notes.range()]
                .iter()
                .any(|op| matches!(op, NoteOp::Custom { .. }))
        }));
    }

    /// A node-local kernel type needs no entry in a central execution enum.
    #[test]
    fn a_local_kernel_can_register_a_note_stream() {
        #[derive(Debug)]
        struct LocalKernel;

        impl NoteKernel for LocalKernel {
            type State = ();
            fn create_state(&self) {}
            fn eval(&self, _: &mut (), _: &mut crate::engine::NoteEvalContext<'_>) {}
        }

        let (graph, source, node) = delay_graph(true, true);
        let plan = Plan {
            stages: vec![Chunking::WholeBlock],
            of: vec![0],
        };
        let mut notes = compile_notes(&graph, &[source], &plan).unwrap();
        let mut cx = NoteCx {
            graph: &graph,
            notes: &mut notes,
            node,
            port: 0,
        };
        let input = cx.input(0).unwrap();
        cx.emit_custom(input, &[], LocalKernel).unwrap();
        assert_eq!(notes.custom_notes.len(), 1);
        assert_eq!(
            notes.custom_notes[0].processor.kind(),
            KernelKind::of::<Stateful<LocalKernel, ()>>()
        );
        assert_eq!(notes.streams[1].source, Some(input));
        assert_eq!(
            notes.streams[1].kind,
            NoteStreamKind::Custom(KernelKind::of::<LocalKernel>())
        );
    }
}
