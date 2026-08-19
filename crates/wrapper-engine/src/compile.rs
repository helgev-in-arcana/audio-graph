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

use crate::graph::{Graph, NodeId, NodeKind, Rate};
use crate::program::{MAX_LFOS, MAX_REGISTERS, Op, Operand, Program, RateSpec, Reg};

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
        }
    }
}

impl std::error::Error for CompileError {}

/// Compile `graph` for a wrapper with `slot_count` slots.
pub fn compile(graph: &Graph, slot_count: usize) -> Result<Program, CompileError> {
    // Only what feeds an output matters. A node the user has dropped on the
    // canvas but not wired up yet must cost the audio thread nothing.
    let mut order: Vec<NodeId> = Vec::new();
    let mut mark = vec![Mark::New; graph.nodes.len()];
    let index: Vec<NodeId> = graph.nodes.iter().map(|n| n.id).collect();

    let outputs_first: Vec<NodeId> = graph
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::SlotOut { .. }))
        .map(|n| n.id)
        .collect();

    for &root in &outputs_first {
        visit(graph, &index, root, &mut mark, &mut order)?;
    }

    // Registers, in the order the ops will write them.
    let mut reg_of: Vec<(NodeId, Reg)> = Vec::new();
    let mut ops: Vec<Op> = Vec::new();
    let mut lfo_nodes: Vec<NodeId> = Vec::new();
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

    let lookup = |reg_of: &Vec<(NodeId, Reg)>, id: NodeId| -> Option<Reg> {
        reg_of.iter().find(|&&(n, _)| n == id).map(|&(_, r)| r)
    };

    for &id in &order {
        let node = graph.node(id).expect("ordering only contains real nodes");
        let input = |slot: u8| -> Option<Reg> {
            graph
                .source_of(id, slot)
                .and_then(|from| lookup(&reg_of, from))
        };

        match node.kind {
            NodeKind::Constant { value } => {
                let out = alloc()?;
                ops.push(Op::Const { out, value });
                reg_of.push((id, out));
            }
            NodeKind::SlotIn { slot } => {
                check_slot(id, slot, slot_count)?;
                let out = alloc()?;
                ops.push(Op::Slot {
                    out,
                    slot: slot as u16,
                });
                reg_of.push((id, out));
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
                reg_of.push((id, out));
            }
            NodeKind::Expression { source } => {
                let out = alloc()?;
                ops.push(Op::Expr { out, source });
                reg_of.push((id, out));
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
                reg_of.push((id, out));
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
                reg_of.push((id, out));
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
        }
    }

    outputs.sort_unstable();
    Ok(Program {
        ops,
        registers: next_reg,
        outputs,
        lfo_nodes,
    })
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
    for input in 0..node.kind.inputs().len() as u8 {
        if let Some(from) = graph.source_of(id, input) {
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
        graph.connect(used, out, 0);
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
        graph.connect(sum, out, 0);
        graph.connect(b, sum, 1);
        graph.connect(a, sum, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let mut written = vec![false; program.registers];
        for op in &program.ops {
            for read in reads(op) {
                assert!(
                    written[read as usize],
                    "op reads register {read} before it is written"
                );
            }
            written[writes(op) as usize] = true;
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
        graph.connect(x, y, 0);
        graph.connect(y, x, 0);
        graph.connect(y, out, 0);

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
        graph.connect(a, one, 0);
        graph.connect(a, two, 0);

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
        graph.connect(a, out, 0);
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
            graph.connect(last, next, 0);
            last = next;
        }
        graph.connect(last, out, 0);
        assert!(matches!(
            compile(&graph, SLOTS),
            Err(CompileError::TooLarge { .. })
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
        }
    }

    fn writes(op: &Op) -> Reg {
        match *op {
            Op::Const { out, .. }
            | Op::Slot { out, .. }
            | Op::Lfo { out, .. }
            | Op::Expr { out, .. }
            | Op::Math { out, .. }
            | Op::Range { out, .. } => out,
        }
    }
}
