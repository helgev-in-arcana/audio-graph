//! The audio half of compilation (§14.6, §14.7, §14.9).
//!
//! Separate from the param half because the two run at different rates and
//! share nothing but the topological order they were both derived from. This
//! pass does three things the param half never has to: it hands out buffers
//! rather than registers (a buffer is expensive enough to be worth reusing), it
//! lines up paths of unequal latency, and it decides how often the whole thing
//! runs.

use crate::graph::{Graph, NodeId, NodeKind, PortType};
use crate::program::{AudioOp, Buf, Chunking, MAX_COMPENSATION, MAX_COMPENSATORS};

use crate::compile::{CompileError, Line, NO_WRITER};

/// Ceiling on the buffer pool, so `activate` can size it once and never grow.
pub const MAX_BUFFERS: usize = 64;

/// The audio half of a `Program`.
pub(crate) struct Audio {
    pub ops: Vec<AudioOp>,
    pub buffers: Vec<u16>,
    pub chunking: Chunking,
    pub latency: u32,
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

    /// One of `buf`'s readers has run.
    fn consume(&mut self, buf: Buf) {
        let slot = &mut self.pending[buf as usize];
        *slot = slot.saturating_sub(1);
    }
}

pub(crate) fn compile_audio(
    graph: &Graph,
    order: &[NodeId],
    lines: &[Line],
) -> Result<Audio, CompileError> {
    let mut ops: Vec<AudioOp> = Vec::new();
    let mut pool = Pool::new();
    let mut produced: Vec<Produced> = Vec::new();
    let mut latency = 0u32;
    let mut compensators = 0u16;

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
                let (input, in_latency) = match sources.first().copied().flatten() {
                    Some(found) => found,
                    None => {
                        // A plugin expects an input buffer even when nothing
                        // feeds it.
                        let silent = pool.alloc(out_width, 1)?;
                        ops.push(AudioOp::Silence { out: silent });
                        (silent, 0)
                    }
                };
                pool.consume(input);
                let output = pool.alloc_avoiding(out_width, readers, &[input])?;
                ops.push(AudioOp::Plugin {
                    instance: *instance as u32,
                    input,
                    output,
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
            _ => {}
        }
    }

    // §14.9. An audio line with both halves present closes a loop, and then
    // every plugin in the program has to run at sub-block granularity.
    let looped = lines
        .iter()
        .any(|line| matches!(line.ty, PortType::Audio { .. }) && line.writer != NO_WRITER);

    Ok(Audio {
        ops,
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
    use super::*;
    use crate::compile::compile;
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
}
