//! The audio half of compilation (§14.6, §14.7, §14.9).
//!
//! Separate from the param half because the two run at different rates and
//! share nothing but the topological order they were both derived from. This
//! pass does three things the param half never has to: it hands out buffers
//! rather than registers (a buffer is expensive enough to be worth reusing), it
//! lines up paths of unequal latency, and it decides how often the whole thing
//! runs.

use crate::graph::{Graph, LineId, NodeId, NodeKind, PortType};
use crate::program::{
    AudioOp, Buf, Chunking, InstanceIo, MAX_AUDIO_DELAY_LINES, MAX_AUX_BUSES, MAX_COMPENSATION,
    MAX_COMPENSATORS, NoteSource,
};

use crate::compile::{CompileError, Line, NO_WRITER};

/// Ceiling on the buffer pool, so `activate` can size it once and never grow.
pub const MAX_BUFFERS: usize = 64;

/// The audio half of a `Program`.
pub(crate) struct Audio {
    pub ops: Vec<AudioOp>,
    /// Audio line index → its `DelayWrite` node, so a program swap can carry
    /// the ring contents over (§14.5).
    pub delay_nodes: Vec<NodeId>,
    pub buffers: Vec<u16>,
    pub chunking: Chunking,
    pub latency: u32,
    pub instances: Vec<InstanceIo>,
}

/// One node's audio output, once it has been emitted.
struct Produced {
    node: NodeId,
    buf: Buf,
    /// Samples of delay accumulated on the way here. Two of these arriving at
    /// one `Mix` with different values is what §14.6 exists to fix.
    latency: u32,
}

/// Hands out audio buffers and takes them back (§14.7).
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
    /// Two callers need this and for different reasons. A plugin reads its
    /// input and writes its output, and whether those may be the same memory is
    /// a question about the plugin's internals that a host has no way to ask,
    /// so it is never asked. A `Mix` may accumulate into its first input, but
    /// the moment it does, that buffer stops holding what the *other* inputs
    /// expect to be summed with — so all but the first are off limits.
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

    /// One of `buf`'s readers has run.
    fn consume(&mut self, buf: Buf) {
        let slot = &mut self.pending[buf as usize];
        *slot = slot.saturating_sub(1);
    }
}

/// The audio index of `line`, assigning one if this is its first mention.
///
/// `delay_nodes` grows alongside, so index `i` always names the writer of the
/// line at `audio_lines[i]` — that pairing is what lets a program swap keep the
/// ring contents (§14.5).
fn audio_line(
    audio_lines: &mut Vec<LineId>,
    delay_nodes: &mut Vec<NodeId>,
    lines: &[Line],
    line: LineId,
) -> Result<u16, CompileError> {
    if let Some(index) = audio_lines.iter().position(|&l| l == line) {
        return Ok(index as u16);
    }
    if audio_lines.len() >= MAX_AUDIO_DELAY_LINES {
        return Err(CompileError::TooLarge {
            what: "audio delay lines",
            limit: MAX_AUDIO_DELAY_LINES,
        });
    }
    audio_lines.push(line);
    delay_nodes.push(
        lines
            .iter()
            .find(|l| l.id == line)
            .map(|l| l.writer)
            .unwrap_or(NO_WRITER),
    );
    Ok((audio_lines.len() - 1) as u16)
}

/// Which note stream a plugin node is wired to (§14.10).
///
/// `None` when nothing is connected, which is the answer that makes an
/// unwired instrument silent rather than making it play whatever the DAW
/// happened to send. Only `NoteIn` produces notes today; a plugin's own note
/// output would need the engine to carry event buffers, and that is M9.
fn note_source(graph: &Graph, id: NodeId, ports: &crate::graph::PluginPorts) -> NoteSource {
    if !ports.accepts_notes {
        return NoteSource::None;
    }
    // The notes port sits after the audio inputs and before the parameters —
    // see `plugin_input_ports`, which is the one place that order is decided.
    let port = ports.audio_in.len() as u8;
    match graph.source_of(id, port) {
        Some((from, _)) if matches!(graph.node(from).map(|n| &n.kind), Some(NodeKind::NoteIn)) => {
            NoteSource::Daw { bus: 0 }
        }
        _ => NoteSource::None,
    }
}

pub(crate) fn compile_audio(
    graph: &Graph,
    order: &[NodeId],
    lines: &[Line],
    delay_lanes: &[(NodeId, u16)],
) -> Result<Audio, CompileError> {
    let mut ops: Vec<AudioOp> = Vec::new();
    let mut pool = Pool::new();
    let mut produced: Vec<Produced> = Vec::new();
    let mut latency = 0u32;
    let mut compensators = 0u16;
    let mut instances: Vec<InstanceIo> = Vec::new();
    // Audio lines are numbered among themselves: their rings are a scarcer
    // resource than a param line's, so they get their own ceiling and their own
    // index space.
    let mut audio_lines: Vec<LineId> = Vec::new();
    let mut delay_nodes: Vec<NodeId> = Vec::new();
    // Held back and appended last, for the reason the param half holds its
    // writes back (`compile`): within one chunk every read must see the line as
    // it stood before this chunk was written, or a delay of exactly one chunk
    // would read back what it had just written.
    let mut writes: Vec<AudioOp> = Vec::new();

    for &id in order {
        let node = graph.node(id).expect("ordering only contains real nodes");

        // Where each audio input's signal came from, in port order. `None` for
        // an input nobody wired, which is silence rather than an error — the
        // same rule the param half uses.
        let sources: Vec<Option<(Buf, u32)>> = node
            .kind
            .input_ports()
            .iter()
            .enumerate()
            .filter(|(_, p)| matches!(p.ty, PortType::Audio { .. }))
            .map(|(port, _)| {
                graph
                    .source_of(id, port as u8)
                    .and_then(|(from, _)| produced.iter().find(|p| p.node == from))
                    .map(|p| (p.buf, p.latency))
            })
            .collect();

        let readers = graph.links.iter().filter(|l| l.from == id).count();
        let out_width = match node.kind.output_ports().first().map(|p| p.ty) {
            Some(PortType::Audio { channels }) => channels,
            _ => 0,
        };

        match &node.kind {
            NodeKind::AudioIn { bus, channels } => {
                let out = pool.alloc(*channels, readers)?;
                ops.push(AudioOp::Input {
                    out,
                    bus: *bus as u16,
                });
                produced.push(Produced {
                    node: id,
                    buf: out,
                    latency: 0,
                });
            }
            NodeKind::AudioOut { bus, .. } => {
                if let Some((buf, late)) = sources[0] {
                    latency = latency.max(late);
                    pool.consume(buf);
                    ops.push(AudioOp::Output {
                        a: buf,
                        bus: *bus as u16,
                    });
                }
            }
            NodeKind::Plugin { instance, ports } => {
                if out_width == 0 {
                    // A plugin with no output bus cannot be routed through. It
                    // is still legal to place — an analyser is one — but there
                    // is nothing downstream of it to compile.
                    continue;
                }

                // Which input buses the graph actually feeds (§14.11). A
                // sidechain nobody wired is left off entirely rather than
                // activated and fed silence: a compressor with an active,
                // silent sidechain ducks to nothing.
                let mut wired = ports.audio_in.len();
                while wired > 1 && sources.get(wired - 1).copied().flatten().is_none() {
                    wired -= 1;
                }
                if wired > 1 + MAX_AUX_BUSES {
                    return Err(CompileError::TooLarge {
                        what: "aux input buses on one plugin",
                        limit: 1 + MAX_AUX_BUSES,
                    });
                }
                let buses: Vec<u16> = ports.audio_in[..wired].to_vec();

                // One buffer per bus, at the width the plugin wants. An unwired
                // bus before a wired one still needs something to read.
                let mut in_latency = 0u32;
                let mut parts: Vec<(Buf, u16)> = Vec::with_capacity(buses.len());
                for (index, &width) in buses.iter().enumerate() {
                    match sources.get(index).copied().flatten() {
                        Some((buf, late)) => {
                            in_latency = in_latency.max(late);
                            parts.push((buf, width));
                        }
                        None => {
                            let silent = pool.alloc(width, 1)?;
                            ops.push(AudioOp::Silence { out: silent });
                            parts.push((silent, width));
                        }
                    }
                }

                // One bus at the right width already is the plugin's input
                // region; anything else has to be assembled. Skipping the copy
                // in the common case matters — most plugins are one stereo bus.
                let total: u16 = buses.iter().sum();
                let input = match parts.as_slice() {
                    [] => {
                        // An instrument. It is still handed a buffer, because
                        // the caller's slice has to point somewhere.
                        let silent = pool.alloc(out_width, 1)?;
                        ops.push(AudioOp::Silence { out: silent });
                        silent
                    }
                    [(buf, width)] if pool.width_of(*buf) == *width => *buf,
                    _ => {
                        let avoid: Vec<Buf> = parts.iter().map(|&(b, _)| b).collect();
                        let out = pool.alloc_avoiding(total, 1, &avoid)?;
                        ops.push(AudioOp::Gather {
                            out,
                            buses: parts.clone(),
                        });
                        out
                    }
                };
                for (buf, _) in &parts {
                    pool.consume(*buf);
                }
                if !parts.iter().any(|&(b, _)| b == input) {
                    pool.consume(input);
                }

                let output = pool.alloc_avoiding(out_width, readers, &[input])?;
                ops.push(AudioOp::Plugin {
                    instance: *instance as u32,
                    input,
                    input_buses: buses.clone(),
                    output,
                    notes: note_source(graph, id, ports),
                });
                instances.push(InstanceIo {
                    instance: *instance as u32,
                    input_channels: buses.first().copied().unwrap_or(0),
                    aux_inputs: buses.get(1..).unwrap_or(&[]).to_vec(),
                    output_channels: out_width,
                });
                produced.push(Produced {
                    node: id,
                    buf: output,
                    latency: in_latency + ports.latency,
                });
            }
            NodeKind::Mix { channels, .. } => {
                // §14.6, the merge point: every branch waits for the latest one
                // or they phase-cancel.
                let arrive = sources
                    .iter()
                    .filter_map(|s| s.map(|(_, late)| late))
                    .max()
                    .unwrap_or(0);
                let mut inputs = Vec::new();
                for (buf, late) in sources.iter().flatten().copied() {
                    if arrive > late {
                        if compensators as usize >= MAX_COMPENSATORS {
                            return Err(CompileError::TooLarge {
                                what: "compensated paths",
                                limit: MAX_COMPENSATORS,
                            });
                        }
                        if (arrive - late) as usize >= MAX_COMPENSATION {
                            return Err(CompileError::TooLarge {
                                what: "samples of delay compensation",
                                limit: MAX_COMPENSATION,
                            });
                        }
                        ops.push(AudioOp::Compensate {
                            buf,
                            slot: compensators,
                            samples: arrive - late,
                        });
                        compensators += 1;
                    }
                    inputs.push(buf);
                }
                for &buf in &inputs {
                    pool.consume(buf);
                }
                // The first input may be reused as the destination — that is
                // what makes the mix an accumulate rather than a copy — but the
                // rest may not, or the sum would be built out of a buffer that
                // has already been written over.
                let out =
                    pool.alloc_avoiding(*channels, readers, inputs.get(1..).unwrap_or(&[]))?;
                ops.push(AudioOp::Mix { out, inputs });
                produced.push(Produced {
                    node: id,
                    buf: out,
                    latency: arrive,
                });
            }
            NodeKind::DelayRead {
                line,
                ty: PortType::Audio { channels },
                max_time,
                time,
            } => {
                let index = audio_line(&mut audio_lines, &mut delay_nodes, lines, *line)?;
                let out = pool.alloc(*channels, readers)?;
                ops.push(AudioOp::DelayRead {
                    out,
                    line: index,
                    lane: delay_lanes.iter().find(|&&(n, _)| n == id).map(|&(_, l)| l),
                    time: time.max(0.0),
                    max_time: max_time.max(0.0),
                });
                produced.push(Produced {
                    node: id,
                    buf: out,
                    // A line is a cut, not an edge: what comes out of it did not
                    // travel here through the paths §14.6 is lining up, so it
                    // arrives with no latency of its own to compensate for.
                    latency: 0,
                });
            }
            NodeKind::DelayWrite {
                line,
                ty: PortType::Audio { .. },
            } => {
                let index = audio_line(&mut audio_lines, &mut delay_nodes, lines, *line)?;
                if let Some((buf, _)) = sources[0] {
                    pool.consume(buf);
                    writes.push(AudioOp::DelayWrite {
                        line: index,
                        a: buf,
                    });
                }
            }
            _ => {}
        }
    }

    ops.append(&mut writes);

    // §14.9. An audio line with both halves present closes a loop, and then
    // every plugin in the program has to run at sub-block granularity.
    let looped = lines
        .iter()
        .any(|line| matches!(line.ty, PortType::Audio { .. }) && line.writer != NO_WRITER);

    instances.sort_unstable_by_key(|i| i.instance);
    Ok(Audio {
        instances,
        ops,
        delay_nodes,
        buffers: pool.widths,
        chunking: if looped {
            Chunking::SubBlock
        } else {
            Chunking::WholeBlock
        },
        latency,
    })
}

#[cfg(test)]
mod tests {

    /// A context for a test that only cares about the frame count: no
    /// automation, and a quantum big enough that a whole-block program stays
    /// one chunk.
    fn ctx(frames: u32) -> AudioContext<'static> {
        AudioContext {
            frames,
            quantum: 32,
            sample_rate: 48_000.0,
            lanes: &[],
            lanes_per_row: 0,
        }
    }
    use super::*;
    use crate::compile::compile;
    use crate::engine::{AudioChunk, AudioContext, AudioNodes};
    use crate::graph::PluginPorts;
    use crate::program::AudioOp;

    const SLOTS: usize = 32;

    fn plugin(graph: &mut Graph, instance: usize, latency: u32) -> NodeId {
        graph.add(
            NodeKind::Plugin {
                instance,
                ports: PluginPorts {
                    audio_in: vec![2],
                    audio_out: vec![2],
                    latency,
                    ..PluginPorts::default()
                },
            },
            [0.0, 0.0],
        )
    }

    /// A plugin with a main stereo bus and one aux bus of `aux` channels.
    fn with_sidechain(graph: &mut Graph, instance: usize, aux: u16) -> NodeId {
        graph.add(
            NodeKind::Plugin {
                instance,
                ports: PluginPorts {
                    audio_in: vec![2, aux],
                    audio_out: vec![2],
                    ..PluginPorts::default()
                },
            },
            [0.0, 0.0],
        )
    }

    fn stereo_in(graph: &mut Graph) -> NodeId {
        graph.add(
            NodeKind::AudioIn {
                bus: 0,
                channels: 2,
            },
            [0.0, 0.0],
        )
    }

    fn stereo_out(graph: &mut Graph) -> NodeId {
        graph.add(
            NodeKind::AudioOut {
                bus: 0,
                channels: 2,
            },
            [0.0, 0.0],
        )
    }

    #[test]
    fn two_plugins_in_series_run_in_order() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let first = plugin(&mut graph, 0, 0);
        let second = plugin(&mut graph, 1, 0);
        let output = stereo_out(&mut graph);
        graph.connect(input, 0, first, 0);
        graph.connect(first, 0, second, 0);
        graph.connect(second, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let order: Vec<u32> = program
            .audio_ops
            .iter()
            .filter_map(|op| match op {
                AudioOp::Plugin { instance, .. } => Some(*instance),
                _ => None,
            })
            .collect();
        assert_eq!(order, vec![0, 1]);
    }

    /// §14.7. Nothing reads the first plugin's output once the second has run,
    /// so the third one may have it back.
    #[test]
    fn a_buffer_comes_back_once_nothing_reads_it() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let mut last = input;
        for i in 0..6 {
            let node = plugin(&mut graph, i, 0);
            graph.connect(last, 0, node, 0);
            last = node;
        }
        graph.connect(last, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        assert!(
            program.buffers.len() <= 3,
            "a chain of six wants two or three buffers, not {}: {:?}",
            program.buffers.len(),
            program.buffers
        );
    }

    /// A plugin must never be handed the same buffer to read and to write.
    #[test]
    fn a_plugin_never_reads_and_writes_one_buffer() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let mut last = input;
        for i in 0..4 {
            let node = plugin(&mut graph, i, 0);
            graph.connect(last, 0, node, 0);
            last = node;
        }
        graph.connect(last, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        for op in &program.audio_ops {
            if let AudioOp::Plugin { input, output, .. } = op {
                assert_ne!(input, output, "{op:?}");
            }
        }
    }

    /// §14.6. One branch goes through a plugin with latency, the other does
    /// not; the short branch has to wait or the two phase-cancel at the mix.
    #[test]
    fn parallel_paths_of_unequal_latency_are_lined_up() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let slow = plugin(&mut graph, 0, 128);
        let mix = graph.add(
            NodeKind::Mix {
                channels: 2,
                inputs: 2,
            },
            [0.0, 0.0],
        );
        let output = stereo_out(&mut graph);
        graph.connect(input, 0, slow, 0);
        graph.connect(slow, 0, mix, 0);
        // The dry branch, straight from the input.
        graph.connect(input, 0, mix, 1);
        graph.connect(mix, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let compensations: Vec<u32> = program
            .audio_ops
            .iter()
            .filter_map(|op| match op {
                AudioOp::Compensate { samples, .. } => Some(*samples),
                _ => None,
            })
            .collect();
        assert_eq!(compensations, vec![128], "the dry branch waits 128 samples");
        assert_eq!(program.latency, 128, "and the wrapper tells the DAW so");
    }

    #[test]
    fn equal_paths_need_no_compensation() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let a = plugin(&mut graph, 0, 64);
        let b = plugin(&mut graph, 1, 64);
        let mix = graph.add(
            NodeKind::Mix {
                channels: 2,
                inputs: 2,
            },
            [0.0, 0.0],
        );
        let output = stereo_out(&mut graph);
        graph.connect(input, 0, a, 0);
        graph.connect(input, 0, b, 0);
        graph.connect(a, 0, mix, 0);
        graph.connect(b, 0, mix, 1);
        graph.connect(mix, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        assert!(
            !program
                .audio_ops
                .iter()
                .any(|op| matches!(op, AudioOp::Compensate { .. }))
        );
        assert_eq!(program.latency, 64);
    }

    /// §14.9. A graph with no audio loop is not made to pay for one.
    #[test]
    fn only_an_audio_loop_forces_the_fine_grain() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let node = plugin(&mut graph, 0, 0);
        let output = stereo_out(&mut graph);
        graph.connect(input, 0, node, 0);
        graph.connect(node, 0, output, 0);
        assert_eq!(
            compile(&graph, SLOTS).unwrap().chunking,
            Chunking::WholeBlock
        );

        // Feed the plugin from its own output, through a delay line.
        let read = graph.add(
            NodeKind::DelayRead {
                line: 0,
                ty: PortType::STEREO,
                max_time: 1.0,
                time: 0.01,
            },
            [0.0, 0.0],
        );
        let write = graph.add(
            NodeKind::DelayWrite {
                line: 0,
                ty: PortType::STEREO,
            },
            [0.0, 0.0],
        );
        let mix = graph.add(
            NodeKind::Mix {
                channels: 2,
                inputs: 2,
            },
            [0.0, 0.0],
        );
        graph.connect(input, 0, mix, 0);
        graph.connect(read, 0, mix, 1);
        graph.connect(mix, 0, node, 0);
        graph.connect(node, 0, write, 0);

        assert_eq!(compile(&graph, SLOTS).unwrap().chunking, Chunking::SubBlock);
    }

    /// A param feedback loop is not an audio loop, and must not drag the audio
    /// half down to sub-block granularity with it.
    #[test]
    fn a_param_loop_leaves_the_audio_grain_alone() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let node = plugin(&mut graph, 0, 0);
        let output = stereo_out(&mut graph);
        graph.connect(input, 0, node, 0);
        graph.connect(node, 0, output, 0);

        let read = graph.add(
            NodeKind::DelayRead {
                line: 0,
                ty: PortType::Param,
                max_time: 1.0,
                time: 0.01,
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
        graph.connect(read, 0, write, 0);

        assert_eq!(
            compile(&graph, SLOTS).unwrap().chunking,
            Chunking::WholeBlock
        );
    }

    /// A synth node with an instrument's ports.
    fn synth(graph: &mut Graph, instance: usize) -> NodeId {
        graph.add(
            NodeKind::Plugin {
                instance,
                ports: PluginPorts {
                    audio_in: vec![],
                    audio_out: vec![2],
                    accepts_notes: true,
                    ..PluginPorts::default()
                },
            },
            [0.0, 0.0],
        )
    }

    fn note_sources(program: &crate::program::Program) -> Vec<(u32, NoteSource)> {
        program
            .audio_ops
            .iter()
            .filter_map(|op| match op {
                AudioOp::Plugin {
                    instance, notes, ..
                } => Some((*instance, *notes)),
                _ => None,
            })
            .collect()
    }

    /// §14.14. An analyser is fed audio and its output goes nowhere, which is
    /// exactly the shape the compiler otherwise deletes.
    #[test]
    fn an_always_on_plugin_still_gets_its_input() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let analyser = plugin(&mut graph, 0, 0);
        graph.connect(input, 0, analyser, 0);

        assert!(
            compile(&graph, SLOTS).unwrap().audio_ops.is_empty(),
            "nothing reads it, so nothing runs"
        );

        graph.node_mut(analyser).unwrap().always_on = true;
        let program = compile(&graph, SLOTS).unwrap();
        let feeds_it = program.audio_ops.iter().any(|op| {
            matches!(op, AudioOp::Plugin { instance: 0, input, .. }
                if program.audio_ops.iter().any(|w| matches!(w, AudioOp::Input { out, .. } if out == input)))
        });
        assert!(
            feeds_it,
            "the DAW's input reaches it: {:?}",
            program.audio_ops
        );
        assert_eq!(
            program.instances.len(),
            1,
            "and its buses are activated for it"
        );
    }

    /// §14.10, the DoD: notes reach the instrument the graph points at.
    #[test]
    fn a_wired_instrument_hears_the_daw() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let synth = synth(&mut graph, 0);
        let output = stereo_out(&mut graph);
        // Port 0 is the notes port: this plugin has no audio inputs.
        graph.connect(notes, 0, synth, 0);
        graph.connect(synth, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        assert_eq!(
            note_sources(&program),
            vec![(0, NoteSource::Daw { bus: 0 })]
        );
    }

    /// The bug M8.3 exists to fix: before this, every instance was handed every
    /// event the DAW sent, so a second synth played along whatever the graph
    /// said. An unwired notes port has to mean silence.
    #[test]
    fn an_unwired_instrument_hears_nothing() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let wired = synth(&mut graph, 0);
        let idle = synth(&mut graph, 1);
        let mix = graph.add(
            NodeKind::Mix {
                channels: 2,
                inputs: 2,
            },
            [0.0, 0.0],
        );
        let output = stereo_out(&mut graph);
        graph.connect(notes, 0, wired, 0);
        graph.connect(wired, 0, mix, 0);
        graph.connect(idle, 0, mix, 1);
        graph.connect(mix, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let mut sources = note_sources(&program);
        sources.sort_by_key(|&(i, _)| i);
        assert_eq!(
            sources,
            vec![(0, NoteSource::Daw { bus: 0 }), (1, NoteSource::None)]
        );
    }

    /// The notes port sits after the audio inputs, so an effect that also takes
    /// notes must not read its sidechain link as a note link.
    #[test]
    fn an_effect_that_takes_notes_finds_its_notes_port() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let node = graph.add(
            NodeKind::Plugin {
                instance: 0,
                ports: PluginPorts {
                    audio_in: vec![2],
                    audio_out: vec![2],
                    accepts_notes: true,
                    ..PluginPorts::default()
                },
            },
            [0.0, 0.0],
        );
        let output = stereo_out(&mut graph);
        graph.connect(input, 0, node, 0);
        // Port 1 is the notes port: port 0 is the audio input.
        graph.connect(notes, 0, node, 1);
        graph.connect(node, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        assert_eq!(
            note_sources(&program),
            vec![(0, NoteSource::Daw { bus: 0 })]
        );
    }

    /// A plugin that does not take notes has no notes port, and nothing may be
    /// wired to it — so it stays `None` whatever the user does.
    #[test]
    fn a_plugin_that_takes_no_notes_is_never_given_any() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let node = plugin(&mut graph, 0, 0);
        let output = stereo_out(&mut graph);
        graph.connect(input, 0, node, 0);
        graph.connect(node, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        assert_eq!(note_sources(&program), vec![(0, NoteSource::None)]);
    }

    /// Instrument -> effect -> effect: the DoD's chain. Only the instrument
    /// hears the notes, and the effects run after it in order.
    #[test]
    fn an_instrument_into_two_effects_routes_notes_only_to_the_instrument() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let synth = synth(&mut graph, 0);
        let first = plugin(&mut graph, 1, 0);
        let second = plugin(&mut graph, 2, 0);
        let output = stereo_out(&mut graph);
        graph.connect(notes, 0, synth, 0);
        graph.connect(synth, 0, first, 0);
        graph.connect(first, 0, second, 0);
        graph.connect(second, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        assert_eq!(
            note_sources(&program),
            vec![
                (0, NoteSource::Daw { bus: 0 }),
                (1, NoteSource::None),
                (2, NoteSource::None)
            ],
            "the order is the order they run in, and only the synth hears notes"
        );
    }

    /// A plugin with a sidechain socket nobody wired is activated with one bus.
    ///
    /// Not "activated with a silent sidechain": a compressor whose sidechain is
    /// switched on and fed nothing ducks to silence (§14.11).
    #[test]
    fn an_unwired_sidechain_is_not_switched_on() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let node = with_sidechain(&mut graph, 0, 1);
        let output = stereo_out(&mut graph);
        graph.connect(input, 0, node, 0);
        graph.connect(node, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        assert_eq!(program.instances[0].aux_inputs, Vec::<u16>::new());
        assert!(
            !program
                .audio_ops
                .iter()
                .any(|op| matches!(op, AudioOp::Gather { .. })),
            "one bus at the right width needs no assembling"
        );
    }

    /// Wiring the sidechain switches the bus on and assembles the input region.
    #[test]
    fn a_wired_sidechain_is_gathered_behind_the_main_bus() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let key = stereo_in(&mut graph);
        let node = with_sidechain(&mut graph, 0, 1);
        let output = stereo_out(&mut graph);
        graph.connect(input, 0, node, 0);
        graph.connect(key, 0, node, 1);
        graph.connect(node, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        assert_eq!(program.instances[0].input_channels, 2);
        assert_eq!(program.instances[0].aux_inputs, vec![1]);

        let gather = program
            .audio_ops
            .iter()
            .find_map(|op| match op {
                AudioOp::Gather { buses, .. } => Some(buses.clone()),
                _ => None,
            })
            .expect("the two buses have to be assembled into one region");
        assert_eq!(gather.len(), 2);
        assert_eq!(gather[0].1, 2, "main bus stays stereo");
        assert_eq!(
            gather[1].1, 1,
            "the sidechain is the width the plugin wants"
        );

        let buses = program
            .audio_ops
            .iter()
            .find_map(|op| match op {
                AudioOp::Plugin { input_buses, .. } => Some(input_buses.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(buses, vec![2, 1]);
    }

    /// A stereo source into a mono sidechain is summed, not halved and not
    /// left-only: a detector that ignored one channel would miss half the
    /// signal it is supposed to react to.
    #[test]
    fn a_stereo_source_reaches_a_mono_sidechain_as_a_sum() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let key = stereo_in(&mut graph);
        let node = with_sidechain(&mut graph, 0, 1);
        let output = stereo_out(&mut graph);
        graph.connect(input, 0, node, 0);
        graph.connect(key, 0, node, 1);
        graph.connect(node, 0, output, 0);

        let mut engine = crate::Engine::new();
        engine.prepare(8, &[2]);
        let handoff = crate::Handoff::new();
        handoff.send(Box::new(compile(&graph, SLOTS).unwrap()));
        assert!(engine.adopt(&handoff));

        // Both stereo inputs read DAW bus 0, so the sidechain sees the same
        // two channels: 1.0 and 2.0, which have to arrive as 3.0.
        let daw_in = [1.0f32, 1.0, 1.0, 1.0, 2.0, 2.0, 2.0, 2.0];
        let mut daw_out = [0.0f32; 8];
        let mut seen = RecordInput::default();
        engine.run_audio(&ctx(4), &daw_in, &mut daw_out, &mut seen);

        assert_eq!(seen.channels, 3, "stereo main plus mono sidechain");
        assert_eq!(seen.first_of_each, vec![1.0, 2.0, 3.0]);
    }

    /// Records the shape and content of what a plugin node was handed.
    #[derive(Default)]
    struct RecordInput {
        channels: u16,
        first_of_each: Vec<f32>,
    }

    impl AudioNodes for RecordInput {
        fn process(
            &mut self,
            _instance: u32,
            _notes: NoteSource,
            input: &[f32],
            output: &mut [f32],
            chunk: AudioChunk,
        ) {
            self.channels = chunk.input_channels;
            self.first_of_each = (0..chunk.input_channels)
                .map(|ch| input[ch as usize * chunk.frames as usize])
                .collect();
            for ch in 0..chunk.output_channels {
                output[chunk.channel(ch)].fill(0.0);
            }
        }
    }
}
