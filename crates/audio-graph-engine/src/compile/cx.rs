//! Node compilation contexts for parameter and audio execution pipelines.
//!
//! Provides [`ParamCx`], [`AudioCx`], and [`DeclareCx`] contexts passed to nodes during
//! compilation passes. These contexts handle register allocation, instruction emission,
//! audio buffer lifecycle management, and note routing.

use super::audio::Audio;
use super::{CompileError, Line, NO_WRITER};
use crate::graph::{Graph, LineId, NodeId};
use crate::ir::{
    AudioOp, Buf, Chunking, MAX_AUDIO_DELAY_LINES, MAX_AUDIO_DELAY_SECONDS, MAX_AUDIO_LANES,
    MAX_BUFFERS, MAX_COMPENSATION, MAX_COMPENSATORS, MAX_DELAY_LINES, MAX_GRAPH_PARAMS,
    MAX_LATCHES, MAX_LFOS, MAX_REGISTERS, NoteRoute, Op, Reg,
};

/// Offset added to output socket index when indexing note gate lanes to distinguish them from input lanes.
const OUTPUT_SOCKET: u8 = 128;

/// Traces upstream note connections from `(node, port)` to determine the origin note source,
/// the nearest note gate socket, and the accumulated key mute bitmask.
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

/// Compilation context for scalar/parameter operations.
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
    /// Output socket -> register containing note gate condition.
    note_gates: Vec<((NodeId, u8), Reg)>,
}

/// Compilation artifacts produced for the parameter evaluation pipeline.
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

    /// Sets the node currently being compiled.
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

    /// Returns the register holding the value plugged into input `port`, if connected.
    pub(crate) fn input(&self, port: u8) -> Option<Reg> {
        let from = self.graph.source_of(self.id, port)?;
        self.reg_of
            .iter()
            .find(|&&(key, _)| key == from)
            .map(|&(_, reg)| reg)
    }

    /// Returns the register connected to input `port`, or allocates a zero constant register if unconnected.
    pub(crate) fn input_or_zero(&mut self, port: u8) -> Result<Reg, CompileError> {
        match self.input(port) {
            Some(reg) => Ok(reg),
            None => self.zero(),
        }
    }

    /// Checks whether anything is connected to input `port`.
    pub(crate) fn has_input(&self, port: u8) -> bool {
        self.graph.source_of(self.id, port).is_some()
    }

    /// Returns a register containing constant zero, reusing an existing zero register if available.
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

    /// Allocates the next register index.
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

    /// Emits an operation that runs after all standard operations in the sub-block (e.g. delay writes).
    pub(crate) fn emit_deferred(&mut self, op: Op) {
        self.deferred.push(op);
    }

    /// Binds output `port` of the current node to `reg`.
    pub(crate) fn bind_output(&mut self, port: u8, reg: Reg) {
        self.reg_of.push(((self.id, port), reg));
    }

    // --- the scarce, numbered things --------------------------------------

    /// Allocates an LFO state index for preserving oscillator phase across program updates.
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

    /// Allocates a latch state index for preserving switch position across program updates.
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

    /// Maps register `reg` to drive plugin parameter `target`.
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

    /// Returns the upstream note gate condition register feeding input `port`, if present.
    ///
    /// Returning a register allows a gate node to fold upstream gates into its own
    /// condition, rather than forcing the audio thread to check a list of gates.
    pub(crate) fn upstream_note_gate(&self, port: u8) -> Option<Reg> {
        let (_, socket, _) = trace_notes(self.graph, self.id, port);
        let socket = socket?;
        self.note_gates
            .iter()
            .find(|&&(key, _)| key == socket)
            .map(|&(_, reg)| reg)
    }

    /// Binds output `port` to be gated by `reg`, allocating an audio control lane.
    pub(crate) fn bind_note_gate(&mut self, port: u8, reg: Reg) -> Result<(), CompileError> {
        self.note_gates.push(((self.id, port), reg));
        self.drive_audio(OUTPUT_SOCKET + port, reg)
    }

    /// Allocates or retrieves an audio control lane driven by `reg` for socket `port`.
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

/// One node's audio output metadata.
struct Produced {
    node: NodeId,
    /// Output socket index on the node.
    port: u8,
    buf: Buf,
    /// Accumulated signal processing latency in samples.
    latency: u32,
}

/// Audio buffer pool and reuse allocator.
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

    /// Allocates an audio buffer, avoiding any buffer in `avoid`.
    ///
    /// Plugins and `Mix` nodes must avoid certain buffers to prevent memory aliasing
    /// and corruption. A plugin reads its input and writes its output; since a host
    /// cannot safely assume a plugin supports in-place processing, we never alias
    /// its input to its output. A `Mix` node accumulates into its first input, which
    /// would corrupt the data for other readers if they shared the same buffer.
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

/// Context provided to nodes during compilation of audio processing operations.
pub(crate) struct AudioCx<'a> {
    graph: &'a Graph,
    lines: &'a [Line],
    lanes: &'a [((NodeId, u8), u16)],
    id: NodeId,

    /// Audio input source buffers and latencies in port order. Unconnected inputs are `None` (silence).
    sources: Vec<Option<(Buf, u32)>>,
    /// Total number of connections leaving this node across all output ports.
    readers: usize,
    /// Channel width of this node's primary audio output port.
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
        lanes: &'a [((NodeId, u8), u16)],
    ) -> AudioCx<'a> {
        AudioCx {
            graph,
            lines,
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

    /// Begins compilation for `id`, determining its input source connections and output port configuration.
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
        self.readers = self.graph.links.iter().filter(|l| l.from == id).count();
        self.out_width = match kind.output_ports().first().map(|p| p.ty) {
            Some(PortType::Audio { channels }) => channels,
            _ => 0,
        };
    }

    pub(crate) fn finish(mut self) -> Audio {
        self.ops.append(&mut self.deferred);

        // If an audio delay loop is present, audio must run at sub-block granularity.
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

    /// Returns the buffer and accumulated latency for audio input `index`.
    pub(crate) fn source(&self, index: usize) -> Option<(Buf, u32)> {
        self.sources.get(index).copied().flatten()
    }

    /// Every audio input in socket order, wired or not.
    pub(crate) fn sources(&self) -> &[Option<(Buf, u32)>] {
        &self.sources
    }

    /// How many links leave this node in total.
    pub(crate) fn readers(&self) -> usize {
        self.readers
    }

    /// How many links leave one particular output socket.
    pub(crate) fn readers_of(&self, port: u8) -> usize {
        self.graph
            .links
            .iter()
            .filter(|l| l.from == self.id && l.from_port == port)
            .count()
    }

    /// One past the highest output socket anything reads, or 0 if nothing does.
    pub(crate) fn outputs_read(&self) -> usize {
        self.graph
            .links
            .iter()
            .filter(|l| l.from == self.id)
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

    /// Determines the note routing configuration for note input `port`.
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

    /// Emits an audio operation deferred to the end of the chunk (e.g. delay writes).
    pub(crate) fn emit_deferred(&mut self, op: AudioOp) {
        self.deferred.push(op);
    }

    /// Registers an audio output buffer `buf` with accumulated latency for `port`.
    pub(crate) fn produce(&mut self, port: u8, buf: Buf, latency: u32) {
        self.produced.push(Produced {
            node: self.id,
            port,
            buf,
            latency,
        });
    }

    /// Records total path latency to the host audio output.
    pub(crate) fn report_latency(&mut self, latency: u32) {
        self.latency = self.latency.max(latency);
    }

    /// Inserts a fixed delay compensation operation on `buf` for `samples`.
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

    /// Declares host audio bus configuration for a plugin instance.
    pub(crate) fn declare_instance(&mut self, io: InstanceIo) {
        self.instances.push(io);
    }

    // --- delay lines ------------------------------------------------------

    /// Assigns or returns the audio delay line index for `line`.
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

    /// Sets the required capacity in seconds for audio delay line `index`.
    pub(crate) fn want_ring(&mut self, index: u16, seconds: f64) {
        let slot = &mut self.ring_seconds[index as usize];
        *slot = slot.max(seconds.clamp(0.0, MAX_AUDIO_DELAY_SECONDS));
    }
}

// ---------------------------------------------------------------------------

/// Pre-compilation context used during the initial pass to collect delay lines and graph metadata.
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

    /// Declares a node's participation in delay line `line` with data type `ty`.
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
