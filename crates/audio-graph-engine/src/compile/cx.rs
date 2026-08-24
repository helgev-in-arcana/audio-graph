//! What a node is handed while the parameter half is being compiled.
//!
//! Every local the old single loop kept — the register counter, the op list,
//! the lane books — is a field here, and every closure it defined is a method.
//! That is the whole change: a node's arm used to reach into the loop's
//! variables, and now it asks for what it needs. It cannot see another node's
//! registers, cannot renumber a lane, and cannot append to `ops` out of turn.
//!
//! The order in which these methods are called is still what decides register
//! and lane numbering, and that is deliberate: the audio thread indexes both
//! without checking, so the numbering has to come from somewhere
//! deterministic. It comes from the topological order, and from the order of
//! the calls inside each node.

use super::{CompileError, Line};
use crate::graph::{Graph, LineId, NodeId};
use crate::ir::{MAX_AUDIO_LANES, MAX_GRAPH_PARAMS, MAX_LFOS, MAX_REGISTERS, Op, ParamTarget, Reg};

pub(crate) struct ParamCx<'a> {
    graph: &'a Graph,
    lines: &'a [Line],
    slot_count: usize,
    /// The node currently being compiled. Set by [`ParamCx::begin`].
    id: NodeId,

    next_reg: usize,
    reg_of: Vec<((NodeId, u8), Reg)>,
    ops: Vec<Op>,
    deferred: Vec<Op>,
    outputs: Vec<(u16, Reg)>,
    lfo_nodes: Vec<NodeId>,
    param_targets: Vec<ParamTarget>,
    audio_lanes: Vec<((NodeId, u8), u16)>,
}

/// What the parameter half produced.
pub(crate) struct ParamHalf {
    pub ops: Vec<Op>,
    pub registers: usize,
    pub outputs: Vec<(u16, Reg)>,
    pub lfo_nodes: Vec<NodeId>,
    pub param_targets: Vec<ParamTarget>,
    pub audio_lanes: Vec<((NodeId, u8), u16)>,
}

impl<'a> ParamCx<'a> {
    pub(crate) fn new(graph: &'a Graph, lines: &'a [Line], slot_count: usize) -> ParamCx<'a> {
        ParamCx {
            graph,
            lines,
            slot_count,
            id: NodeId::MAX,
            next_reg: 0,
            reg_of: Vec::new(),
            ops: Vec::new(),
            deferred: Vec::new(),
            outputs: Vec::new(),
            lfo_nodes: Vec::new(),
            param_targets: Vec::new(),
            audio_lanes: Vec::new(),
        }
    }

    /// Say which node the calls that follow belong to.
    pub(crate) fn begin(&mut self, id: NodeId) {
        self.id = id;
    }

    pub(crate) fn finish(mut self) -> ParamHalf {
        self.ops.append(&mut self.deferred);
        self.outputs.sort_unstable();
        ParamHalf {
            ops: self.ops,
            registers: self.next_reg,
            outputs: self.outputs,
            lfo_nodes: self.lfo_nodes,
            param_targets: self.param_targets,
            audio_lanes: self.audio_lanes,
        }
    }

    // --- reading what is wired in ----------------------------------------

    /// The register holding what is plugged into this node's input `port`, if
    /// anything is.
    ///
    /// Only ever asked for `Param` inputs; the type check that makes that safe
    /// is in `check_links`.
    pub(crate) fn input(&self, port: u8) -> Option<Reg> {
        let from = self.graph.source_of(self.id, port)?;
        self.reg_of
            .iter()
            .find(|&&(key, _)| key == from)
            .map(|&(_, reg)| reg)
    }

    /// Like [`ParamCx::input`], but an unconnected socket reads as zero.
    ///
    /// The identity element for each operator would be a slightly nicer
    /// answer, but "nothing plugged in reads as zero" is one rule instead of
    /// six.
    pub(crate) fn input_or_zero(&mut self, port: u8) -> Result<Reg, CompileError> {
        match self.input(port) {
            Some(reg) => Ok(reg),
            None => self.zero(),
        }
    }

    /// A register holding zero, for an input nobody has connected yet.
    fn zero(&mut self) -> Result<Reg, CompileError> {
        // Reuse one if this graph already needed it. Constants are free to run
        // but not free to hold, and a wide graph can want a lot of them.
        if let Some(&Op::Const { out, .. }) = self
            .ops
            .iter()
            .find(|op| matches!(op, Op::Const { value, .. } if *value == 0.0))
        {
            return Ok(out);
        }
        let out = self.alloc()?;
        self.emit(Op::Const { out, value: 0.0 });
        Ok(out)
    }

    // --- emitting ---------------------------------------------------------

    /// Book the next register. The call order is what decides the numbering.
    pub(crate) fn alloc(&mut self) -> Result<Reg, CompileError> {
        if self.next_reg >= MAX_REGISTERS {
            return Err(CompileError::TooLarge {
                what: "nodes",
                limit: MAX_REGISTERS,
            });
        }
        let reg = self.next_reg as Reg;
        self.next_reg += 1;
        Ok(reg)
    }

    pub(crate) fn emit(&mut self, op: Op) {
        self.ops.push(op);
    }

    /// Emit an op that runs after every other op in the program.
    ///
    /// Only delay writes use this, and §14.4 is why: where a `DelayWrite` lands
    /// in the topological order is not determined when nothing downstream reads
    /// the line, and "sometimes a sub-block earlier" is not a semantics anyone
    /// can reason about. Putting every write at the end makes one rule — a read
    /// sees the line as it stood at the end of the previous sub-block — and
    /// that rule belongs to the compiler rather than to the delay node, which
    /// is why it is spelled out here rather than there.
    pub(crate) fn emit_deferred(&mut self, op: Op) {
        self.deferred.push(op);
    }

    /// Say that this node's output `port` is the value in `reg`.
    ///
    /// Keyed by socket rather than by node: a plugin node has one per bus.
    pub(crate) fn bind_output(&mut self, port: u8, reg: Reg) {
        self.reg_of.push(((self.id, port), reg));
    }

    // --- the scarce, numbered things --------------------------------------

    /// Book this node a slot in the LFO state table.
    ///
    /// The table is what survives a program swap, so that recompiling — which
    /// happens on every drag of every knob — does not restart an oscillator.
    pub(crate) fn lfo_state(&mut self) -> Result<u16, CompileError> {
        if self.lfo_nodes.len() >= MAX_LFOS {
            return Err(CompileError::TooLarge {
                what: "LFOs",
                limit: MAX_LFOS,
            });
        }
        let state = self.lfo_nodes.len() as u16;
        self.lfo_nodes.push(self.id);
        Ok(state)
    }

    /// Where `line` ended up in the program's numbering.
    pub(crate) fn line_index(&self, line: LineId) -> u16 {
        self.lines
            .iter()
            .position(|l| l.id == line)
            .expect("collect_lines saw every delay node") as u16
    }

    pub(crate) fn check_slot(&self, slot: usize) -> Result<(), CompileError> {
        if slot >= self.slot_count {
            return Err(CompileError::BadSlot {
                node: self.id,
                slot,
            });
        }
        Ok(())
    }

    /// Refuse a second output on one slot.
    ///
    /// Silently letting one win would make the graph's behaviour depend on
    /// node creation order.
    pub(crate) fn claim_slot(&self, slot: usize) -> Result<(), CompileError> {
        if self.outputs.iter().any(|&(s, _)| s as usize == slot) {
            return Err(CompileError::DuplicateOutput { slot });
        }
        Ok(())
    }

    /// Drive a wrapper slot from `reg`, replacing the DAW's automation for it.
    pub(crate) fn drive_slot(&mut self, slot: usize, reg: Reg) {
        self.outputs.push((slot as u16, reg));
    }

    /// Drive one of a sub-plugin's own parameters from `reg` (§14.12).
    ///
    /// Two sockets naming one parameter is a patch the user can draw; the last
    /// one to compile wins, which at least is a rule rather than an accident of
    /// node order.
    pub(crate) fn drive_param(
        &mut self,
        target: ParamTarget,
        reg: Reg,
    ) -> Result<(), CompileError> {
        if self.param_targets.len() >= MAX_GRAPH_PARAMS {
            return Err(CompileError::TooLarge {
                what: "graph-driven parameters",
                limit: MAX_GRAPH_PARAMS,
            });
        }
        match self.param_targets.iter().position(|t| *t == target) {
            Some(lane) => {
                let lane = self.slot_count + lane;
                self.outputs.retain(|&(l, _)| l as usize != lane);
                self.outputs.push((lane as u16, reg));
            }
            None => {
                self.param_targets.push(target);
                let lane = self.slot_count + self.param_targets.len() - 1;
                self.outputs.push((lane as u16, reg));
            }
        }
        Ok(())
    }

    /// Carry the value in `reg` across to the audio half, as this node's
    /// control on socket `port` (§14.5, §14.12).
    ///
    /// A range of lane numbers of its own, past the slot table and past the
    /// parameter lanes, so that each consumer reads only what it understands:
    /// the sub-plugin adapter never sees one of these, and the audio half never
    /// sees a parameter.
    pub(crate) fn drive_audio(&mut self, port: u8, reg: Reg) -> Result<(), CompileError> {
        let socket = (self.id, port);
        let lane = match self.audio_lanes.iter().find(|&&(s, _)| s == socket) {
            Some(&(_, lane)) => lane,
            None => {
                if self.audio_lanes.len() >= MAX_AUDIO_LANES {
                    return Err(CompileError::TooLarge {
                        what: "automated delay times and gains",
                        limit: MAX_AUDIO_LANES,
                    });
                }
                let lane = (self.slot_count + MAX_GRAPH_PARAMS + self.audio_lanes.len()) as u16;
                self.audio_lanes.push((socket, lane));
                lane
            }
        };
        self.outputs.push((lane, reg));
        Ok(())
    }
}
