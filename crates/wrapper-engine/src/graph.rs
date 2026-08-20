//! The edit graph: what the user draws, and what gets saved.
//!
//! Deliberately dumb. It stores nodes and links and nothing else — no cached
//! ordering, no resolved pointers, no execution state. Everything the audio
//! thread needs is derived by [`compile`][crate::compile] into a flat
//! [`Program`][crate::Program]. That split is ARCHITECTURE.md §9.1, and it is
//! what makes it safe for this type to be edited freely: a half-finished graph
//! with a dangling link or a cycle is a perfectly ordinary thing for a user to
//! have on screen, and none of it can reach the audio thread.
//!
//! Nothing here knows what a VST3 is. A node reads a slot and a node writes a
//! slot; what a slot is bound to is the outer layer's business (§8).

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

pub type NodeId = u32;

/// Identifies one delay line. See [`NodeKind::DelayWrite`].
pub type LineId = u32;

/// What a port carries (§14.3).
///
/// Ports only connect to ports of the same type. Mono-to-stereo is deliberately
/// not implicit: a hidden widening rule is the same kind of thing as a hidden
/// mixing rule, and the graph already says no to those.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PortType {
    /// A scalar. One value per sub-block (§9.2).
    Param,
    /// Audio, `channels` wide.
    Audio { channels: u16 },
    /// Note events.
    Note,
}

impl PortType {
    pub const STEREO: PortType = PortType::Audio { channels: 2 };

    pub fn label(self) -> String {
        match self {
            PortType::Param => "param".into(),
            PortType::Audio { channels: 1 } => "mono".into(),
            PortType::Audio { channels: 2 } => "stereo".into(),
            PortType::Audio { channels } => format!("{channels} ch"),
            PortType::Note => "notes".into(),
        }
    }
}

/// One socket on a node.
#[derive(Debug, Clone, PartialEq)]
pub struct Port {
    pub name: Cow<'static, str>,
    pub ty: PortType,
}

impl Port {
    fn new(name: impl Into<Cow<'static, str>>, ty: PortType) -> Port {
        Port {
            name: name.into(),
            ty,
        }
    }

    fn param(name: impl Into<Cow<'static, str>>) -> Port {
        Port::new(name, PortType::Param)
    }
}

/// One sub-plugin parameter the graph is allowed to drive.
///
/// A plugin node does not get a socket per parameter — Chroma has 2106 of them.
/// The user picks which ones to expose, exactly as they pick slot bindings
/// today (§8.3), and each pick becomes a port.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamPort {
    /// The sub-plugin's own id for the parameter. A plain `u32` because the
    /// common data model is CLAP-shaped (ADR-4) — nothing here is VST3.
    pub id: u32,
    pub name: String,
}

/// A sub-plugin's port layout, as discovered after loading (§14.2).
///
/// Cached in the graph rather than asked for on demand. A patch has to reopen
/// with the right shape *before* its plugins have finished loading, and a node
/// whose plugin has gone missing still has to draw with the links it had.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PluginPorts {
    /// Channel count of each input bus. Bus 0 is main; the rest are aux
    /// (sidechain). Discovered from what the plugin *accepted*, not from what
    /// was asked for.
    #[serde(default)]
    pub audio_in: Vec<u16>,
    #[serde(default)]
    pub audio_out: Vec<u16>,
    #[serde(default)]
    pub accepts_notes: bool,
    #[serde(default)]
    pub params: Vec<ParamPort>,
    /// The plugin's reported latency, in samples.
    ///
    /// Discovered after loading like everything else here, and re-read when the
    /// plugin says `kLatencyChanged`. The compiler needs it to line up parallel
    /// paths (§14.6) and to work out how short a feedback loop may be (§14.4),
    /// so a change to it means a recompile.
    #[serde(default)]
    pub latency: u32,
}

impl PluginPorts {
    /// Build a node's ports from what a loaded plugin reported (§14.2).
    ///
    /// `params` is deliberately left empty. The parameter sockets are the
    /// user's choice, not the plugin's: a compressor with 90 parameters would
    /// otherwise arrive as a node with 90 sockets. The editor adds them one at
    /// a time.
    ///
    /// Widths are clamped to [`MAX_CHANNELS`][crate::MAX_CHANNELS]. M8 is
    /// stereo throughout (§14.8), and a node drawn with a socket the compiler
    /// will refuse is worse than one drawn narrow.
    pub fn from_layout(layout: &plugin_host_api::IoLayout, latency: u32) -> PluginPorts {
        let widths = |buses: &[plugin_host_api::BusInfo]| -> Vec<u16> {
            buses
                .iter()
                .map(|b| b.channels.min(crate::MAX_CHANNELS as u16))
                .filter(|&c| c > 0)
                .collect()
        };
        PluginPorts {
            audio_in: widths(&layout.inputs),
            audio_out: widths(&layout.outputs),
            accepts_notes: layout.accepts_notes,
            params: Vec::new(),
            latency,
        }
    }
}

/// One node's identity, position and settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    /// Canvas position, in the editor's own units. Stored because a graph that
    /// reopens with its nodes rearranged is a graph the user has to re-read.
    #[serde(default)]
    pub pos: [f32; 2],
    pub kind: NodeKind,
}

/// A connection from one node's output port to another node's input port.
///
/// Before M8 every node had exactly one output, so a link only had to name the
/// destination socket. A plugin node has as many outputs as the plugin turned
/// out to have buses (§14.2), so both ends are numbered now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub from: NodeId,
    /// Which of `from`'s outputs. Absent in pre-M8 patches, where it was
    /// always the only one.
    #[serde(default)]
    pub from_port: u8,
    pub to: NodeId,
    /// Which of `to`'s inputs. Named `input` before M8.
    #[serde(alias = "input")]
    pub to_port: u8,
}

/// The node set (§9.3 for the v1 core, §14 for the M8 additions).
///
/// Adding a variant here should require touching the compiler's `match` and
/// nothing else — that property is the thing §9.3 asks to be checked early, so
/// it is worth noticing if a new node ever wants more.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NodeKind {
    /// A fixed number.
    Constant { value: f64 },
    /// The DAW's automation for one wrapper slot, 0..1.
    SlotIn { slot: usize },
    /// A free-running or tempo-synced oscillator.
    Lfo {
        waveform: Waveform,
        rate: Rate,
        /// Starting phase, 0..1.
        phase: f64,
        /// Half the peak-to-peak swing.
        depth: f64,
        /// Centre of the swing. `depth 0.5 / offset 0.5` fills 0..1.
        offset: f64,
    },
    /// A note expression, reduced to one value (see [`ExprSource`]).
    Expression { source: ExprSource },
    /// Two inputs and an operator. Input 1 falls back to `b` when unconnected,
    /// so a "multiply by 0.5" node needs no second node feeding it.
    Math { op: MathOp, b: f64 },
    /// Rescale one range onto another. The 0..1 → plain-units half of §9.3 is
    /// the slot table's job (`ResolvedTarget::to_plain`); this is the shaping
    /// that happens before it.
    RangeMap {
        in_lo: f64,
        in_hi: f64,
        out_lo: f64,
        out_hi: f64,
        clamp: bool,
    },
    /// Drive a wrapper slot, replacing the DAW's automation for it.
    SlotOut { slot: usize },

    // --- M8 (§14) ---
    /// Audio arriving from the DAW on one of the wrapper's own input buses.
    AudioIn { bus: usize, channels: u16 },
    /// Audio leaving for the DAW on one of the wrapper's own output buses.
    AudioOut { bus: usize, channels: u16 },
    /// Notes arriving from the DAW.
    NoteIn,
    /// One hosted sub-plugin.
    ///
    /// `instance` indexes the wrapper's table of loaded sub-plugins, the same
    /// way `slot` indexes the slot table: which file that is, and how it was
    /// bound, stays outside the graph (§8.3). `ports` is the layout that was
    /// discovered after loading (§14.2), cached here.
    Plugin { instance: usize, ports: PluginPorts },
    /// The writing half of a delay line (§14.4).
    ///
    /// Has an input and no output, so a graph that goes through a delay has no
    /// cycle for the topological sort to find. That is the whole mechanism: the
    /// two halves are paired by `line`, never by an edge.
    DelayWrite { line: LineId, ty: PortType },
    /// Sum several audio inputs of the same width into one.
    ///
    /// The only way two audio sources reach one destination. An input takes one
    /// link everywhere in this graph, so mixing is a node rather than a rule —
    /// and being a node is what lets the compiler see the merge and line the
    /// paths up (§14.6).
    Mix { channels: u16, inputs: u8 },
    /// The reading half of a delay line (§14.4).
    ///
    /// Has an output and no input. Several reads may share one line — that is a
    /// multi-tap delay, and it falls out for free.
    ///
    /// `time` is in seconds and is clamped at run time to the floor of §14.4;
    /// the compiler cannot do the clamping itself because the floor depends on
    /// the sample rate and the sub-block size, neither of which it knows.
    DelayRead {
        line: LineId,
        ty: PortType,
        /// Longest delay this line will ever be asked for. Not automatable: the
        /// ring is allocated for it at activate, and §9.1 forbids allocating in
        /// `process`.
        max_time: f64,
        time: f64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Waveform {
    Sine,
    Triangle,
    Saw,
    Square,
    /// Sample and hold: a new random value at each cycle boundary.
    Random,
}

impl Waveform {
    pub const ALL: [Waveform; 5] = [
        Waveform::Sine,
        Waveform::Triangle,
        Waveform::Saw,
        Waveform::Square,
        Waveform::Random,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Waveform::Sine => "Sine",
            Waveform::Triangle => "Triangle",
            Waveform::Saw => "Saw",
            Waveform::Square => "Square",
            Waveform::Random => "Random",
        }
    }
}

/// How fast an LFO runs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Rate {
    Hz(f64),
    /// One cycle per this many beats, following the host's tempo.
    Beats(f64),
}

/// Which per-note controller a node reads.
///
/// v1 reduces polyphony away: each source keeps the most recent value from any
/// note. The graph is monophonic, so a per-voice value would have nowhere to
/// go. `Capabilities.poly_modulation` is what will decide whether the *voice*
/// level ever becomes reachable, and the editor already greys these out when
/// the sub-plugin cannot accept per-note modulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExprSource {
    Pressure,
    Tuning,
    Brightness,
    Expression,
    Vibrato,
    Volume,
    Pan,
    /// Velocity of the most recent note-on, 0..1.
    Velocity,
    /// 1 while any note is held, 0 otherwise.
    Gate,
    /// The most recent note's key, scaled to 0..1 across the MIDI range.
    KeyTrack,
}

impl ExprSource {
    pub const ALL: [ExprSource; 10] = [
        ExprSource::Pressure,
        ExprSource::Tuning,
        ExprSource::Brightness,
        ExprSource::Expression,
        ExprSource::Vibrato,
        ExprSource::Volume,
        ExprSource::Pan,
        ExprSource::Velocity,
        ExprSource::Gate,
        ExprSource::KeyTrack,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ExprSource::Pressure => "Pressure",
            ExprSource::Tuning => "Tuning",
            ExprSource::Brightness => "Brightness",
            ExprSource::Expression => "Expression",
            ExprSource::Vibrato => "Vibrato",
            ExprSource::Volume => "Volume",
            ExprSource::Pan => "Pan",
            ExprSource::Velocity => "Velocity",
            ExprSource::Gate => "Gate",
            ExprSource::KeyTrack => "Key track",
        }
    }

    /// Whether this source comes from a per-note controller rather than from
    /// the note itself. These are the ones a sub-plugin without per-note
    /// modulation cannot meaningfully receive.
    pub fn is_per_note(self) -> bool {
        !matches!(
            self,
            ExprSource::Velocity | ExprSource::Gate | ExprSource::KeyTrack
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MathOp {
    Add,
    Subtract,
    Multiply,
    Min,
    Max,
    /// `a^b` on a 0..1 input — the curve control of §9.3.
    Curve,
}

impl MathOp {
    pub const ALL: [MathOp; 6] = [
        MathOp::Add,
        MathOp::Subtract,
        MathOp::Multiply,
        MathOp::Min,
        MathOp::Max,
        MathOp::Curve,
    ];

    pub fn label(self) -> &'static str {
        match self {
            MathOp::Add => "Add",
            MathOp::Subtract => "Subtract",
            MathOp::Multiply => "Multiply",
            MathOp::Min => "Min",
            MathOp::Max => "Max",
            MathOp::Curve => "Curve",
        }
    }
}

impl NodeKind {
    /// This kind's input sockets, in order. Empty for a source node.
    ///
    /// Returns owned ports because a plugin node's sockets depend on what the
    /// plugin turned out to have (§14.2) and so cannot be a static slice. Every
    /// caller is on the main thread — the audio thread sees only a `Program`.
    pub fn input_ports(&self) -> Vec<Port> {
        match self {
            NodeKind::Constant { .. }
            | NodeKind::SlotIn { .. }
            | NodeKind::Lfo { .. }
            | NodeKind::Expression { .. }
            | NodeKind::AudioIn { .. }
            | NodeKind::NoteIn
            | NodeKind::DelayRead { .. } => Vec::new(),
            NodeKind::Math { .. } => vec![Port::param("a"), Port::param("b")],
            NodeKind::RangeMap { .. } | NodeKind::SlotOut { .. } => vec![Port::param("in")],
            NodeKind::AudioOut { channels, .. } => {
                vec![Port::new(
                    "in",
                    PortType::Audio {
                        channels: *channels,
                    },
                )]
            }
            NodeKind::DelayWrite { ty, .. } => vec![Port::new("in", *ty)],
            NodeKind::Mix { channels, inputs } => (0..*inputs)
                .map(|i| {
                    Port::new(
                        format!("in {}", i + 1),
                        PortType::Audio {
                            channels: *channels,
                        },
                    )
                })
                .collect(),
            NodeKind::Plugin { ports, .. } => plugin_input_ports(ports),
        }
    }

    /// This kind's output sockets, in order. Empty for a sink node.
    pub fn output_ports(&self) -> Vec<Port> {
        match self {
            NodeKind::SlotOut { .. } | NodeKind::AudioOut { .. } | NodeKind::DelayWrite { .. } => {
                Vec::new()
            }
            NodeKind::Constant { .. }
            | NodeKind::SlotIn { .. }
            | NodeKind::Lfo { .. }
            | NodeKind::Expression { .. }
            | NodeKind::Math { .. }
            | NodeKind::RangeMap { .. } => vec![Port::param("out")],
            NodeKind::AudioIn { channels, .. } => {
                vec![Port::new(
                    "out",
                    PortType::Audio {
                        channels: *channels,
                    },
                )]
            }
            NodeKind::NoteIn => vec![Port::new("out", PortType::Note)],
            NodeKind::DelayRead { ty, .. } => vec![Port::new("out", *ty)],
            NodeKind::Mix { channels, .. } => {
                vec![Port::new(
                    "out",
                    PortType::Audio {
                        channels: *channels,
                    },
                )]
            }
            NodeKind::Plugin { ports, .. } => ports
                .audio_out
                .iter()
                .enumerate()
                .map(|(i, &channels)| {
                    let name = if i == 0 {
                        "out".to_string()
                    } else {
                        format!("out {}", i + 1)
                    };
                    Port::new(name, PortType::Audio { channels })
                })
                .collect(),
        }
    }

    pub fn title(&self) -> String {
        match self {
            NodeKind::Constant { .. } => "Constant".into(),
            NodeKind::SlotIn { slot } => format!("Slot {} in", slot + 1),
            NodeKind::Lfo { .. } => "LFO".into(),
            NodeKind::Expression { source } => source.label().into(),
            NodeKind::Math { op, .. } => op.label().into(),
            NodeKind::RangeMap { .. } => "Range map".into(),
            NodeKind::SlotOut { slot } => format!("Slot {} out", slot + 1),
            NodeKind::AudioIn { bus, .. } => format!("Audio in {}", bus + 1),
            NodeKind::AudioOut { bus, .. } => format!("Audio out {}", bus + 1),
            NodeKind::NoteIn => "Note in".into(),
            NodeKind::Plugin { instance, .. } => format!("Plugin {}", instance + 1),
            NodeKind::DelayWrite { line, .. } => format!("Delay {} write", line + 1),
            NodeKind::DelayRead { line, .. } => format!("Delay {} read", line + 1),
            NodeKind::Mix { .. } => "Mix".into(),
        }
    }
}

/// A plugin node's inputs: main audio, then aux (sidechain), then notes if it
/// takes them, then one socket per exposed parameter.
///
/// The order matters more than it looks: it is what link indices mean, so
/// inserting a category in the middle would re-point every saved link. Grow it
/// only at the end.
fn plugin_input_ports(ports: &PluginPorts) -> Vec<Port> {
    let mut out = Vec::new();
    for (i, &channels) in ports.audio_in.iter().enumerate() {
        let name = match i {
            0 => "in".to_string(),
            1 => "sidechain".to_string(),
            _ => format!("aux {i}"),
        };
        out.push(Port::new(name, PortType::Audio { channels }));
    }
    if ports.accepts_notes {
        out.push(Port::new("notes", PortType::Note));
    }
    for param in &ports.params {
        out.push(Port::param(param.name.clone()));
    }
    out
}

/// The whole patch.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub links: Vec<Link>,
    /// Never reused, so a stale link can always be recognised as stale rather
    /// than silently re-pointing at whatever took the old id.
    next_id: NodeId,
}

impl Graph {
    pub fn new() -> Graph {
        Graph::default()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn add(&mut self, kind: NodeKind, pos: [f32; 2]) -> NodeId {
        let id = self.next_id;
        self.next_id += 1;
        self.nodes.push(Node { id, pos, kind });
        id
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

    /// Whether these two sockets may be joined.
    ///
    /// The editor asks this to decide what to draw; [`connect`][Self::connect]
    /// asks it again so that a caller which does not ask cannot make a graph
    /// the compiler would have to reject.
    pub fn can_connect(&self, from: NodeId, from_port: u8, to: NodeId, to_port: u8) -> bool {
        if from == to {
            return false;
        }
        match (
            self.output_type(from, from_port),
            self.input_type(to, to_port),
        ) {
            // Audio joins audio whatever the widths are; the compiler adapts
            // them explicitly (§14.11). The strict rule this replaces made a
            // real plugin's sidechain unreachable -- RoughRider3's is mono and
            // everything that would feed it is stereo -- and refusing the link
            // taught the user nothing, because nothing in the graph converts.
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

    /// Connect two sockets, replacing whatever already fed that input.
    ///
    /// An input takes one link. Feeding it two would need a mixing rule, and
    /// an explicit Add node says what a hidden rule would only imply.
    ///
    /// A mismatched or non-existent pair is ignored rather than reported: the
    /// editor has already decided what it will let the user drag onto what, and
    /// there is nothing useful for it to do with a failure here.
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

    /// What feeds one of a node's inputs: the source node and its output port.
    pub fn source_of(&self, to: NodeId, to_port: u8) -> Option<(NodeId, u8)> {
        self.links
            .iter()
            .find(|l| l.to == to && l.to_port == to_port)
            .map(|l| (l.from, l.from_port))
    }

    /// Drop links whose endpoints no longer exist, and inputs a node lost when
    /// its kind changed. Called after loading a graph that may predate an edit
    /// somewhere else.
    /// Also drops links whose ends stopped agreeing on a type — which is what
    /// happens when a sub-plugin is swapped for one with a different bus layout
    /// (§14.2). A patch that loses a few wires is better than one that refuses
    /// to make a sound until every wire is right.
    pub fn prune(&mut self) {
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
        // A hand-edited or future-versioned file could hold ids at or above the
        // counter; handing one of them out again would alias two nodes.
        self.next_id = self
            .next_id
            .max(ids.iter().copied().max().map_or(0, |m| m + 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_input_only_ever_has_one_source() {
        let mut graph = Graph::new();
        let a = graph.add(NodeKind::Constant { value: 1.0 }, [0.0, 0.0]);
        let b = graph.add(NodeKind::Constant { value: 2.0 }, [0.0, 0.0]);
        let out = graph.add(NodeKind::SlotOut { slot: 0 }, [0.0, 0.0]);

        graph.connect(a, 0, out, 0);
        graph.connect(b, 0, out, 0);

        assert_eq!(graph.links.len(), 1);
        assert_eq!(graph.source_of(out, 0), Some((b, 0)));
    }

    #[test]
    fn removing_a_node_takes_its_links_with_it() {
        let mut graph = Graph::new();
        let a = graph.add(NodeKind::Constant { value: 1.0 }, [0.0, 0.0]);
        let out = graph.add(NodeKind::SlotOut { slot: 3 }, [0.0, 0.0]);
        graph.connect(a, 0, out, 0);

        graph.remove(a);
        assert!(graph.links.is_empty());
        assert!(graph.source_of(out, 0).is_none());
    }

    #[test]
    fn ids_are_not_reused_after_a_delete() {
        let mut graph = Graph::new();
        let a = graph.add(NodeKind::Constant { value: 1.0 }, [0.0, 0.0]);
        graph.remove(a);
        let b = graph.add(NodeKind::Constant { value: 1.0 }, [0.0, 0.0]);
        assert_ne!(a, b);
    }

    #[test]
    fn prune_drops_links_that_no_longer_make_sense() {
        let mut graph = Graph::new();
        let a = graph.add(NodeKind::Constant { value: 1.0 }, [0.0, 0.0]);
        let math = graph.add(
            NodeKind::Math {
                op: MathOp::Add,
                b: 0.0,
            },
            [0.0, 0.0],
        );
        graph.connect(a, 0, math, 1);

        // Something the user could do in the editor: turn a two-input node into
        // a one-input one. The link to input 1 is now meaningless.
        graph.node_mut(math).unwrap().kind = NodeKind::RangeMap {
            in_lo: 0.0,
            in_hi: 1.0,
            out_lo: 0.0,
            out_hi: 1.0,
            clamp: true,
        };
        graph.prune();
        assert!(graph.links.is_empty());
    }

    #[test]
    fn ports_only_join_ports_of_the_same_type() {
        let mut graph = Graph::new();
        let audio = graph.add(
            NodeKind::AudioIn {
                bus: 0,
                channels: 2,
            },
            [0.0, 0.0],
        );
        let slot = graph.add(NodeKind::SlotOut { slot: 0 }, [0.0, 0.0]);
        let speaker = graph.add(
            NodeKind::AudioOut {
                bus: 0,
                channels: 2,
            },
            [0.0, 0.0],
        );

        graph.connect(audio, 0, slot, 0);
        assert!(graph.links.is_empty(), "audio does not fit a param socket");

        graph.connect(audio, 0, speaker, 0);
        assert_eq!(graph.links.len(), 1);
    }

    /// Audio joins audio whatever the widths are, and the compiler adapts them
    /// explicitly (§14.11).
    ///
    /// This reverses an earlier rule. Refusing the link read well until a real
    /// plugin turned up whose sidechain is mono — RoughRider3's is — with
    /// nothing in the graph able to convert. Refusing then taught the user
    /// nothing and left the socket unusable.
    #[test]
    fn audio_of_different_widths_still_connects() {
        let mut graph = Graph::new();
        let mono = graph.add(
            NodeKind::AudioIn {
                bus: 0,
                channels: 1,
            },
            [0.0, 0.0],
        );
        let stereo = graph.add(
            NodeKind::AudioOut {
                bus: 0,
                channels: 2,
            },
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
            NodeKind::AudioIn {
                bus: 0,
                channels: 2,
            },
            [0.0, 0.0],
        );
        let slot = graph.add(NodeKind::SlotOut { slot: 0 }, [0.0, 0.0]);
        graph.connect(audio, 0, slot, 0);
        assert!(graph.links.is_empty());
    }

    /// The two halves of a delay line are joined by `line`, never by a link, so
    /// neither has a socket facing the other.
    #[test]
    fn a_delay_has_no_edge_between_its_halves() {
        let write = NodeKind::DelayWrite {
            line: 0,
            ty: PortType::Param,
        };
        let read = NodeKind::DelayRead {
            line: 0,
            ty: PortType::Param,
            max_time: 1.0,
            time: 0.5,
        };
        assert!(write.output_ports().is_empty());
        assert!(read.input_ports().is_empty());
    }

    #[test]
    fn a_plugin_node_takes_its_shape_from_what_it_found() {
        let ports = PluginPorts {
            audio_in: vec![2, 2],
            audio_out: vec![2],
            accepts_notes: true,
            params: vec![ParamPort {
                id: 7,
                name: "Cutoff".into(),
            }],
            latency: 0,
        };
        let node = NodeKind::Plugin { instance: 0, ports };
        let inputs = node.input_ports();
        let names: Vec<&str> = inputs.iter().map(|p| p.name.as_ref()).collect();
        assert_eq!(names, ["in", "sidechain", "notes", "Cutoff"]);
        assert_eq!(inputs[1].ty, PortType::STEREO);
        assert_eq!(inputs[3].ty, PortType::Param);
        assert_eq!(node.output_ports().len(), 1);
    }

    /// A sub-plugin swapped for one with fewer buses leaves links pointing at
    /// sockets that no longer exist, or that changed type (§14.2). Those wires
    /// go; the rest of the patch stays.
    #[test]
    fn swapping_a_plugin_drops_only_the_links_that_stopped_making_sense() {
        let mut graph = Graph::new();
        let source = graph.add(
            NodeKind::AudioIn {
                bus: 0,
                channels: 2,
            },
            [0.0, 0.0],
        );
        let plugin = graph.add(
            NodeKind::Plugin {
                instance: 0,
                ports: PluginPorts {
                    audio_in: vec![2, 2],
                    audio_out: vec![2],
                    ..PluginPorts::default()
                },
            },
            [0.0, 0.0],
        );
        graph.connect(source, 0, plugin, 0);
        graph.connect(source, 0, plugin, 1);
        assert_eq!(graph.links.len(), 2);

        // The replacement has no sidechain.
        graph.node_mut(plugin).unwrap().kind = NodeKind::Plugin {
            instance: 0,
            ports: PluginPorts {
                audio_in: vec![2],
                audio_out: vec![2],
                ..PluginPorts::default()
            },
        };
        graph.prune();
        assert_eq!(graph.links.len(), 1, "the main input link survives");
        assert_eq!(graph.source_of(plugin, 0), Some((source, 0)));
    }

    /// Patches written before M8 name the destination socket `input` and have
    /// no source socket at all, because every node had exactly one output.
    #[test]
    fn a_pre_m8_patch_still_loads() {
        let json = r#"{
            "nodes": [
                {"id": 0, "pos": [0.0, 0.0], "kind": {"Constant": {"value": 0.5}}},
                {"id": 1, "pos": [10.0, 0.0], "kind": {"SlotOut": {"slot": 2}}}
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
            NodeKind::Lfo {
                waveform: Waveform::Triangle,
                rate: Rate::Beats(2.0),
                phase: 0.25,
                depth: 0.5,
                offset: 0.5,
            },
            [12.0, 34.0],
        );
        let out = graph.add(NodeKind::SlotOut { slot: 5 }, [200.0, 34.0]);
        graph.connect(lfo, 0, out, 0);

        let json = serde_json::to_string(&graph).unwrap();
        assert_eq!(serde_json::from_str::<Graph>(&json).unwrap(), graph);
    }
}
