//! Edit graph → `Program`. Runs on the UI thread, as often as the user edits.
//!
//! Three jobs, in order: find the nodes that actually matter, put them in an
//! order where every node comes after the ones it reads, and hand out
//! registers. Everything the audio thread would otherwise have to work out —
//! which input is connected, whether a value needs clamping, what a tempo-sync
//! rate means in cycles per beat — is settled here.
//!
//! Failures are values, not panics. A cycle or a missing input is an ordinary
//! state for a graph someone is halfway through drawing; the editor shows the
//! message and keeps running the last program that compiled.

mod audio;
mod cx;

pub(crate) use cx::{AudioCx, DeclareCx, ParamCx};

use crate::graph::{Graph, LineId, NodeId};
use crate::ir::Program;
use crate::port::PortType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    /// A link chain that comes back to where it started.
    Cycle { node: NodeId },
    /// More registers or LFOs than the audio thread has room for.
    TooLarge { what: &'static str, limit: usize },
    /// A slot index outside the wrapper's table.
    BadSlot { node: NodeId, slot: usize },
    /// A link whose ends carry different things (§14.3). `connect` and `prune`
    /// both refuse to make one, so reaching here means a hand-edited or
    /// future-versioned patch.
    TypeMismatch { node: NodeId, port: u8 },
    /// Two writers on one delay line. Which one wins would otherwise depend
    /// on node creation order.
    DuplicateDelayWrite { line: LineId },
    /// A delay line whose two halves disagree about what they carry.
    DelayTypeMismatch { line: LineId },
    /// A node kind the compiler does not emit code for yet. M8.1 settles the
    /// graph's shape. Audio and note routing landed in M8.2 and M8.3; what is
    /// left behind this is note delay lines (§14.10) and a plugin's own note
    /// output.
    NotYet { what: &'static str },
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::Cycle { node } => {
                write!(f, "the graph loops back on itself at node {node}")
            }
            CompileError::TooLarge { what, limit } => {
                write!(f, "too many {what} in one graph (limit {limit})")
            }
            CompileError::BadSlot { node, slot } => {
                write!(
                    f,
                    "node {node} names slot {}, which does not exist",
                    slot + 1
                )
            }
            CompileError::TypeMismatch { node, port } => {
                write!(
                    f,
                    "input {} of node {node} is fed something of another type",
                    port + 1
                )
            }
            CompileError::DuplicateDelayWrite { line } => {
                write!(f, "two nodes both write delay line {}", line + 1)
            }
            CompileError::DelayTypeMismatch { line } => {
                write!(
                    f,
                    "the two halves of delay line {} carry different things",
                    line + 1
                )
            }
            CompileError::NotYet { what } => {
                write!(f, "{what} is not implemented yet")
            }
        }
    }
}

impl std::error::Error for CompileError {}

/// Compile `graph` for a wrapper with `slot_count` slots.
pub fn compile(graph: &Graph, slot_count: usize) -> Result<Program, CompileError> {
    // Only what feeds an output matters. A node the user has dropped on the
    // canvas but not wired up yet must cost the audio thread nothing.
    check_links(graph)?;

    let mut order: Vec<NodeId> = Vec::new();
    let mut mark = vec![Mark::New; graph.nodes.len()];
    let index: Vec<NodeId> = graph.nodes.iter().map(|n| n.id).collect();

    // Every sink is a root. `DelayWrite` is one even though nothing reads it —
    // that is exactly what makes it a graph cut rather than an edge (§14.4).
    // So is anything the user has marked always-on (§14.14).
    let sinks: Vec<NodeId> = graph
        .nodes
        .iter()
        .filter(|n| n.kind.output_ports().is_empty() || n.always_on)
        .map(|n| n.id)
        .collect();

    // Delay lines, numbered in the order their writers appear. Done before the
    // walk so that a `DelayRead` compiled early already knows its line index.
    let lines = collect_lines(graph)?;

    for &root in &sinks {
        visit(graph, &index, root, &mut mark, &mut order)?;
    }

    // The registers, the op list and the lane books all live in the context
    // now; each node reaches for what it needs through it (see `cx.rs`).
    let mut cx = ParamCx::new(graph, &lines, slot_count);
    for &id in &order {
        let node = graph.node(id).expect("ordering only contains real nodes");
        cx.begin(id);
        node.kind.compile(&mut cx)?;
    }
    let param = cx.finish();

    let audio = audio::compile_audio(graph, &order, &lines, &param.audio_lanes)?;

    Ok(Program {
        ops: param.ops,
        registers: param.registers,
        outputs: param.outputs,
        audio_ops: audio.ops,
        param_targets: param.param_targets,
        audio_lane_base: (slot_count + crate::ir::MAX_GRAPH_PARAMS) as u16,
        instances: audio.instances,
        buffers: audio.buffers,
        chunking: audio.chunking,
        latency: audio.latency,
        delay_nodes: lines.iter().map(|l| l.writer).collect(),
        audio_delay_nodes: audio.delay_nodes,
        audio_ring_seconds: audio.ring_seconds,
        // Filled in on the main thread, which is the only side that knows the
        // sample rate and the only side allowed to allocate (§9.1).
        audio_ring_len: Vec::new(),
        audio_rings: Vec::new(),
        lfo_nodes: param.lfo_nodes,
    })
}

/// One delay line, as the compiler sees it.
pub(crate) struct Line {
    pub id: LineId,
    /// The `DelayWrite` node, which is what the line's ring state is keyed to
    /// across a program swap (§14.5).
    pub writer: NodeId,
    /// What the line carries. An audio line with a writer closes an audio loop,
    /// which is what forces the fine evaluation grain (§14.9).
    pub ty: PortType,
}

/// Ask every node what delay lines it is an end of, and number them.
fn collect_lines(graph: &Graph) -> Result<Vec<Line>, CompileError> {
    let mut cx = DeclareCx::new();
    for node in &graph.nodes {
        cx.begin(node.id);
        node.kind.declare(&mut cx)?;
    }
    Ok(cx.finish())
}

/// Stands in for "this line has no writer yet". Node ids are never reused, and
/// `Graph::next_id` cannot reach `u32::MAX` without exhausting the counter
/// first, so no real node can collide with it.
pub(crate) const NO_WRITER: NodeId = NodeId::MAX;

/// Every link's two ends must carry the same thing (§14.3).
///
/// `Graph::connect` and `Graph::prune` both refuse to make a mismatched link,
/// so this only ever fires on a patch that was edited by hand or written by a
/// later version. Checking anyway is cheap, and the alternative is the
/// compiler reading a register that holds the wrong kind of value.
fn check_links(graph: &Graph) -> Result<(), CompileError> {
    for link in &graph.links {
        if !graph.can_connect(link.from, link.from_port, link.to, link.to_port) {
            return Err(CompileError::TypeMismatch {
                node: link.to,
                port: link.to_port,
            });
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    New,
    /// On the current path — seeing this again is a cycle.
    Open,
    Done,
}

/// Depth-first post-order, which is exactly a topological sort when it
/// finishes: a node is appended only once everything it reads is already in.
fn visit(
    graph: &Graph,
    index: &[NodeId],
    id: NodeId,
    mark: &mut [Mark],
    order: &mut Vec<NodeId>,
) -> Result<(), CompileError> {
    let Some(pos) = index.iter().position(|&n| n == id) else {
        // A link to a node that no longer exists. `Graph::prune` normally
        // clears these; ignoring one is better than refusing to compile.
        return Ok(());
    };
    match mark[pos] {
        Mark::Done => return Ok(()),
        Mark::Open => return Err(CompileError::Cycle { node: id }),
        Mark::New => {}
    }
    mark[pos] = Mark::Open;

    let node = &graph.nodes[pos];
    for port in 0..node.kind.input_ports().len() as u8 {
        if let Some((from, _)) = graph.source_of(id, port) {
            visit(graph, index, from, mark, order)?;
        }
    }

    mark[pos] = Mark::Done;
    order.push(id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{MAX_REGISTERS, MathOp, Op, Operand, Reg, Waveform};
    use crate::nodes::{
        AudioIn, Constant, DelayRead, DelayWrite, Lfo, Math, NodeKind, ParamPort, Plugin,
        PluginPorts, Rate, SlotIn,
    };

    const SLOTS: usize = 32;

    /// Somewhere for a parameter chain to go.
    ///
    /// A graph whose values reach nothing is pruned, so every test that wants
    /// to see an op emitted needs a sink. `SlotOut` used to be it; §14.12's
    /// parameter socket is, now — and unlike a slot it does not fight the DAW
    /// for the lane.
    fn param_sink(graph: &mut Graph) -> NodeId {
        graph.add(
            NodeKind::Plugin(Plugin {
                instance: 0,
                ports: PluginPorts {
                    params: vec![ParamPort {
                        id: 0,
                        name: "p".into(),
                    }],
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        )
    }

    #[test]
    fn an_empty_graph_compiles_to_an_empty_program() {
        let program = compile(&Graph::new(), SLOTS).unwrap();
        assert!(program.is_empty());
        assert_eq!(program.registers, 0);
    }

    #[test]
    fn only_what_feeds_an_output_is_compiled() {
        let mut graph = Graph::new();
        let used = graph.add(NodeKind::Constant(Constant { value: 0.5 }), [0.0, 0.0]);
        let out = param_sink(&mut graph);
        graph.connect(used, 0, out, 0);
        // Dropped on the canvas and wired to nothing.
        graph.add(NodeKind::Constant(Constant { value: 0.25 }), [0.0, 0.0]);
        graph.add(
            NodeKind::Lfo(Lfo {
                waveform: Waveform::Sine,
                rate: Rate::Hz(1.0),
                phase: 0.0,
                depth: 0.5,
                offset: 0.5,
            }),
            [0.0, 0.0],
        );

        let program = compile(&graph, SLOTS).unwrap();
        assert_eq!(
            program.ops.len(),
            1,
            "the unwired nodes cost the audio thread nothing"
        );
        assert!(program.lfo_nodes.is_empty());
    }

    /// §14.14. An analyser produces nothing anyone reads, so the rule above
    /// would delete it — the toggle is how the user says otherwise.
    #[test]
    fn an_always_on_node_is_compiled_with_nothing_downstream() {
        let mut graph = Graph::new();
        let lfo = graph.add(
            NodeKind::Lfo(Lfo {
                waveform: Waveform::Sine,
                rate: Rate::Hz(1.0),
                phase: 0.0,
                depth: 0.5,
                offset: 0.5,
            }),
            [0.0, 0.0],
        );
        assert!(
            compile(&graph, SLOTS).unwrap().is_empty(),
            "unwired, it costs nothing"
        );

        graph.node_mut(lfo).unwrap().always_on = true;
        let program = compile(&graph, SLOTS).unwrap();
        assert_eq!(program.ops.len(), 1);
        assert_eq!(program.lfo_nodes.len(), 1, "and it keeps its phase");
    }

    /// Whatever it reads has to be compiled too, or it runs on nothing.
    #[test]
    fn an_always_on_node_pulls_its_inputs_in_with_it() {
        let mut graph = Graph::new();
        let source = graph.add(NodeKind::Constant(Constant { value: 0.5 }), [0.0, 0.0]);
        let sink = graph.add(
            NodeKind::Math(Math {
                op: MathOp::Multiply,
                b: 2.0,
            }),
            [0.0, 0.0],
        );
        graph.connect(source, 0, sink, 0);
        graph.node_mut(sink).unwrap().always_on = true;

        let program = compile(&graph, SLOTS).unwrap();
        assert_eq!(program.ops.len(), 2, "the constant came with it");
    }

    #[test]
    fn every_op_reads_only_registers_already_written() {
        let mut graph = Graph::new();
        let a = graph.add(NodeKind::Constant(Constant { value: 0.5 }), [0.0, 0.0]);
        let b = graph.add(NodeKind::SlotIn(SlotIn { slot: 2 }), [0.0, 0.0]);
        let sum = graph.add(
            NodeKind::Math(Math {
                op: MathOp::Add,
                b: 0.0,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        // Wired back to front on purpose: creation order must not matter.
        graph.connect(sum, 0, out, 0);
        graph.connect(b, 0, sum, 1);
        graph.connect(a, 0, sum, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let mut written = vec![false; program.registers];
        for op in &program.ops {
            for read in reads(op) {
                assert!(
                    written[read as usize],
                    "op reads register {read} before it is written"
                );
            }
            if let Some(out) = writes(op) {
                written[out as usize] = true;
            }
        }
    }

    #[test]
    fn a_cycle_is_reported_rather_than_hung_on() {
        let mut graph = Graph::new();
        let x = graph.add(
            NodeKind::Math(Math {
                op: MathOp::Add,
                b: 1.0,
            }),
            [0.0, 0.0],
        );
        let y = graph.add(
            NodeKind::Math(Math {
                op: MathOp::Add,
                b: 1.0,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(x, 0, y, 0);
        graph.connect(y, 0, x, 0);
        graph.connect(y, 0, out, 0);

        assert!(matches!(
            compile(&graph, SLOTS),
            Err(CompileError::Cycle { .. })
        ));
    }

    #[test]
    fn a_slot_outside_the_table_is_refused() {
        let mut graph = Graph::new();
        let read = graph.add(NodeKind::SlotIn(SlotIn { slot: 99 }), [0.0, 0.0]);
        let out = param_sink(&mut graph);
        graph.connect(read, 0, out, 0);
        assert!(matches!(
            compile(&graph, SLOTS),
            Err(CompileError::BadSlot { .. })
        ));
    }

    #[test]
    fn a_graph_bigger_than_the_audio_thread_can_hold_is_refused() {
        let mut graph = Graph::new();
        let out = param_sink(&mut graph);
        let mut last = graph.add(NodeKind::Constant(Constant { value: 0.0 }), [0.0, 0.0]);
        for _ in 0..MAX_REGISTERS + 8 {
            let next = graph.add(
                NodeKind::Math(Math {
                    op: MathOp::Add,
                    b: 1.0,
                }),
                [0.0, 0.0],
            );
            graph.connect(last, 0, next, 0);
            last = next;
        }
        graph.connect(last, 0, out, 0);
        assert!(matches!(
            compile(&graph, SLOTS),
            Err(CompileError::TooLarge { .. })
        ));
    }

    /// §14.4: the point of splitting a delay into two halves is that the
    /// existing cycle check needs no exception for it. This is the same graph
    /// as `a_cycle_is_reported_rather_than_hung_on`, with the loop closed
    /// through a delay line instead of directly.
    #[test]
    fn a_loop_closed_through_a_delay_is_not_a_cycle() {
        let mut graph = Graph::new();
        let read = graph.add(
            NodeKind::DelayRead(DelayRead {
                line: 0,
                ty: PortType::Param,
                max_time: 1.0,
                time: 0.25,
            }),
            [0.0, 0.0],
        );
        let scale = graph.add(
            NodeKind::Math(Math {
                op: MathOp::Multiply,
                b: 0.5,
            }),
            [0.0, 0.0],
        );
        let write = graph.add(
            NodeKind::DelayWrite(DelayWrite {
                line: 0,
                ty: PortType::Param,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);

        graph.connect(read, 0, scale, 0);
        graph.connect(scale, 0, write, 0);
        graph.connect(scale, 0, out, 0);

        let program = compile(&graph, SLOTS).expect("a delay is a graph cut, not an edge");
        assert_eq!(program.delay_nodes, vec![write]);
        assert!(
            program
                .ops
                .iter()
                .any(|op| matches!(op, Op::DelayRead { .. }))
        );
        assert!(
            program
                .ops
                .iter()
                .any(|op| matches!(op, Op::DelayWrite { .. }))
        );
    }

    /// Where a write lands in the topological order is not determined when
    /// nothing downstream reads the line. Every write goes last so that a read
    /// always means "as of the end of the previous sub-block".
    #[test]
    fn delay_writes_are_emitted_after_everything_else() {
        let mut graph = Graph::new();
        let read = graph.add(
            NodeKind::DelayRead(DelayRead {
                line: 7,
                ty: PortType::Param,
                max_time: 1.0,
                time: 0.1,
            }),
            [0.0, 0.0],
        );
        let source = graph.add(NodeKind::Constant(Constant { value: 0.25 }), [0.0, 0.0]);
        let write = graph.add(
            NodeKind::DelayWrite(DelayWrite {
                line: 7,
                ty: PortType::Param,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(source, 0, write, 0);
        graph.connect(read, 0, out, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let last = program.ops.last().expect("something was emitted");
        assert!(matches!(last, Op::DelayWrite { .. }));
    }

    #[test]
    fn two_writers_on_one_line_is_an_error_not_a_coin_toss() {
        let mut graph = Graph::new();
        for _ in 0..2 {
            graph.add(
                NodeKind::DelayWrite(DelayWrite {
                    line: 3,
                    ty: PortType::Param,
                }),
                [0.0, 0.0],
            );
        }
        assert!(matches!(
            compile(&graph, SLOTS),
            Err(CompileError::DuplicateDelayWrite { line: 3 })
        ));
    }

    #[test]
    fn the_two_halves_of_a_line_must_agree_on_what_it_carries() {
        let mut graph = Graph::new();
        graph.add(
            NodeKind::DelayWrite(DelayWrite {
                line: 1,
                ty: PortType::Param,
            }),
            [0.0, 0.0],
        );
        graph.add(
            NodeKind::DelayRead(DelayRead {
                line: 1,
                ty: PortType::STEREO,
                max_time: 1.0,
                time: 0.1,
            }),
            [0.0, 0.0],
        );
        assert!(matches!(
            compile(&graph, SLOTS),
            Err(CompileError::DelayTypeMismatch { line: 1 })
        ));
    }

    /// `connect` refuses to make one, so this can only arrive from a file. It
    /// still has to be caught: the compiler would otherwise read a register
    /// holding the wrong kind of value.
    #[test]
    fn a_hand_written_link_between_two_types_is_refused() {
        let mut graph = Graph::new();
        let audio = graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.links.push(crate::graph::Link {
            from: audio,
            from_port: 0,
            to: out,
            to_port: 0,
        });
        assert!(matches!(
            compile(&graph, SLOTS),
            Err(CompileError::TypeMismatch { .. })
        ));
    }

    fn reads(op: &Op) -> Vec<Reg> {
        match *op {
            Op::Const { .. } | Op::Slot { .. } | Op::Lfo { .. } | Op::Expr { .. } => Vec::new(),
            Op::Math { a, b, .. } => match b {
                Operand::Reg(b) => vec![a, b],
                Operand::Value(_) => vec![a],
            },
            Op::Select {
                control, low, high, ..
            } => {
                let mut regs = vec![control];
                for operand in [low, high] {
                    if let Operand::Reg(reg) = operand {
                        regs.push(reg);
                    }
                }
                regs
            }
            Op::Range { a, .. } => vec![a],
            Op::DelayRead { .. } => Vec::new(),
            Op::DelayWrite { a, .. } => vec![a],
        }
    }

    /// The register an op writes, if it writes one. `DelayWrite` does not:
    /// that is what keeps it off the topological sort (§14.4).
    fn writes(op: &Op) -> Option<Reg> {
        match *op {
            Op::Const { out, .. }
            | Op::Slot { out, .. }
            | Op::Lfo { out, .. }
            | Op::Expr { out, .. }
            | Op::Select { out, .. }
            | Op::Math { out, .. }
            | Op::Range { out, .. }
            | Op::DelayRead { out, .. } => Some(out),
            Op::DelayWrite { .. } => None,
        }
    }
}
