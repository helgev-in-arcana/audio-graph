//! Mutable edit graph data structures and operations.
//!
//! Represents the patch graph containing nodes, links, and positions for serialization
//! and editing. The graph is decoupled from execution state and audio processing,
//! which are compiled into a separate [`Program`][crate::Program].

use serde::{Deserialize, Serialize};

use crate::nodes::{AudioIn, AudioOut, DelayRead, DelayWrite, NodeKind};
use crate::port::PortType;

pub use crate::ir::NodeId;

/// Identifies one delay line. See [`NodeKind::DelayWrite`].
pub type LineId = u32;

/// One node's identity, position and settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    /// Canvas position, in editor coordinates.
    #[serde(default)]
    pub pos: [f32; 2],
    /// Compile this node even when nothing downstream reads its output (e.g. for visualizers/analyzers).
    #[serde(default)]
    pub always_on: bool,
    pub kind: NodeKind,
}

/// A connection from one node's output port to another node's input port.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub from: NodeId,
    /// Output port index on the source node.
    #[serde(default)]
    pub from_port: u8,
    pub to: NodeId,
    /// Input port index on the destination node.
    #[serde(alias = "input")]
    pub to_port: u8,
}

/// The complete node graph patch.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub links: Vec<Link>,
    /// Monotonically increasing ID counter to avoid ID reuse after deletions.
    next_id: NodeId,
}

impl Graph {
    pub fn new() -> Graph {
        Graph::default()
    }

    /// Returns the default patch: a main stereo audio input connected to the main stereo audio output.
    pub fn default_patch() -> Graph {
        let mut graph = Graph::new();
        let input = graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 2,
            }),
            [60.0, 80.0],
        );
        let output = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [360.0, 80.0],
        );
        graph.connect(input, 0, output, 0);
        graph
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn add(&mut self, kind: NodeKind, pos: [f32; 2]) -> NodeId {
        let id = self.next_id;
        self.next_id += 1;
        self.nodes.push(Node {
            id,
            pos,
            always_on: false,
            kind,
        });
        id
    }

    /// Add both halves of a delay line (write and read nodes) sharing an unused line index.
    ///
    /// Returns `(write, read)`.
    pub fn add_delay(&mut self, ty: PortType, pos: [f32; 2]) -> (NodeId, NodeId) {
        let line = self.free_line();
        let write = self.add(NodeKind::DelayWrite(DelayWrite { line, ty }), pos);
        let read = self.add(
            NodeKind::DelayRead(DelayRead {
                line,
                ty,
                max_time: 2.0,
                time: 0.25,
            }),
            [pos[0] + 170.0, pos[1]],
        );
        (write, read)
    }

    /// Finds the lowest line number not currently used by any delay node.
    pub fn free_line(&self) -> LineId {
        let mut line = 0;
        while self.nodes.iter().any(|n| match n.kind {
            NodeKind::DelayWrite(DelayWrite { line: l, .. })
            | NodeKind::DelayRead(DelayRead { line: l, .. }) => l == line,
            _ => false,
        }) {
            line += 1;
        }
        line
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        self.nodes.iter_mut().find(|n| n.id == id)
    }

    /// The type carried by one of a node's output sockets, if it has one.
    pub fn output_type(&self, id: NodeId, port: u8) -> Option<PortType> {
        let node = self.node(id)?;
        node.kind.output_ports().get(port as usize).map(|p| p.ty)
    }

    /// The type expected by one of a node's input sockets, if it has one.
    pub fn input_type(&self, id: NodeId, port: u8) -> Option<PortType> {
        let node = self.node(id)?;
        node.kind.input_ports().get(port as usize).map(|p| p.ty)
    }

    /// Checks whether an output port can connect to an input port.
    pub fn can_connect(&self, from: NodeId, from_port: u8, to: NodeId, to_port: u8) -> bool {
        if from == to {
            return false;
        }
        match (
            self.output_type(from, from_port),
            self.input_type(to, to_port),
        ) {
            // Audio ports can connect across different channel widths (adapted at compile time).
            (Some(PortType::Audio { .. }), Some(PortType::Audio { .. })) => true,
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }

    /// Remove a node and every link that touched it.
    pub fn remove(&mut self, id: NodeId) {
        self.nodes.retain(|n| n.id != id);
        self.links.retain(|l| l.from != id && l.to != id);
    }

    /// Connect two sockets, replacing any existing connection to the destination input port.
    pub fn connect(&mut self, from: NodeId, from_port: u8, to: NodeId, to_port: u8) {
        if !self.can_connect(from, from_port, to, to_port) {
            return;
        }
        self.disconnect(to, to_port);
        self.links.push(Link {
            from,
            from_port,
            to,
            to_port,
        });
    }

    pub fn disconnect(&mut self, to: NodeId, to_port: u8) {
        self.links.retain(|l| !(l.to == to && l.to_port == to_port));
    }

    /// Adjusts input connections when `count` input sockets starting at `first` are removed.
    ///
    /// Drops links into removed sockets and shifts indices of subsequent connections down.
    pub fn drop_inputs(&mut self, node: NodeId, first: u8, count: u8) {
        if count == 0 {
            return;
        }
        let end = first.saturating_add(count);
        self.links
            .retain(|l| !(l.to == node && (first..end).contains(&l.to_port)));
        for link in &mut self.links {
            if link.to == node && link.to_port >= end {
                link.to_port -= count;
            }
        }
    }

    /// Adjusts output connections when `count` output sockets starting at `first` are removed.
    ///
    /// Drops links from removed sockets and shifts indices of subsequent connections down.
    pub fn drop_outputs(&mut self, node: NodeId, first: u8, count: u8) {
        if count == 0 {
            return;
        }
        let end = first.saturating_add(count);
        self.links
            .retain(|l| !(l.from == node && (first..end).contains(&l.from_port)));
        for link in &mut self.links {
            if link.from == node && link.from_port >= end {
                link.from_port -= count;
            }
        }
    }

    /// What feeds one of a node's inputs: the source node and its output port.
    pub fn source_of(&self, to: NodeId, to_port: u8) -> Option<(NodeId, u8)> {
        self.links
            .iter()
            .find(|l| l.to == to && l.to_port == to_port)
            .map(|l| (l.from, l.from_port))
    }

    /// Prunes orphaned links and invalid connections whose port types or node IDs no longer match.
    pub fn prune(&mut self) {
        self.migrate_plugin_outputs();
        let ids: Vec<NodeId> = self.nodes.iter().map(|n| n.id).collect();
        let mut keep = Vec::with_capacity(self.links.len());
        for link in &self.links {
            keep.push(
                ids.contains(&link.from)
                    && ids.contains(&link.to)
                    && self.can_connect(link.from, link.from_port, link.to, link.to_port),
            );
        }
        let mut alive = keep.into_iter();
        self.links.retain(|_| alive.next().unwrap_or(false));
        // Ensure next_id is strictly greater than all existing node IDs.
        self.next_id = self
            .next_id
            .max(ids.iter().copied().max().map_or(0, |m| m + 1));
    }

    /// Migrates legacy plugin output configurations to preserve only active/wired output buses plus the main bus.
    pub fn migrate_plugin_outputs(&mut self) {
        for node in &mut self.nodes {
            let NodeKind::Plugin(plugin) = &mut node.kind else {
                continue;
            };
            if !plugin.ports.audio_out_shown.is_empty() || plugin.ports.audio_out.is_empty() {
                continue;
            }
            let mut keep: Vec<u16> = vec![0];
            for link in &self.links {
                if link.from == node.id && !keep.contains(&u16::from(link.from_port)) {
                    keep.push(u16::from(link.from_port));
                }
            }
            keep.retain(|&bus| usize::from(bus) < plugin.ports.audio_out.len());
            keep.sort_unstable();
            for link in &mut self.links {
                if link.from != node.id {
                    continue;
                }
                if let Some(port) = keep
                    .iter()
                    .position(|&bus| bus == u16::from(link.from_port))
                {
                    link.from_port = port as u8;
                }
            }
            plugin.ports.audio_out_shown = keep;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{MathOp, Waveform};
    use crate::nodes::{Constant, Lfo, Math, ParamPort, Plugin, PluginPorts, RangeMap, Rate};

    /// A node with one parameter socket, for the tests that only need
    /// something to wire *into*.
    fn param_input(graph: &mut Graph) -> NodeId {
        graph.add(
            NodeKind::Math(Math {
                op: MathOp::Add,
                b: 0.0,
            }),
            [0.0, 0.0],
        )
    }
    // `remove_input` is the editor's half of the node set, so the two tests
    // that exercise it are compiled with it.
    #[cfg(feature = "ui")]
    use crate::nodes::{Mix, Node};

    #[test]
    fn an_input_only_ever_has_one_source() {
        let mut graph = Graph::new();
        let a = graph.add(NodeKind::Constant(Constant { value: 1.0 }), [0.0, 0.0]);
        let b = graph.add(NodeKind::Constant(Constant { value: 2.0 }), [0.0, 0.0]);
        let out = param_input(&mut graph);

        graph.connect(a, 0, out, 0);
        graph.connect(b, 0, out, 0);

        assert_eq!(graph.links.len(), 1);
        assert_eq!(graph.source_of(out, 0), Some((b, 0)));
    }

    #[test]
    fn removing_a_node_takes_its_links_with_it() {
        let mut graph = Graph::new();
        let a = graph.add(NodeKind::Constant(Constant { value: 1.0 }), [0.0, 0.0]);
        let out = param_input(&mut graph);
        graph.connect(a, 0, out, 0);

        graph.remove(a);
        assert!(graph.links.is_empty());
        assert!(graph.source_of(out, 0).is_none());
    }

    #[test]
    fn ids_are_not_reused_after_a_delete() {
        let mut graph = Graph::new();
        let a = graph.add(NodeKind::Constant(Constant { value: 1.0 }), [0.0, 0.0]);
        graph.remove(a);
        let b = graph.add(NodeKind::Constant(Constant { value: 1.0 }), [0.0, 0.0]);
        assert_ne!(a, b);
    }

    /// A patch saved before `audio_out_shown` was populated retains active output buses
    /// and the main bus while pruning unused buses.
    #[test]
    fn an_old_patch_keeps_only_the_output_buses_it_wired() {
        let mut graph = Graph::new();
        let plugin = graph.add(
            NodeKind::Plugin(Plugin {
                instance: 0,
                ports: PluginPorts {
                    audio_in: vec![2],
                    audio_out: vec![2; 8],
                    // No picks: the file predates the field.
                    audio_out_shown: Vec::new(),
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        );
        let sink = graph.add(
            NodeKind::Mix(crate::nodes::Mix {
                channels: 2,
                inputs: 2,
                gains: vec![0.0, 0.0],
            }),
            [0.0, 0.0],
        );
        graph.connect(plugin, 5, sink, 0);

        graph.prune();

        let NodeKind::Plugin(node) = &graph.node(plugin).unwrap().kind else {
            unreachable!()
        };
        assert_eq!(
            node.ports.audio_out_shown,
            vec![0, 5],
            "the main bus and the one that was wired"
        );
        let link = graph.links.iter().find(|l| l.from == plugin).unwrap();
        assert_eq!(
            link.from_port, 1,
            "bus 5 is the second socket now, and the link says so"
        );
    }

    #[test]
    fn prune_drops_links_that_no_longer_make_sense() {
        let mut graph = Graph::new();
        let a = graph.add(NodeKind::Constant(Constant { value: 1.0 }), [0.0, 0.0]);
        let math = graph.add(
            NodeKind::Math(Math {
                op: MathOp::Add,
                b: 0.0,
            }),
            [0.0, 0.0],
        );
        graph.connect(a, 0, math, 1);

        // Something the user could do in the editor: turn a two-input node into
        // a one-input one. The link to input 1 is now meaningless.
        graph.node_mut(math).unwrap().kind = NodeKind::RangeMap(RangeMap {
            in_lo: 0.0,
            in_hi: 1.0,
            out_lo: 0.0,
            out_hi: 1.0,
            clamp: true,
        });
        graph.prune();
        assert!(graph.links.is_empty());
    }

    #[test]
    fn ports_only_join_ports_of_the_same_type() {
        let mut graph = Graph::new();
        let audio = graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        let slot = param_input(&mut graph);
        let speaker = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );

        graph.connect(audio, 0, slot, 0);
        assert!(graph.links.is_empty(), "audio does not fit a param socket");

        graph.connect(audio, 0, speaker, 0);
        assert_eq!(graph.links.len(), 1);
    }

    /// Audio ports connect across different channel widths, with channel adaptation handled during compilation.
    #[test]
    fn audio_of_different_widths_still_connects() {
        let mut graph = Graph::new();
        let mono = graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 1,
            }),
            [0.0, 0.0],
        );
        let stereo = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        graph.connect(mono, 0, stereo, 0);
        assert_eq!(graph.links.len(), 1);
    }

    /// What the widths rule does *not* do: audio and parameters stay apart.
    #[test]
    fn audio_still_does_not_connect_to_a_parameter() {
        let mut graph = Graph::new();
        let audio = graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        let slot = param_input(&mut graph);
        graph.connect(audio, 0, slot, 0);
        assert!(graph.links.is_empty());
    }

    /// The two halves of a delay line are joined by `line`, never by a link, so
    /// neither has a socket facing the other.
    #[test]
    fn a_delay_has_no_edge_between_its_halves() {
        let write = NodeKind::DelayWrite(DelayWrite {
            line: 0,
            ty: PortType::Param,
        });
        let read = NodeKind::DelayRead(DelayRead {
            line: 0,
            ty: PortType::Param,
            max_time: 1.0,
            time: 0.5,
        });
        assert!(write.output_ports().is_empty());
        // The read node's input is the delay time parameter, not a signal edge.
        // It expects a parameter input rather than a signal edge from the writer
        // to keep cycles out of the topological sort.
        assert_eq!(read.input_ports().len(), 1);
        assert!(matches!(read.input_ports()[0].ty, PortType::Param));
    }

    #[test]
    fn a_plugin_node_takes_its_shape_from_what_it_found() {
        let ports = PluginPorts {
            audio_in: vec![2, 2],
            audio_out: vec![2],
            audio_out_shown: Vec::new(),
            accepts_notes: true,
            params: vec![ParamPort {
                id: 7,
                name: "Cutoff".into(),
            }],
            latency: 0,
        };
        let node = NodeKind::Plugin(Plugin { instance: 0, ports });
        let inputs = node.input_ports();
        let names: Vec<&str> = inputs.iter().map(|p| p.name.as_ref()).collect();
        assert_eq!(names, ["in", "sidechain", "notes", "Cutoff"]);
        assert_eq!(inputs[1].ty, PortType::STEREO);
        assert_eq!(inputs[3].ty, PortType::Param);
        assert_eq!(node.output_ports().len(), 1);
    }

    /// A sub-plugin swapped for one with fewer buses leaves links pointing at
    /// sockets that no longer exist, or that changed type. Those wires
    /// go; the rest of the patch stays.
    #[test]
    fn swapping_a_plugin_drops_only_the_links_that_stopped_making_sense() {
        let mut graph = Graph::new();
        let source = graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        let plugin = graph.add(
            NodeKind::Plugin(Plugin {
                instance: 0,
                ports: PluginPorts {
                    audio_in: vec![2, 2],
                    audio_out: vec![2],
                    audio_out_shown: Vec::new(),
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        );
        graph.connect(source, 0, plugin, 0);
        graph.connect(source, 0, plugin, 1);
        assert_eq!(graph.links.len(), 2);

        // The replacement has no sidechain.
        graph.node_mut(plugin).unwrap().kind = NodeKind::Plugin(Plugin {
            instance: 0,
            ports: PluginPorts {
                audio_in: vec![2],
                audio_out: vec![2],
                audio_out_shown: Vec::new(),
                ..PluginPorts::default()
            },
        });
        graph.prune();
        assert_eq!(graph.links.len(), 1, "the main input link survives");
        assert_eq!(graph.source_of(plugin, 0), Some((source, 0)));
    }

    /// Taking one of a `Mix`'s inputs away is two sockets going out of the
    /// middle of the list, and every link after them means a socket one lower
    /// than it did. Nothing else notices: `prune` would leave these links
    /// where they were, still pointing at sockets of the right type and the
    /// wrong index, and the patch would quietly mix the wrong things.
    #[test]
    #[cfg(feature = "ui")]
    fn removing_an_input_slides_the_links_after_it_down() {
        let mut graph = Graph::new();
        let a = graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        let b = graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 1,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels: 2,
                inputs: 3,
                gains: vec![0.0, 0.0, 0.0],
            }),
            [0.0, 0.0],
        );
        // Signal, gain, signal, gain: input 2 is socket 2, input 3 is socket 4.
        graph.connect(a, 0, mix, 2);
        graph.connect(b, 0, mix, 4);

        // The user takes input 2 away — the pair at sockets 2 and 3.
        let NodeKind::Mix(node) = &mut graph.node_mut(mix).unwrap().kind else {
            unreachable!()
        };
        let count = node.remove_input(2);
        assert_eq!(count, 2, "an input is a signal and its gain");
        graph.drop_inputs(mix, 2, count);

        assert_eq!(
            graph.source_of(mix, 2),
            Some((b, 0)),
            "input 3 became input 2"
        );
        assert_eq!(graph.links.len(), 1, "what fed the removed input is cut");
        assert_eq!(graph.node(mix).unwrap().kind.input_ports().len(), 4);
    }

    /// The same for a plugin node's parameter sockets, which are one socket
    /// each rather than a pair — and which sit after the audio and note ones,
    /// so the sockets before them must not move.
    #[test]
    #[cfg(feature = "ui")]
    fn removing_a_parameter_socket_leaves_the_audio_ones_alone() {
        let mut graph = Graph::new();
        let audio = graph.add(
            NodeKind::AudioIn(AudioIn {
                bus: 0,
                channels: 2,
            }),
            [0.0, 0.0],
        );
        let value = graph.add(NodeKind::Constant(Constant { value: 0.5 }), [0.0, 0.0]);
        let plugin = graph.add(
            NodeKind::Plugin(Plugin {
                instance: 0,
                ports: PluginPorts {
                    audio_in: vec![2],
                    audio_out: vec![2],
                    audio_out_shown: Vec::new(),
                    params: vec![
                        ParamPort {
                            id: 1,
                            name: "Cutoff".into(),
                        },
                        ParamPort {
                            id: 2,
                            name: "Resonance".into(),
                        },
                    ],
                    ..PluginPorts::default()
                },
            }),
            [0.0, 0.0],
        );
        graph.connect(audio, 0, plugin, 0);
        graph.connect(value, 0, plugin, 2);

        // Cutoff, the first of the two parameter sockets, is socket 1.
        let NodeKind::Plugin(node) = &mut graph.node_mut(plugin).unwrap().kind else {
            unreachable!()
        };
        let count = node.remove_input(1);
        assert_eq!(count, 1);
        graph.drop_inputs(plugin, 1, count);

        assert_eq!(
            graph.source_of(plugin, 0),
            Some((audio, 0)),
            "audio is untouched"
        );
        assert_eq!(
            graph.source_of(plugin, 1),
            Some((value, 0)),
            "Resonance moved into Cutoff's place, and its link with it"
        );
    }

    /// Legacy serialized patches where links specify `input` without `from_port` remain loadable.
    #[test]
    fn a_pre_m8_patch_still_loads() {
        let json = r#"{
            "nodes": [
                {"id": 0, "pos": [0.0, 0.0], "kind": {"Constant": {"value": 0.5}}},
                {"id": 1, "pos": [10.0, 0.0], "kind": {"Math": {"op": "Add", "b": 0.0}}}
            ],
            "links": [{"from": 0, "to": 1, "input": 0}],
            "next_id": 2
        }"#;
        let graph: Graph = serde_json::from_str(json).expect("an M5 patch is still a patch");
        assert_eq!(graph.source_of(1, 0), Some((0, 0)));
    }

    #[test]
    fn a_graph_survives_a_json_round_trip() {
        let mut graph = Graph::new();
        let lfo = graph.add(
            NodeKind::Lfo(Lfo {
                waveform: Waveform::Triangle,
                rate: Rate::Beats(2.0),
                phase: 0.25,
                depth: 0.5,
                offset: 0.5,
            }),
            [12.0, 34.0],
        );
        let out = param_input(&mut graph);
        graph.connect(lfo, 0, out, 0);

        let json = serde_json::to_string(&graph).unwrap();
        assert_eq!(serde_json::from_str::<Graph>(&json).unwrap(), graph);
    }
}
