//! Node compilation contexts for parameter and audio execution pipelines.
//!
//! Provides [`ParamCx`], [`AudioCx`], and [`DeclareCx`], handed to nodes during
//! the compilation passes. They handle register allocation, instruction
//! emission, audio buffer lifecycle and note routing.
//!
//! Every local a single compile loop would keep — the register counter, the op
//! list, the lane books — is a field on one of these, and every closure it
//! would define is a method. That is what stops a node's arm from reaching into
//! the loop's variables: it cannot see another node's registers, cannot
//! renumber a lane, and cannot append to `ops` out of turn.
//!
//! **The order in which these methods are called is what decides register and
//! lane numbering.** That is deliberate: the audio thread indexes both without
//! checking, so the numbering has to come from somewhere deterministic. It
//! comes from the topological order, and from the order of the calls inside
//! each node.

use super::audio::Audio;
use super::{CompileError, Line, NO_WRITER};
use crate::graph::{Graph, LineId, NodeId};
use crate::ir::{
    AudioOp, Buf, Chunking, MAX_AUDIO_DELAY_LINES, MAX_AUDIO_DELAY_SECONDS, MAX_AUDIO_LANES,
    MAX_BUFFERS, MAX_COMPENSATION, MAX_COMPENSATORS, MAX_DELAY_LINES, MAX_GRAPH_PARAMS,
    MAX_LATCHES, MAX_LFOS, MAX_REGISTERS, NoteRoute, Op, Reg,
};

/// Offset added to an output socket index when filing a note gate's lane, so it
/// cannot collide with the lane of an input socket of the same number.
///
/// A lane is keyed by `(node, socket)` and every other user of one means an
/// *input* socket by it. A gate belongs to an output, and a node has both.
/// Adding this rather than widening the key keeps every existing lookup as it
/// was.
const OUTPUT_SOCKET: u8 = 128;

/// Walks up a note chain from `(node, port)` to whatever makes the notes.
///
/// Returns the source, the gate socket nearest the reader, and the accumulated
/// key mute mask. Nearest, because each gate already folds the ones above it
/// into its own condition, so the closest one is the whole answer.
fn trace_notes(graph: &Graph, node: NodeId, port: u8) -> (NoteSource, Option<(NodeId, u8)>, u128) {
    let mut at = graph.source_of(node, port);
    let mut gate = None;
    let mut mute = 0u128;
    for _ in 0..=graph.nodes.len() {
        let Some((from, from_port)) = at else {
            return (NoteSource::None, gate, mute);
        };
        let Some(node) = graph.node(from) else {
            return (NoteSource::None, gate, mute);
        };
        if let Some(source) = node.kind.note_identity() {
            return (source, gate, mute);
        }
        let Some(input) = node.kind.note_passthrough(from_port) else {
            return (NoteSource::None, gate, mute);
        };
        mute |= node.kind.note_mute(from_port);
        gate = gate.or(Some((from, from_port)));
        at = graph.source_of(from, input);
    }
    (NoteSource::None, gate, mute)
}
use crate::nodes::NodeKind;
use crate::port::PortType;
use subhost_adapter::{InstanceIo, NoteSource, ParamTarget};

/// What a node is handed while the parameter half is being compiled.
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
    latch_nodes: Vec<NodeId>,
    param_targets: Vec<ParamTarget>,
    audio_lanes: Vec<((NodeId, u8), u16)>,
    /// Output socket → the register saying whether notes leaving it pass.
    /// Read by the gate downstream of it, never by the audio half, which reads
    /// the lane instead.
    note_gates: Vec<((NodeId, u8), Reg)>,
}

/// What the parameter half produced.
pub(crate) struct ParamHalf {
    pub ops: Vec<Op>,
    pub registers: usize,
    pub outputs: Vec<(u16, Reg)>,
    pub lfo_nodes: Vec<NodeId>,
    pub latch_nodes: Vec<NodeId>,
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
            latch_nodes: Vec::new(),
            param_targets: Vec::new(),
            audio_lanes: Vec::new(),
            note_gates: Vec::new(),
        }
    }

    /// Says which node the calls that follow belong to.
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
            latch_nodes: self.latch_nodes,
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
    /// The identity element for each operator would be a slightly nicer answer,
    /// but "nothing plugged in reads as zero" is one rule instead of six.
    pub(crate) fn input_or_zero(&mut self, port: u8) -> Result<Reg, CompileError> {
        match self.input(port) {
            Some(reg) => Ok(reg),
            None => self.zero(),
        }
    }

    /// Whether anything is wired to input `port`.
    ///
    /// For the sockets that carry no register — a notes port — where
    /// [`ParamCx::input`] cannot tell "nothing wired" from "wired to something
    /// that binds no register".
    pub(crate) fn has_input(&self, port: u8) -> bool {
        self.graph.source_of(self.id, port).is_some()
    }

    /// A register holding zero, for an input nobody has connected yet.
    ///
    /// Reused across the program if one was already needed: constants are free
    /// to run but not free to hold, and a wide graph can want a lot of them.
    pub(crate) fn zero(&mut self) -> Result<Reg, CompileError> {
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

    /// Books the next register. The call order is what decides the numbering.
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

    /// Emits an op that runs after every other op in the program.
    ///
    /// Only delay writes use this. Where a `DelayWrite` lands in the
    /// topological order is not determined when nothing downstream reads the
    /// line, and "sometimes a sub-block earlier" is not a semantics anyone can
    /// reason about. Putting every write at the end makes one rule — a read
    /// sees the line as it stood at the end of the previous sub-block — and
    /// that rule belongs to the compiler rather than to the delay node, which
    /// is why it is spelled out here rather than there.
    pub(crate) fn emit_deferred(&mut self, op: Op) {
        self.deferred.push(op);
    }

    /// Says that this node's output `port` is the value in `reg`.
    ///
    /// Keyed by socket rather than by node: a plugin node has one per bus.
    pub(crate) fn bind_output(&mut self, port: u8, reg: Reg) {
        self.reg_of.push(((self.id, port), reg));
    }

    // --- the scarce, numbered things --------------------------------------

    /// Books this node a slot in the LFO state table.
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

    /// Books this node a latch, which is what survives a program swap — see
    /// [`Op::KeyToggle`][crate::Op::KeyToggle].
    pub(crate) fn latch(&mut self) -> Result<u16, CompileError> {
        if self.latch_nodes.len() >= MAX_LATCHES {
            return Err(CompileError::TooLarge {
                what: "key switches",
                limit: MAX_LATCHES,
            });
        }
        let state = self.latch_nodes.len() as u16;
        self.latch_nodes.push(self.id);
        Ok(state)
    }

    /// Returns the compiled program index for `line`.
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

    /// Drives one of a sub-plugin's own parameters from `reg`.
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

    /// The gate condition on the note chain feeding this node's input `port`,
    /// if there is one upstream.
    ///
    /// A gate node asks for this so it can fold the gates above it into its own
    /// condition: two gates in series pass notes only when both are open, and
    /// one register saying so is cheaper than the audio half carrying a list.
    pub(crate) fn upstream_note_gate(&self, port: u8) -> Option<Reg> {
        let (_, socket, _) = trace_notes(self.graph, self.id, port);
        let socket = socket?;
        self.note_gates
            .iter()
            .find(|&&(key, _)| key == socket)
            .map(|&(_, reg)| reg)
    }

    /// Says that the notes leaving this node's output `port` pass only while
    /// `reg` is 1.
    ///
    /// Booked as an audio lane, because the audio half is where the decision is
    /// applied and the two halves run at different rates.
    pub(crate) fn bind_note_gate(&mut self, port: u8, reg: Reg) -> Result<(), CompileError> {
        self.note_gates.push(((self.id, port), reg));
        self.drive_audio(OUTPUT_SOCKET + port, reg)
    }

    /// Carries the value in `reg` across to the audio half, as this node's
    /// control on socket `port`.
    ///
    /// These get a range of lane numbers of their own, past the slot table and
    /// past the parameter lanes, so that each consumer reads only what it
    /// understands: the sub-plugin adapter never sees one of these, and the
    /// audio half never sees a parameter.
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

/// One node's audio output, once it has been emitted.
struct Produced {
    node: NodeId,
    /// Which of the node's output sockets this is. Only a plugin node has more
    /// than one; everything else produces port 0.
    port: u8,
    buf: Buf,
    /// Samples of delay accumulated on the way here. Two of these arriving at
    /// one `Mix` with different values is what latency compensation exists to
    /// fix: without it the short branch runs ahead and the two phase-cancel.
    latency: u32,
}

/// Hands out audio buffers and takes them back.
///
/// A linear-scan register allocator, with the one wrinkle that buffers have a
/// width: a stereo buffer cannot stand in for a mono one, so the free list is
/// searched by width rather than popped.
struct Pool {
    widths: Vec<u16>,
    /// How many reads of each buffer are still to come. Zero means free.
    pending: Vec<usize>,
}

impl Pool {
    fn new() -> Pool {
        Pool {
            widths: Vec::new(),
            pending: Vec::new(),
        }
    }

    fn alloc(&mut self, channels: u16, readers: usize) -> Result<Buf, CompileError> {
        if let Some(i) =
            (0..self.widths.len()).find(|&i| self.pending[i] == 0 && self.widths[i] == channels)
        {
            self.pending[i] = readers;
            return Ok(i as Buf);
        }
        if self.widths.len() >= MAX_BUFFERS {
            return Err(CompileError::TooLarge {
                what: "audio buffers",
                limit: MAX_BUFFERS,
            });
        }
        self.widths.push(channels);
        self.pending.push(readers);
        Ok((self.widths.len() - 1) as Buf)
    }

    /// Like `alloc`, but never returns one of `avoid`.
    ///
    /// Two callers need this, for different reasons. A plugin reads its input
    /// and writes its output, and whether those may be the same memory is a
    /// question about the plugin's internals that a host has no way to ask, so
    /// it is never asked. A `Mix` may accumulate into its first input, but the
    /// moment it does, that buffer stops holding what the *other* inputs expect
    /// to be summed with — so all but the first are off limits.
    ///
    /// Implemented by parking the buffers rather than by filtering, so there is
    /// exactly one place that knows how a free buffer is chosen.
    fn alloc_avoiding(
        &mut self,
        channels: u16,
        readers: usize,
        avoid: &[Buf],
    ) -> Result<Buf, CompileError> {
        let saved: Vec<usize> = avoid
            .iter()
            .map(|&b| std::mem::replace(&mut self.pending[b as usize], usize::MAX))
            .collect();
        let got = self.alloc(channels, readers);
        for (&b, was) in avoid.iter().zip(saved) {
            self.pending[b as usize] = was;
        }
        got
    }

    fn width_of(&self, buf: Buf) -> u16 {
        self.widths[buf as usize]
    }

    /// Decrements the pending reader count for `buf`.
    fn consume(&mut self, buf: Buf) {
        let slot = &mut self.pending[buf as usize];
        *slot = slot.saturating_sub(1);
    }
}
// ---------------------------------------------------------------------------

/// What a node is handed while the audio half is being compiled.
///
/// The same idea as [`ParamCx`], with three things the param half never has to
/// deal with. Buffers are expensive enough to reuse, so they are handed out by
/// a pool rather than counted off; a node may say which buffers a new one must
/// *not* be ([`AudioCx::alloc_avoiding`]); and what a node produces carries an
/// accumulated latency, which is what a merge has to line up.
pub(crate) struct AudioCx<'a> {
    graph: &'a Graph,
    lines: &'a [Line],
    /// The nodes the program runs, in order. A link is a read only when the
    /// node at its far end is one of these.
    order: &'a [NodeId],
    lanes: &'a [((NodeId, u8), u16)],
    id: NodeId,

    /// Where each audio input's signal came from, in port order. `None` for an
    /// input nobody wired, which is silence rather than an error — the same
    /// rule the param half uses.
    sources: Vec<Option<(Buf, u32)>>,
    /// Total number of connections leaving this node across all output ports.
    readers: usize,
    /// Channel width of this node's first output socket, or 0 if it has none.
    out_width: u16,

    pool: Pool,
    produced: Vec<Produced>,
    ops: Vec<AudioOp>,
    deferred: Vec<AudioOp>,
    instances: Vec<InstanceIo>,
    latency: u32,
    compensators: u16,
    audio_lines: Vec<LineId>,
    delay_nodes: Vec<NodeId>,
    ring_seconds: Vec<f64>,
}

impl<'a> AudioCx<'a> {
    pub(crate) fn new(
        graph: &'a Graph,
        lines: &'a [Line],
        order: &'a [NodeId],
        lanes: &'a [((NodeId, u8), u16)],
    ) -> AudioCx<'a> {
        AudioCx {
            graph,
            lines,
            order,
            lanes,
            id: NodeId::MAX,
            sources: Vec::new(),
            readers: 0,
            out_width: 0,
            pool: Pool::new(),
            produced: Vec::new(),
            ops: Vec::new(),
            deferred: Vec::new(),
            instances: Vec::new(),
            latency: 0,
            compensators: 0,
            audio_lines: Vec::new(),
            delay_nodes: Vec::new(),
            ring_seconds: Vec::new(),
        }
    }

    /// Says which node the calls that follow belong to, and works out what is
    /// wired into it.
    ///
    /// Audio lines are numbered among themselves: their rings are a scarcer
    /// resource than a param line's, so they get their own ceiling and their
    /// own index space.
    pub(crate) fn begin(&mut self, id: NodeId, kind: &NodeKind) {
        self.id = id;
        self.sources = kind
            .input_ports()
            .iter()
            .enumerate()
            .filter(|(_, p)| matches!(p.ty, PortType::Audio { .. }))
            .map(|(port, _)| {
                self.graph
                    .source_of(id, port as u8)
                    .and_then(|(from, from_port)| {
                        self.produced
                            .iter()
                            .find(|p| p.node == from && p.port == from_port)
                    })
                    .map(|p| (p.buf, p.latency))
            })
            .collect();
        self.readers = self
            .graph
            .links
            .iter()
            .filter(|l| l.from == id && self.runs(l.to))
            .count();
        self.out_width = match kind.output_ports().first().map(|p| p.ty) {
            Some(PortType::Audio { channels }) => channels,
            _ => 0,
        };
    }

    pub(crate) fn finish(mut self) -> Audio {
        self.ops.append(&mut self.deferred);

        // An audio line with both halves present closes a loop, and then every
        // plugin in the program has to run at sub-block granularity.
        let looped = self
            .lines
            .iter()
            .any(|line| matches!(line.ty, PortType::Audio { .. }) && line.writer != NO_WRITER);

        self.instances.sort_unstable_by_key(|i| i.instance);
        Audio {
            instances: self.instances,
            ops: self.ops,
            delay_nodes: self.delay_nodes,
            ring_seconds: self.ring_seconds,
            buffers: self.pool.widths,
            chunking: if looped {
                Chunking::SubBlock
            } else {
                Chunking::WholeBlock
            },
            latency: self.latency,
        }
    }

    // --- what is wired in -------------------------------------------------

    /// The buffer and accumulated latency on this node's audio input `index`,
    /// counting only the audio sockets.
    pub(crate) fn source(&self, index: usize) -> Option<(Buf, u32)> {
        self.sources.get(index).copied().flatten()
    }

    /// Every audio input in socket order, wired or not.
    pub(crate) fn sources(&self) -> &[Option<(Buf, u32)>] {
        &self.sources
    }

    /// Whether `node` is one of the nodes this program runs.
    ///
    /// Every question of the form "does anything read this?" goes through
    /// here. A node that reaches no output is not compiled and so never reads
    /// what is wired to it: its buffer would be held for a read that never
    /// comes, and its bus handed over for nobody to collect.
    fn runs(&self, node: NodeId) -> bool {
        self.order.contains(&node)
    }

    /// How many of the links leaving this node are read by a node that runs.
    pub(crate) fn readers(&self) -> usize {
        self.readers
    }

    /// The same count for one particular output socket.
    pub(crate) fn readers_of(&self, port: u8) -> usize {
        self.graph
            .links
            .iter()
            .filter(|l| l.from == self.id && l.from_port == port && self.runs(l.to))
            .count()
    }

    /// One past the highest output socket anything reads, or 0 if nothing does.
    pub(crate) fn outputs_read(&self) -> usize {
        self.graph
            .links
            .iter()
            .filter(|l| l.from == self.id && self.runs(l.to))
            .map(|l| l.from_port as usize + 1)
            .max()
            .unwrap_or(0)
    }

    /// The node being compiled, for an error that has to name it.
    pub(crate) fn node(&self) -> NodeId {
        self.id
    }

    pub(crate) fn out_width(&self) -> u16 {
        self.out_width
    }

    /// The lane the param half booked for one of this node's sockets, if it
    /// booked one.
    pub(crate) fn lane(&self, port: u8) -> Option<u16> {
        self.lanes
            .iter()
            .find(|&&(socket, _)| socket == (self.id, port))
            .map(|&(_, lane)| lane)
    }

    /// How notes reach this node's input `port`.
    ///
    /// `None` when nothing is connected, which is the answer that makes an
    /// unwired instrument silent rather than making it play whatever the DAW
    /// happened to send. Only `NoteIn` produces notes today; a plugin's own
    /// note output would need the engine to carry event buffers. What sits
    /// between the source and here — gates — comes back as a lane number for
    /// the audio half to read each chunk.
    pub(crate) fn note_route(&self, port: u8) -> NoteRoute {
        let (source, socket, mute) = trace_notes(self.graph, self.id, port);
        let gate = socket.and_then(|(node, out_port)| {
            self.lanes
                .iter()
                .find(|&&(key, _)| key == (node, OUTPUT_SOCKET + out_port))
                .map(|&(_, lane)| lane)
        });
        NoteRoute { source, gate, mute }
    }

    // --- buffers ----------------------------------------------------------

    pub(crate) fn alloc(&mut self, channels: u16, readers: usize) -> Result<Buf, CompileError> {
        self.pool.alloc(channels, readers)
    }

    /// Allocates an audio buffer, avoiding any buffer in `avoid`.
    pub(crate) fn alloc_avoiding(
        &mut self,
        channels: u16,
        readers: usize,
        avoid: &[Buf],
    ) -> Result<Buf, CompileError> {
        self.pool.alloc_avoiding(channels, readers, avoid)
    }

    pub(crate) fn width_of(&self, buf: Buf) -> u16 {
        self.pool.width_of(buf)
    }

    /// Decrements the pending reader count for `buf`.
    pub(crate) fn consume(&mut self, buf: Buf) {
        self.pool.consume(buf);
    }

    // --- emitting ---------------------------------------------------------

    pub(crate) fn emit(&mut self, op: AudioOp) {
        self.ops.push(op);
    }

    /// Emits an op that runs after every other op in the chunk.
    ///
    /// Only delay writes use this, for the reason the param half holds its
    /// writes back: within one chunk every read must see the line as it stood
    /// before this chunk was written, or a delay of exactly one chunk would
    /// read back what it had just written.
    pub(crate) fn emit_deferred(&mut self, op: AudioOp) {
        self.deferred.push(op);
    }

    /// Says what this node leaves in `buf` on its output socket `port`, and how
    /// much delay it has accumulated getting there.
    pub(crate) fn produce(&mut self, port: u8, buf: Buf, latency: u32) {
        self.produced.push(Produced {
            node: self.id,
            port,
            buf,
            latency,
        });
    }

    /// Reports a path that reaches the DAW, so the wrapper can tell it how far
    /// behind the audio is.
    pub(crate) fn report_latency(&mut self, latency: u32) {
        self.latency = self.latency.max(latency);
    }

    /// Delays one branch of a merge so it lines up with the latest one.
    ///
    /// The alternative to doing this is the two branches phase-cancelling, so
    /// running out of compensators is refused rather than skipped.
    pub(crate) fn compensate(&mut self, buf: Buf, samples: u32) -> Result<(), CompileError> {
        if self.compensators as usize >= MAX_COMPENSATORS {
            return Err(CompileError::TooLarge {
                what: "compensated paths",
                limit: MAX_COMPENSATORS,
            });
        }
        if samples as usize >= MAX_COMPENSATION {
            return Err(CompileError::TooLarge {
                what: "samples of delay compensation",
                limit: MAX_COMPENSATION,
            });
        }
        let slot = self.compensators;
        self.compensators += 1;
        self.emit(AudioOp::Compensate { buf, slot, samples });
        Ok(())
    }

    /// Says how a sub-plugin instance has to be activated: which buses, how
    /// wide.
    pub(crate) fn declare_instance(&mut self, io: InstanceIo) {
        self.instances.push(io);
    }

    // --- delay lines ------------------------------------------------------

    /// The audio index of `line`, assigning one if this is its first mention.
    ///
    /// `delay_nodes` grows alongside, so index `i` always names the writer of
    /// the line at `audio_lines[i]` — that pairing is what lets a program swap
    /// keep the ring contents.
    pub(crate) fn audio_line(&mut self, line: LineId) -> Result<u16, CompileError> {
        if let Some(index) = self.audio_lines.iter().position(|&l| l == line) {
            return Ok(index as u16);
        }
        if self.audio_lines.len() >= MAX_AUDIO_DELAY_LINES {
            return Err(CompileError::TooLarge {
                what: "audio delay lines",
                limit: MAX_AUDIO_DELAY_LINES,
            });
        }
        self.audio_lines.push(line);
        self.ring_seconds.push(0.0);
        self.delay_nodes.push(
            self.lines
                .iter()
                .find(|l| l.id == line)
                .map(|l| l.writer)
                .unwrap_or(NO_WRITER),
        );
        Ok((self.audio_lines.len() - 1) as u16)
    }

    /// Asks that line `index`'s ring be long enough for `seconds`.
    ///
    /// Several reads may share a line — that is a multi-tap delay — and the
    /// ring has to be long enough for the furthest of them.
    pub(crate) fn want_ring(&mut self, index: u16, seconds: f64) {
        let slot = &mut self.ring_seconds[index as usize];
        *slot = slot.max(seconds.clamp(0.0, MAX_AUDIO_DELAY_SECONDS));
    }
}

// ---------------------------------------------------------------------------

/// What a node is handed before either half is compiled.
///
/// One pass over the graph, in which a node says what it needs that the
/// compiler has to know about *before* it starts emitting: today that is delay
/// lines, and nothing else. The pass exists so the compiler does not have to
/// recognise node kinds — it would otherwise have to find the two halves of a
/// delay line by matching on `DelayWrite` and `DelayRead` itself.
///
/// Deliberately in graph order rather than topological order: line numbering
/// comes from where the writers sit in the patch, and a `DelayRead` compiled
/// early has to already know its line index.
pub(crate) struct DeclareCx {
    id: NodeId,
    lines: Vec<Line>,
}

impl DeclareCx {
    pub(crate) fn new() -> DeclareCx {
        DeclareCx {
            id: NodeId::MAX,
            lines: Vec::new(),
        }
    }

    pub(crate) fn begin(&mut self, id: NodeId) {
        self.id = id;
    }

    pub(crate) fn finish(self) -> Vec<Line> {
        self.lines
    }

    /// Says that this node is one end of delay line `line`, carrying `ty`.
    ///
    /// A line with reads but no write is not an error: it reads silence, which
    /// is what a half-drawn patch should do. A line with a write and no read is
    /// not an error either — it is just unread. Two writers is an error, for
    /// the same reason two outputs on one slot are: which one wins would
    /// otherwise depend on node creation order.
    pub(crate) fn declare_line(
        &mut self,
        line: LineId,
        ty: PortType,
        writes: bool,
    ) -> Result<(), CompileError> {
        if matches!(ty, PortType::Note) {
            return Err(CompileError::NotYet {
                what: "note delay lines",
            });
        }
        match self.lines.iter_mut().find(|l| l.id == line) {
            Some(existing) => {
                if existing.ty != ty {
                    return Err(CompileError::DelayTypeMismatch { line });
                }
                if writes {
                    if existing.writer != NO_WRITER {
                        return Err(CompileError::DuplicateDelayWrite { line });
                    }
                    existing.writer = self.id;
                }
            }
            None => {
                if self.lines.len() >= MAX_DELAY_LINES {
                    return Err(CompileError::TooLarge {
                        what: "delay lines",
                        limit: MAX_DELAY_LINES,
                    });
                }
                self.lines.push(Line {
                    id: line,
                    writer: if writes { self.id } else { NO_WRITER },
                    ty,
                });
            }
        }
        Ok(())
    }
}
