//! Audio pipeline compilation pass.
//!
//! Separate from the param half because the two run at different rates and
//! share nothing but the topological order they were both derived from. This
//! pass does three things the param half never has to: it hands out buffers
//! rather than registers (a buffer is expensive enough to be worth reusing), it
//! lines up paths of unequal latency, and it decides how often the whole thing
//! runs.

use crate::graph::{Graph, NodeId};
use crate::ir::{AudioOp, Chunking};
use subhost_adapter::InstanceIo;

use crate::compile::{AudioCx, CompileError, Line};

/// The audio half of a `Program`.
pub(crate) struct Audio {
    pub ops: Vec<AudioOp>,
    /// Audio line index → its `DelayWrite` node, so a program swap can carry
    /// the ring contents over.
    pub delay_nodes: Vec<NodeId>,
    /// Audio line index → the longest any read on it asks for, in seconds.
    /// What the main thread sizes the ring from.
    pub ring_seconds: Vec<f64>,
    pub buffers: Vec<u16>,
    pub chunking: Chunking,
    pub latency: u32,
    pub instances: Vec<InstanceIo>,
}

/// Walks the order a second time and emits the audio half.
///
/// The same order as the param half, and for the same reason: a node may only
/// read what is already in a buffer. What differs is what a node is handed —
/// see [`AudioCx`], which owns the buffer pool, the latency bookkeeping and the
/// audio line numbering.
pub(crate) fn compile_audio(
    graph: &Graph,
    order: &[NodeId],
    lines: &[Line],
    audio_lanes: &[((NodeId, u8), u16)],
) -> Result<Audio, CompileError> {
    let mut cx = AudioCx::new(graph, lines, order, audio_lanes);
    for &id in order {
        let node = graph.node(id).expect("ordering only contains real nodes");
        cx.begin(id, &node.kind);
        node.kind.compile_audio(&mut cx)?;
    }
    Ok(cx.finish())
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
    use crate::engine::AudioContext;
    use crate::ir::{AudioOp, NoteRoute};
    use crate::nodes::{
        AudioIn, AudioOut, DelayRead, DelayWrite, KeyParam, KeyParamMode, KeySwitch, KeySwitchMode,
        Mix, NodeKind, NoteGate, NoteMute, Plugin, PluginPorts, SlotIn,
    };
    use crate::port::PortType;
    use subhost_adapter::{AudioChunk, AudioInstances, NoteSource, NoteStream};

    const SLOTS: usize = 32;

    fn plugin(graph: &mut Graph, instance: usize, latency: u32) -> NodeId {
        graph.add(
            NodeKind::Plugin(Plugin {
                instance,
                ports: PluginPorts {
                    audio_in: vec![2],
                    audio_out: vec![2],
                    audio_out_shown: Vec::new(),
                    latency,
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        )
    }

    /// A plugin with a main stereo bus and one aux bus of `aux` channels.
    fn with_sidechain(graph: &mut Graph, instance: usize, aux: u16) -> NodeId {
        graph.add(
            NodeKind::Plugin(Plugin {
                instance,
                ports: PluginPorts {
                    audio_in: vec![2, aux],
                    audio_out: vec![2],
                    audio_out_shown: Vec::new(),
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        )
    }

    fn stereo_in(graph: &mut Graph) -> NodeId {
        graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        )
    }

    fn stereo_out(graph: &mut Graph) -> NodeId {
        graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
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

    /// Nothing reads the first plugin's output once the second has run, so the
    /// third one may have it back.
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

    /// A plugin's second output bus is its own signal, not a second name for
    /// the first.
    ///
    /// Real instruments make this concrete: one declares `Output`, `Scene A` and
    /// `Scene B`, and until the buses were routed all three sockets rendered the
    /// main output. What the program has to show is a plugin handed one packed
    /// output region and a `Split` per bus somebody reads.
    #[test]
    fn each_output_bus_is_split_out_of_the_plugins_output_region() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let node = graph.add(
            NodeKind::Plugin(Plugin {
                instance: 0,
                ports: PluginPorts {
                    audio_in: vec![2],
                    // Three buses, as a multi-scene instrument has.
                    audio_out: vec![2, 2, 2],
                    audio_out_shown: Vec::new(),
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, node, 0);
        graph.connect(node, 1, output, 0);

        let program = compile(&graph, SLOTS).expect("the second bus is routable");

        // Two buses are handed over, not three: nothing reads the third, so
        // the plugin is never asked to produce it.
        let buses = program.audio_ops.iter().find_map(|op| match op {
            AudioOp::Plugin { output_buses, .. } => Some(output_buses.clone()),
            _ => None,
        });
        assert_eq!(buses, Some(vec![2, 2]));
        assert_eq!(
            program.instances[0].output_channels, 2,
            "the main bus stays the main bus"
        );
        assert_eq!(program.instances[0].aux_outputs, vec![2]);

        // And the bus that is read is copied out of the region at its own
        // offset — channel 2, not channel 0.
        let split = program
            .audio_ops
            .iter()
            .find_map(|op| match op {
                AudioOp::Split { channel, width, .. } => Some((*channel, *width)),
                _ => None,
            })
            .expect("the read bus is split out");
        assert_eq!(split, (2, 2));
    }

    /// A branch that reaches no output is not compiled, so it is not a reader
    /// either, and the buffer it would have read comes straight back.
    #[test]
    fn a_pruned_branch_does_not_hold_the_buffer_it_would_have_read() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let mut last = input;
        for i in 0..6 {
            let node = plugin(&mut graph, i, 0);
            graph.connect(last, 0, node, 0);
            // Each stage also feeds a plugin that goes nowhere.
            let dangling = plugin(&mut graph, 100 + i, 0);
            graph.connect(node, 0, dangling, 0);
            last = node;
        }
        graph.connect(last, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        assert!(
            program.buffers.len() <= 3,
            "the dangling branches read nothing, so they cost no buffers: {:?}",
            program.buffers
        );
    }

    /// The same question one level down: a bus whose only reader is a pruned
    /// branch is not handed over, and nothing is copied out of it.
    #[test]
    fn a_bus_read_only_by_a_pruned_branch_is_not_handed_over() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let node = graph.add(
            NodeKind::Plugin(Plugin {
                instance: 0,
                ports: PluginPorts {
                    audio_in: vec![2],
                    audio_out: vec![2, 2],
                    audio_out_shown: Vec::new(),
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        );
        let dangling = plugin(&mut graph, 1, 0);
        graph.connect(input, 0, node, 0);
        graph.connect(node, 0, output, 0);
        graph.connect(node, 1, dangling, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let buses = program.audio_ops.iter().find_map(|op| match op {
            AudioOp::Plugin { output_buses, .. } => Some(output_buses.clone()),
            _ => None,
        });
        assert_eq!(buses, Some(vec![2]), "only the bus the output reads");
        assert!(
            !program
                .audio_ops
                .iter()
                .any(|op| matches!(op, AudioOp::Split { .. })),
            "nothing to split out: {:?}",
            program.audio_ops
        );
    }

    /// A socket carries the bus its dropdown says, not the bus it happens to
    /// sit at. One socket pointed at the third bus is the whole node.
    #[test]
    fn an_output_socket_carries_the_bus_it_was_pointed_at() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let node = graph.add(
            NodeKind::Plugin(Plugin {
                instance: 0,
                ports: PluginPorts {
                    audio_in: vec![2],
                    audio_out: vec![2, 2, 2],
                    // One socket, and it is the third bus.
                    audio_out_shown: vec![2],
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, node, 0);
        graph.connect(node, 0, output, 0);

        let program = compile(&graph, SLOTS).expect("a pointed socket is routable");

        // All three buses are handed over, because the one that is read is the
        // last of them and a plugin's buses are activated as a prefix.
        let buses = program.audio_ops.iter().find_map(|op| match op {
            AudioOp::Plugin { output_buses, .. } => Some(output_buses.clone()),
            _ => None,
        });
        assert_eq!(buses, Some(vec![2, 2, 2]));
        let split = program
            .audio_ops
            .iter()
            .find_map(|op| match op {
                AudioOp::Split { channel, width, .. } => Some((*channel, *width)),
                _ => None,
            })
            .expect("the pointed bus is split out");
        assert_eq!(split, (4, 2), "the third bus starts at channel 4");
    }

    /// The one-bus case — every patch until multi-bus routing existed — must
    /// not have grown a copy.
    #[test]
    fn one_output_bus_still_writes_straight_into_the_next_node() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let node = plugin(&mut graph, 0, 0);
        graph.connect(input, 0, node, 0);
        graph.connect(node, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        assert!(
            !program
                .audio_ops
                .iter()
                .any(|op| matches!(op, AudioOp::Split { .. })),
            "{:?}",
            program.audio_ops
        );
    }

    /// A link from a socket the node does not have. `connect` cannot make one,
    /// so this is a hand-edited or future-versioned patch.
    #[test]
    fn a_link_from_an_output_bus_the_plugin_does_not_have_is_refused() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let output = stereo_out(&mut graph);
        let node = plugin(&mut graph, 0, 0);
        graph.connect(input, 0, node, 0);
        graph.links.push(crate::graph::Link {
            from: node,
            from_port: 1,
            to: output,
            to_port: 0,
        });

        let error = compile(&graph, SLOTS).expect_err("there is no second bus");
        assert!(
            matches!(error, CompileError::TypeMismatch { .. }),
            "{error:?}"
        );
    }

    /// One branch goes through a plugin with latency, the other does not; the
    /// short branch has to wait or the two phase-cancel at the mix.
    #[test]
    fn parallel_paths_of_unequal_latency_are_lined_up() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let slow = plugin(&mut graph, 0, 128);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 2,
                gains: Vec::new(),
            }),
            [0.0, 0.0],
        );
        let output = stereo_out(&mut graph);
        graph.connect(input, 0, slow, 0);
        graph.connect(slow, 0, mix, 0);
        // The dry branch, straight from the input.
        graph.connect(input, 0, mix, 2);
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
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 2,
                gains: Vec::new(),
            }),
            [0.0, 0.0],
        );
        let output = stereo_out(&mut graph);
        graph.connect(input, 0, a, 0);
        graph.connect(input, 0, b, 0);
        graph.connect(a, 0, mix, 0);
        graph.connect(b, 0, mix, 2);
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

    /// A graph with no audio loop is not made to pay for one.
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
            NodeKind::DelayRead(DelayRead {
                line: 0,
                ty: PortType::STEREO,
                max_time: 1.0,
                time: 0.01,
            }),
            [0.0, 0.0],
        );
        let write = graph.add(
            NodeKind::DelayWrite(DelayWrite {
                line: 0,
                ty: PortType::STEREO,
            }),
            [0.0, 0.0],
        );
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 2,
                gains: Vec::new(),
            }),
            [0.0, 0.0],
        );
        graph.connect(input, 0, mix, 0);
        graph.connect(read, 0, mix, 2);
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
            NodeKind::DelayRead(DelayRead {
                line: 0,
                ty: PortType::Param,
                max_time: 1.0,
                time: 0.01,
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
        graph.connect(read, 0, write, 0);

        assert_eq!(
            compile(&graph, SLOTS).unwrap().chunking,
            Chunking::WholeBlock
        );
    }

    /// A synth node with an instrument's ports.
    fn synth(graph: &mut Graph, instance: usize) -> NodeId {
        graph.add(
            NodeKind::Plugin(Plugin {
                instance,
                ports: PluginPorts {
                    audio_in: vec![],
                    audio_out: vec![2],
                    audio_out_shown: Vec::new(),
                    accepts_notes: true,
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        )
    }

    fn note_sources(program: &crate::ir::Program) -> Vec<(u32, NoteRoute)> {
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

    /// An analyser is fed audio and its output goes nowhere, which is exactly
    /// the shape the compiler otherwise deletes.
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

    /// Notes reach the instrument the graph points at, and no other.
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
            vec![(0, NoteRoute::from_source(NoteSource::Daw { bus: 0 }))]
        );
    }

    /// An unwired notes port has to mean silence. Handing every instance every
    /// event the DAW sent makes a second synth play along whatever the graph
    /// says.
    #[test]
    fn an_unwired_instrument_hears_nothing() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let wired = synth(&mut graph, 0);
        let idle = synth(&mut graph, 1);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 2,
                gains: Vec::new(),
            }),
            [0.0, 0.0],
        );
        let output = stereo_out(&mut graph);
        graph.connect(notes, 0, wired, 0);
        graph.connect(wired, 0, mix, 0);
        graph.connect(idle, 0, mix, 2);
        graph.connect(mix, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let mut sources = note_sources(&program);
        sources.sort_by_key(|&(i, _)| i);
        assert_eq!(
            sources,
            vec![
                (0, NoteRoute::from_source(NoteSource::Daw { bus: 0 })),
                (1, NoteRoute::default())
            ]
        );
    }

    /// A gate on the way does not change *where* the notes come from — it adds
    /// a lane the audio half reads each chunk to decide whether they get
    /// through.
    #[test]
    fn a_note_gate_leaves_the_source_and_adds_a_lane() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
        let gate = graph.add(
            NodeKind::NoteGate(NoteGate {
                threshold: 0.5,
                invert: false,
            }),
            [0.0, 0.0],
        );
        let synth = synth(&mut graph, 0);
        let output = stereo_out(&mut graph);
        graph.connect(notes, 0, gate, 0);
        graph.connect(control, 0, gate, 1);
        graph.connect(gate, 0, synth, 0);
        graph.connect(synth, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let routes = note_sources(&program);
        assert_eq!(routes.len(), 1);
        let route = routes[0].1;
        assert_eq!(route.source, NoteSource::Daw { bus: 0 });
        let lane = route.gate.expect("the gate booked a lane");
        assert!(
            lane >= program.audio_lane_base,
            "a note gate's lane is an audio lane, not a parameter one"
        );
        assert!(
            program.outputs.iter().any(|&(l, _)| l == lane),
            "the parameter half drives the lane it booked"
        );
        // Shut, the stream keeps its releases so nothing hangs.
        assert_eq!(
            route.resolve(Some(0.0)),
            NoteStream::from_source(NoteSource::DawReleases { bus: 0 })
        );
        assert_eq!(
            route.resolve(Some(1.0)),
            NoteStream::from_source(NoteSource::Daw { bus: 0 })
        );
    }

    /// Two gates in series pass notes only when both are open, and they say so
    /// in one lane: the nearer gate folds the further one into its condition.
    #[test]
    fn gates_in_series_become_one_lane() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let first = graph.add(
            NodeKind::NoteGate(NoteGate {
                threshold: 0.5,
                invert: false,
            }),
            [0.0, 0.0],
        );
        let second = graph.add(
            NodeKind::NoteGate(NoteGate {
                threshold: 0.5,
                invert: false,
            }),
            [0.0, 0.0],
        );
        let synth = synth(&mut graph, 0);
        let output = stereo_out(&mut graph);
        graph.connect(notes, 0, first, 0);
        graph.connect(first, 0, second, 0);
        graph.connect(second, 0, synth, 0);
        graph.connect(synth, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let route = note_sources(&program)[0].1;
        assert_eq!(route.source, NoteSource::Daw { bus: 0 });
        assert!(route.gate.is_some());
        assert!(
            program
                .ops
                .iter()
                .any(|op| matches!(op, crate::ir::Op::Math { .. })),
            "the second gate multiplies the first one's condition into its own"
        );
    }

    /// A selecting key switch has one output per destination, all carrying one
    /// stream, and each gated by a lane of its own: whichever way the switch
    /// stands, one synth hears the notes and the other does not.
    #[test]
    fn a_selecting_key_switch_gates_each_output_of_its_own() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let switch = graph.add(
            NodeKind::KeySwitch(KeySwitch {
                keys: vec![24, 25],
                mode: KeySwitchMode::Select,
                mute_keys: true,
            }),
            [0.0, 0.0],
        );
        let first = synth(&mut graph, 0);
        let second = synth(&mut graph, 1);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 2,
                gains: Vec::new(),
            }),
            [0.0, 0.0],
        );
        let output = stereo_out(&mut graph);
        graph.connect(notes, 0, switch, 0);
        graph.connect(switch, 0, first, 0);
        graph.connect(switch, 1, second, 0);
        graph.connect(first, 0, mix, 0);
        graph.connect(second, 0, mix, 2);
        graph.connect(mix, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        assert_eq!(
            program.latch_nodes,
            vec![switch],
            "the switch keeps a latch"
        );
        let mut routes = note_sources(&program);
        routes.sort_by_key(|&(i, _)| i);
        let (a, b) = (routes[0].1, routes[1].1);
        assert_eq!(a.source, NoteSource::Daw { bus: 0 });
        assert_eq!(b.source, NoteSource::Daw { bus: 0 });
        assert_ne!(
            a.gate, b.gate,
            "the two outputs are gated by different lanes"
        );
        let expected = (1u128 << 24) | (1u128 << 25);
        assert_eq!(
            (a.mute, b.mute),
            (expected, expected),
            "switching keys are filtered from the sounding note stream"
        );
    }

    /// Clearing `mute_keys` puts the switching keys back into the stream, for
    /// the patch where the key that selects a layer is also meant to play it.
    #[test]
    fn an_unmuted_key_switch_passes_its_own_keys() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let switch = graph.add(
            NodeKind::KeySwitch(KeySwitch {
                keys: vec![24, 25],
                mode: KeySwitchMode::Select,
                mute_keys: false,
            }),
            [0.0, 0.0],
        );
        let synth = synth(&mut graph, 0);
        let output = stereo_out(&mut graph);
        graph.connect(notes, 0, switch, 0);
        graph.connect(switch, 0, synth, 0);
        graph.connect(synth, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let routes = note_sources(&program);
        assert_eq!(routes[0].1.mute, 0);
    }

    /// A key parameter hands the stream on through a notes output of its own,
    /// with the keys that pick the value taken out of it.
    #[test]
    fn a_key_parameter_passes_notes_on_without_its_own_keys() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let key = graph.add(
            NodeKind::KeyParam(KeyParam {
                mode: KeyParamMode::Select,
                keys: vec![24, 25],
                values: vec![0.0, 1.0],
                mute_keys: true,
            }),
            [0.0, 0.0],
        );
        let synth = synth(&mut graph, 0);
        let output = stereo_out(&mut graph);
        graph.connect(notes, 0, key, 0);
        graph.connect(key, 1, synth, 0);
        graph.connect(synth, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let routes = note_sources(&program);
        assert_eq!(routes[0].1.source, NoteSource::Daw { bus: 0 });
        assert_eq!(
            routes[0].1.mute,
            (1u128 << 24) | (1u128 << 25),
            "picking keys are filtered from the sounding note stream"
        );
    }

    /// Clearing `mute_keys` puts the picking keys back.
    #[test]
    fn an_unmuted_key_parameter_passes_its_own_keys() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let key = graph.add(
            NodeKind::KeyParam(KeyParam {
                mode: KeyParamMode::Select,
                keys: vec![24, 25],
                values: vec![0.0, 1.0],
                mute_keys: false,
            }),
            [0.0, 0.0],
        );
        let synth = synth(&mut graph, 0);
        let output = stereo_out(&mut graph);
        graph.connect(notes, 0, key, 0);
        graph.connect(key, 1, synth, 0);
        graph.connect(synth, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let routes = note_sources(&program);
        assert_eq!(routes[0].1.mute, 0);
    }

    /// A key mute takes its keys out and leaves everything else alone,
    /// including a gate above it.
    #[test]
    fn a_key_mute_swallows_its_keys_and_keeps_the_gate_above_it() {
        let mut graph = Graph::new();
        let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
        let gate = graph.add(
            NodeKind::NoteGate(NoteGate {
                threshold: 0.5,
                invert: false,
            }),
            [0.0, 0.0],
        );
        let mute = graph.add(
            NodeKind::NoteMute(NoteMute { keys: vec![24, 26] }),
            [0.0, 0.0],
        );
        let synth = synth(&mut graph, 0);
        let output = stereo_out(&mut graph);
        graph.connect(notes, 0, gate, 0);
        graph.connect(gate, 0, mute, 0);
        graph.connect(mute, 0, synth, 0);
        graph.connect(synth, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        let routes = note_sources(&program);
        assert_eq!(routes[0].1.source, NoteSource::Daw { bus: 0 });
        assert_eq!(routes[0].1.mute, (1u128 << 24) | (1u128 << 26));
        assert!(
            routes[0].1.gate.is_some(),
            "the gate upstream of the mute still reaches the synth"
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
            NodeKind::Plugin(Plugin {
                instance: 0,
                ports: PluginPorts {
                    audio_in: vec![2],
                    audio_out: vec![2],
                    audio_out_shown: Vec::new(),
                    accepts_notes: true,
                    ..PluginPorts::default()
                },
            }),
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
            vec![(0, NoteRoute::from_source(NoteSource::Daw { bus: 0 }))]
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
        assert_eq!(note_sources(&program), vec![(0, NoteRoute::default())]);
    }

    /// Instrument -> effect -> effect. Only the instrument hears the notes, and
    /// the effects run after it in order.
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
                (0, NoteRoute::from_source(NoteSource::Daw { bus: 0 })),
                (1, NoteRoute::default()),
                (2, NoteRoute::default())
            ],
            "the order is the order they run in, and only the synth hears notes"
        );
    }

    /// A plugin with a sidechain socket nobody wired is activated with one bus.
    ///
    /// Not "activated with a silent sidechain": a compressor whose sidechain is
    /// switched on and fed nothing ducks to silence.
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

    /// A mono plugin node feeding a stereo socket is heard on both channels.
    ///
    /// The DAW asks for stereo and gets a buffer whose right channel was never
    /// written: the sound is in the left speaker alone. What the left channel
    /// carries has to be copied across, which is what a host does with a mono
    /// track.
    #[test]
    fn a_mono_source_reaches_both_channels_of_a_stereo_output() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let node = mono_plugin(&mut graph, 0);
        let output = stereo_out(&mut graph);
        graph.connect(input, 0, node, 0);
        graph.connect(node, 0, output, 0);

        let mut engine = crate::Engine::new();
        engine.prepare(8, &[2]);
        let handoff = crate::Handoff::new();
        handoff.send(Box::new(compile(&graph, SLOTS).unwrap()));
        assert!(engine.adopt(&handoff));

        // 1.0 on the left and 2.0 on the right are summed into the plugin's
        // mono bus, so both output channels have to read 3.0.
        let daw_in = [1.0f32, 1.0, 1.0, 1.0, 2.0, 2.0, 2.0, 2.0];
        let mut daw_out = [0.0f32; 8];
        engine.run_audio(&ctx(4), &daw_in, &mut daw_out, &mut PassThrough);
        assert_eq!(daw_out, [3.0f32; 8]);
    }

    /// The same conversion on the way into a Mix, which sums channel by
    /// channel across its own width.
    ///
    /// Without it the mono input's second channel is read from whatever buffer
    /// happens to sit next to it in the pool, which is worse than silence.
    #[test]
    fn a_mono_source_is_widened_before_a_stereo_mix_sums_it() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let node = mono_plugin(&mut graph, 0);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 2,
                gains: Vec::new(),
            }),
            [0.0, 0.0],
        );
        let output = stereo_out(&mut graph);
        graph.connect(input, 0, node, 0);
        // The mono plugin into the first input, the stereo bus into the second.
        graph.connect(node, 0, mix, 0);
        graph.connect(input, 0, mix, 2);
        graph.connect(mix, 0, output, 0);

        let mut engine = crate::Engine::new();
        engine.prepare(8, &[2]);
        let handoff = crate::Handoff::new();
        handoff.send(Box::new(compile(&graph, SLOTS).unwrap()));
        assert!(engine.adopt(&handoff));

        let daw_in = [1.0f32, 1.0, 1.0, 1.0, 2.0, 2.0, 2.0, 2.0];
        let mut daw_out = [0.0f32; 8];
        engine.run_audio(&ctx(4), &daw_in, &mut daw_out, &mut PassThrough);
        // 3.0 on both channels out of the plugin, plus the input itself.
        assert_eq!(daw_out, [4.0, 4.0, 4.0, 4.0, 5.0, 5.0, 5.0, 5.0]);
    }

    /// A socket that already matches what is wired to it converts nothing.
    #[test]
    fn a_matching_width_costs_no_conversion() {
        let mut graph = Graph::new();
        let input = stereo_in(&mut graph);
        let node = plugin(&mut graph, 0, 0);
        let output = stereo_out(&mut graph);
        graph.connect(input, 0, node, 0);
        graph.connect(node, 0, output, 0);

        let program = compile(&graph, SLOTS).unwrap();
        assert!(
            !program
                .audio_ops
                .iter()
                .any(|op| matches!(op, AudioOp::Gather { .. })),
            "nothing to convert on either side of a stereo plugin"
        );
    }

    /// A plugin whose main bus is mono in both directions.
    fn mono_plugin(graph: &mut Graph, instance: usize) -> NodeId {
        graph.add(
            NodeKind::Plugin(Plugin {
                instance,
                ports: PluginPorts {
                    audio_in: vec![1],
                    audio_out: vec![1],
                    audio_out_shown: Vec::new(),
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        )
    }

    /// A stand-in sub-plugin that writes its input straight back out.
    struct PassThrough;

    impl AudioInstances for PassThrough {
        fn process(
            &mut self,
            _instance: u32,
            _notes: NoteStream,
            input: &[f32],
            output: &mut [f32],
            chunk: AudioChunk,
        ) {
            for ch in 0..chunk.output_channels {
                let range = chunk.channel(ch);
                if ch < chunk.input_channels {
                    output[range.clone()].copy_from_slice(&input[range]);
                } else {
                    output[range].fill(0.0);
                }
            }
        }
    }

    /// Records the shape and content of what a plugin node was handed.
    #[derive(Default)]
    struct RecordInput {
        channels: u16,
        first_of_each: Vec<f32>,
    }

    impl AudioInstances for RecordInput {
        fn process(
            &mut self,
            _instance: u32,
            _notes: NoteStream,
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
