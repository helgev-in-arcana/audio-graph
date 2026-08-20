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

use crate::graph::{Graph, LineId, NodeId, NodeKind, PortType, Rate};
use crate::program::{
    MAX_DELAY_LINES, MAX_LFOS, MAX_REGISTERS, Op, Operand, Program, RateSpec, Reg,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    /// A link chain that comes back to where it started.
    Cycle { node: NodeId },
    /// More registers or LFOs than the audio thread has room for.
    TooLarge { what: &'static str, limit: usize },
    /// A slot index outside the wrapper's table.
    BadSlot { node: NodeId, slot: usize },
    /// Two outputs fighting over the same slot. Silently letting one win would
    /// make the graph's behaviour depend on node creation order.
    DuplicateOutput { slot: usize },
    /// A link whose ends carry different things (§14.3). `connect` and `prune`
    /// both refuse to make one, so reaching here means a hand-edited or
    /// future-versioned patch.
    TypeMismatch { node: NodeId, port: u8 },
    /// Two writers on one delay line. Same reasoning as `DuplicateOutput`.
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
            CompileError::DuplicateOutput { slot } => {
                write!(f, "two nodes both drive slot {}", slot + 1)
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
    let sinks: Vec<NodeId> = graph
        .nodes
        .iter()
        .filter(|n| n.kind.output_ports().is_empty())
        .map(|n| n.id)
        .collect();

    // Delay lines, numbered in the order their writers appear. Done before the
    // walk so that a `DelayRead` compiled early already knows its line index.
    let lines = collect_lines(graph)?;

    for &root in &sinks {
        visit(graph, &index, root, &mut mark, &mut order)?;
    }

    // Registers, in the order the ops will write them. Keyed by output socket,
    // not by node: a plugin node has one per bus.
    let mut reg_of: Vec<((NodeId, u8), Reg)> = Vec::new();
    let mut ops: Vec<Op> = Vec::new();
    let mut lfo_nodes: Vec<NodeId> = Vec::new();
    // Held back and appended last. Where a `DelayWrite` lands in the
    // topological order is not determined when nothing downstream reads the
    // line, and "sometimes a sub-block earlier" is not a semantics anyone can
    // reason about. Putting every write at the end makes one rule: a read sees
    // the line as it stood at the end of the previous sub-block.
    let mut writes: Vec<Op> = Vec::new();
    let mut outputs: Vec<(u16, Reg)> = Vec::new();

    let mut next_reg: usize = 0;
    let mut alloc = || -> Result<Reg, CompileError> {
        if next_reg >= MAX_REGISTERS {
            return Err(CompileError::TooLarge {
                what: "nodes",
                limit: MAX_REGISTERS,
            });
        }
        let reg = next_reg as Reg;
        next_reg += 1;
        Ok(reg)
    };

    let lookup = |reg_of: &Vec<((NodeId, u8), Reg)>, key: (NodeId, u8)| -> Option<Reg> {
        reg_of.iter().find(|&&(k, _)| k == key).map(|&(_, r)| r)
    };

    for &id in &order {
        let node = graph.node(id).expect("ordering only contains real nodes");
        // Only ever asked for `Param` inputs below; the type check that makes
        // that safe is in `check_links`.
        let input = |port: u8| -> Option<Reg> {
            graph
                .source_of(id, port)
                .and_then(|from| lookup(&reg_of, from))
        };

        match node.kind {
            NodeKind::Constant { value } => {
                let out = alloc()?;
                ops.push(Op::Const { out, value });
                reg_of.push(((id, 0), out));
            }
            NodeKind::SlotIn { slot } => {
                check_slot(id, slot, slot_count)?;
                let out = alloc()?;
                ops.push(Op::Slot {
                    out,
                    slot: slot as u16,
                });
                reg_of.push(((id, 0), out));
            }
            NodeKind::Lfo {
                waveform,
                rate,
                phase,
                depth,
                offset,
            } => {
                if lfo_nodes.len() >= MAX_LFOS {
                    return Err(CompileError::TooLarge {
                        what: "LFOs",
                        limit: MAX_LFOS,
                    });
                }
                let state = lfo_nodes.len() as u16;
                lfo_nodes.push(id);
                let out = alloc()?;
                ops.push(Op::Lfo {
                    out,
                    state,
                    waveform,
                    rate: match rate {
                        Rate::Hz(hz) => RateSpec::Hz(hz.max(0.0)),
                        // Zero beats per cycle would be an infinitely fast LFO;
                        // treat it as "does not move" rather than as NaN.
                        Rate::Beats(beats) if beats > 0.0 => RateSpec::CyclesPerBeat(1.0 / beats),
                        Rate::Beats(_) => RateSpec::CyclesPerBeat(0.0),
                    },
                    offset_phase: phase.rem_euclid(1.0),
                    depth,
                    centre: offset,
                });
                reg_of.push(((id, 0), out));
            }
            NodeKind::Expression { source } => {
                let out = alloc()?;
                ops.push(Op::Expr { out, source });
                reg_of.push(((id, 0), out));
            }
            NodeKind::Math { op, b } => {
                // An unconnected `a` contributes nothing. The identity element
                // for each operator would be a slightly nicer answer, but
                // "nothing plugged in reads as zero" is one rule instead of six.
                let a = match input(0) {
                    Some(reg) => reg,
                    None => zero(&mut ops, &mut alloc)?,
                };
                let b = match input(1) {
                    Some(reg) => Operand::Reg(reg),
                    None => Operand::Value(b),
                };
                let out = alloc()?;
                ops.push(Op::Math { out, a, b, op });
                reg_of.push(((id, 0), out));
            }
            NodeKind::RangeMap {
                in_lo,
                in_hi,
                out_lo,
                out_hi,
                clamp,
            } => {
                let a = match input(0) {
                    Some(reg) => reg,
                    None => zero(&mut ops, &mut alloc)?,
                };
                let out = alloc()?;
                ops.push(Op::Range {
                    out,
                    a,
                    in_lo,
                    in_span: in_hi - in_lo,
                    out_lo,
                    out_span: out_hi - out_lo,
                    clamp,
                });
                reg_of.push(((id, 0), out));
            }
            NodeKind::SlotOut { slot } => {
                check_slot(id, slot, slot_count)?;
                if outputs.iter().any(|&(s, _)| s as usize == slot) {
                    return Err(CompileError::DuplicateOutput { slot });
                }
                // An output with nothing plugged in is not an error — it is a
                // node the user has placed and not yet wired. It just does not
                // take the slot over from the DAW.
                if let Some(reg) = input(0) {
                    outputs.push((slot as u16, reg));
                }
            }
            NodeKind::DelayRead { line, time, .. } => {
                let index = lines
                    .iter()
                    .position(|l| l.id == line)
                    .expect("collect_lines saw every DelayRead");
                let out = alloc()?;
                ops.push(Op::DelayRead {
                    out,
                    line: index as u16,
                    // A negative time would read the future.
                    time: time.max(0.0),
                });
                reg_of.push(((id, 0), out));
            }
            NodeKind::DelayWrite { line, .. } => {
                let index = lines
                    .iter()
                    .position(|l| l.id == line)
                    .expect("collect_lines saw every DelayWrite");
                // Nothing plugged in writes silence, the same way an unwired
                // SlotOut simply does not take its slot over.
                if let Some(reg) = input(0) {
                    writes.push(Op::DelayWrite {
                        line: index as u16,
                        a: reg,
                    });
                }
            }
            // Audio nodes carry no param register. The audio pass walks the
            // same order again and emits their half (§14.9).
            NodeKind::AudioIn { .. }
            | NodeKind::AudioOut { .. }
            | NodeKind::NoteIn
            | NodeKind::Plugin { .. }
            | NodeKind::Mix { .. } => {}
        }
    }

    ops.append(&mut writes);

    let audio = crate::audio::compile_audio(graph, &order, &lines)?;

    outputs.sort_unstable();
    Ok(Program {
        ops,
        registers: next_reg,
        outputs,
        audio_ops: audio.ops,
        instances: audio.instances,
        buffers: audio.buffers,
        chunking: audio.chunking,
        latency: audio.latency,
        delay_nodes: lines.iter().map(|l| l.writer).collect(),
        lfo_nodes,
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

/// Number the delay lines and check that each one's halves agree.
///
/// A line with reads but no write is not an error: it reads silence, which is
/// what a half-drawn patch should do. A line with a write and no read is not an
/// error either — it is just unread.
fn collect_lines(graph: &Graph) -> Result<Vec<Line>, CompileError> {
    let mut lines: Vec<(Line, PortType)> = Vec::new();

    for node in &graph.nodes {
        let (id, ty, writer) = match node.kind {
            NodeKind::DelayWrite { line, ty } => (line, ty, Some(node.id)),
            NodeKind::DelayRead { line, ty, .. } => (line, ty, None),
            _ => continue,
        };
        // A note delay line would have to store events, not values, and the
        // param ring stores one `f64` per sub-block. Refusing is the honest
        // answer; compiling it would drop every note in silence.
        if matches!(ty, PortType::Note) {
            return Err(CompileError::NotYet {
                what: "note delay lines",
            });
        }
        match lines.iter_mut().find(|(l, _)| l.id == id) {
            Some((existing, seen_ty)) => {
                if *seen_ty != ty {
                    return Err(CompileError::DelayTypeMismatch { line: id });
                }
                if let Some(node_id) = writer {
                    if existing.writer != NO_WRITER {
                        return Err(CompileError::DuplicateDelayWrite { line: id });
                    }
                    existing.writer = node_id;
                }
            }
            None => {
                if lines.len() >= MAX_DELAY_LINES {
                    return Err(CompileError::TooLarge {
                        what: "delay lines",
                        limit: MAX_DELAY_LINES,
                    });
                }
                lines.push((
                    Line {
                        id,
                        writer: writer.unwrap_or(NO_WRITER),
                        ty,
                    },
                    ty,
                ));
            }
        }
    }

    Ok(lines.into_iter().map(|(l, _)| l).collect())
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

/// A register holding zero, for an input nobody has connected yet.
fn zero(
    ops: &mut Vec<Op>,
    alloc: &mut impl FnMut() -> Result<Reg, CompileError>,
) -> Result<Reg, CompileError> {
    // Reuse one if this graph already needed it. Constants are free to run but
    // not free to hold, and a wide graph can want a lot of them.
    if let Some(&Op::Const { out, .. }) = ops
        .iter()
        .find(|op| matches!(op, Op::Const { value, .. } if *value == 0.0))
    {
        return Ok(out);
    }
    let out = alloc()?;
    ops.push(Op::Const { out, value: 0.0 });
    Ok(out)
}

fn check_slot(node: NodeId, slot: usize, slot_count: usize) -> Result<(), CompileError> {
    if slot >= slot_count {
        return Err(CompileError::BadSlot { node, slot });
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
    use crate::graph::{MathOp, NodeKind, Waveform};

    const SLOTS: usize = 32;

    #[test]
    fn an_empty_graph_compiles_to_an_empty_program() {
        let program = compile(&Graph::new(), SLOTS).unwrap();
        assert!(program.is_empty());
        assert_eq!(program.registers, 0);
    }

    #[test]
    fn only_what_feeds_an_output_is_compiled() {
        let mut graph = Graph::new();
        let used = graph.add(NodeKind::Constant { value: 0.5 }, [0.0, 0.0]);
        let out = graph.add(NodeKind::SlotOut { slot: 0 }, [0.0, 0.0]);
        graph.connect(used, 0, out, 0);
        // Dropped on the canvas and wired to nothing.
        graph.add(NodeKind::Constant { value: 0.25 }, [0.0, 0.0]);
        graph.add(
            NodeKind::Lfo {
                waveform: Waveform::Sine,
                rate: Rate::Hz(1.0),
                phase: 0.0,
                depth: 0.5,
                offset: 0.5,
            },
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

    #[test]
    fn every_op_reads_only_registers_already_written() {
        let mut graph = Graph::new();
        let a = graph.add(NodeKind::Constant { value: 0.5 }, [0.0, 0.0]);
        let b = graph.add(NodeKind::SlotIn { slot: 2 }, [0.0, 0.0]);
        let sum = graph.add(
            NodeKind::Math {
                op: MathOp::Add,
                b: 0.0,
            },
            [0.0, 0.0],
        );
        let out = graph.add(NodeKind::SlotOut { slot: 1 }, [0.0, 0.0]);
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
            NodeKind::Math {
                op: MathOp::Add,
                b: 1.0,
            },
            [0.0, 0.0],
        );
        let y = graph.add(
            NodeKind::Math {
                op: MathOp::Add,
                b: 1.0,
            },
            [0.0, 0.0],
        );
        let out = graph.add(NodeKind::SlotOut { slot: 0 }, [0.0, 0.0]);
        graph.connect(x, 0, y, 0);
        graph.connect(y, 0, x, 0);
        graph.connect(y, 0, out, 0);

        assert!(matches!(
            compile(&graph, SLOTS),
            Err(CompileError::Cycle { .. })
        ));
    }

    #[test]
    fn two_nodes_driving_one_slot_is_an_error_not_a_coin_toss() {
        let mut graph = Graph::new();
        let a = graph.add(NodeKind::Constant { value: 0.1 }, [0.0, 0.0]);
        let one = graph.add(NodeKind::SlotOut { slot: 4 }, [0.0, 0.0]);
        let two = graph.add(NodeKind::SlotOut { slot: 4 }, [0.0, 0.0]);
        graph.connect(a, 0, one, 0);
        graph.connect(a, 0, two, 0);

        assert_eq!(
            compile(&graph, SLOTS),
            Err(CompileError::DuplicateOutput { slot: 4 })
        );
    }

    #[test]
    fn a_slot_outside_the_table_is_refused() {
        let mut graph = Graph::new();
        let a = graph.add(NodeKind::Constant { value: 0.1 }, [0.0, 0.0]);
        let out = graph.add(NodeKind::SlotOut { slot: 99 }, [0.0, 0.0]);
        graph.connect(a, 0, out, 0);
        assert!(matches!(
            compile(&graph, SLOTS),
            Err(CompileError::BadSlot { .. })
        ));
    }

    #[test]
    fn an_output_with_nothing_plugged_in_leaves_the_slot_to_the_daw() {
        let mut graph = Graph::new();
        graph.add(NodeKind::SlotOut { slot: 0 }, [0.0, 0.0]);
        let program = compile(&graph, SLOTS).unwrap();
        assert!(!program.drives(0));
    }

    #[test]
    fn a_graph_bigger_than_the_audio_thread_can_hold_is_refused() {
        let mut graph = Graph::new();
        let out = graph.add(NodeKind::SlotOut { slot: 0 }, [0.0, 0.0]);
        let mut last = graph.add(NodeKind::Constant { value: 0.0 }, [0.0, 0.0]);
        for _ in 0..MAX_REGISTERS + 8 {
            let next = graph.add(
                NodeKind::Math {
                    op: MathOp::Add,
                    b: 1.0,
                },
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
            NodeKind::DelayRead {
                line: 0,
                ty: PortType::Param,
                max_time: 1.0,
                time: 0.25,
            },
            [0.0, 0.0],
        );
        let scale = graph.add(
            NodeKind::Math {
                op: MathOp::Multiply,
                b: 0.5,
            },
            [0.0, 0.0],
        );
        let write = graph.add(
            NodeKind::DelayWrite {
                line: 0,
                ty: PortType::Param,
            },
            [0.0, 0.0],
        );
        let out = graph.add(NodeKind::SlotOut { slot: 0 }, [0.0, 0.0]);

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
            NodeKind::DelayRead {
                line: 7,
                ty: PortType::Param,
                max_time: 1.0,
                time: 0.1,
            },
            [0.0, 0.0],
        );
        let source = graph.add(NodeKind::Constant { value: 0.25 }, [0.0, 0.0]);
        let write = graph.add(
            NodeKind::DelayWrite {
                line: 7,
                ty: PortType::Param,
            },
            [0.0, 0.0],
        );
        let out = graph.add(NodeKind::SlotOut { slot: 0 }, [0.0, 0.0]);
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
                NodeKind::DelayWrite {
                    line: 3,
                    ty: PortType::Param,
                },
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
            NodeKind::DelayWrite {
                line: 1,
                ty: PortType::Param,
            },
            [0.0, 0.0],
        );
        graph.add(
            NodeKind::DelayRead {
                line: 1,
                ty: PortType::STEREO,
                max_time: 1.0,
                time: 0.1,
            },
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
            NodeKind::AudioIn {
                bus: 0,
                channels: 2,
            },
            [0.0, 0.0],
        );
        let out = graph.add(NodeKind::SlotOut { slot: 0 }, [0.0, 0.0]);
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
            | Op::Math { out, .. }
            | Op::Range { out, .. }
            | Op::DelayRead { out, .. } => Some(out),
            Op::DelayWrite { .. } => None,
        }
    }
}
