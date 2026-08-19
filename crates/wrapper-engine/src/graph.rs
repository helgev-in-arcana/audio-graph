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

use serde::{Deserialize, Serialize};

pub type NodeId = u32;

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

/// A connection from one node's output to another node's numbered input.
///
/// Every node has exactly one output in v1. That is not a limitation worth
/// removing yet: none of the §9.3 node types produce two values, and a second
/// output would double the link model for nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub from: NodeId,
    pub to: NodeId,
    pub input: u8,
}

/// The v1 node set (§9.3).
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
    /// Names of this kind's inputs, in order. Empty for a source node.
    pub fn inputs(&self) -> &'static [&'static str] {
        match self {
            NodeKind::Constant { .. }
            | NodeKind::SlotIn { .. }
            | NodeKind::Lfo { .. }
            | NodeKind::Expression { .. } => &[],
            NodeKind::Math { .. } => &["a", "b"],
            NodeKind::RangeMap { .. } => &["in"],
            NodeKind::SlotOut { .. } => &["in"],
        }
    }

    /// Whether this kind produces a value other nodes can read.
    pub fn has_output(&self) -> bool {
        !matches!(self, NodeKind::SlotOut { .. })
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
        }
    }
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

    /// Remove a node and every link that touched it.
    pub fn remove(&mut self, id: NodeId) {
        self.nodes.retain(|n| n.id != id);
        self.links.retain(|l| l.from != id && l.to != id);
    }

    /// Connect two nodes, replacing whatever already fed that input.
    ///
    /// An input takes one link. Feeding it two would need a mixing rule, and
    /// an explicit Add node says what a hidden rule would only imply.
    pub fn connect(&mut self, from: NodeId, to: NodeId, input: u8) {
        if from == to {
            return;
        }
        self.disconnect(to, input);
        self.links.push(Link { from, to, input });
    }

    pub fn disconnect(&mut self, to: NodeId, input: u8) {
        self.links.retain(|l| !(l.to == to && l.input == input));
    }

    pub fn source_of(&self, to: NodeId, input: u8) -> Option<NodeId> {
        self.links
            .iter()
            .find(|l| l.to == to && l.input == input)
            .map(|l| l.from)
    }

    /// Drop links whose endpoints no longer exist, and inputs a node lost when
    /// its kind changed. Called after loading a graph that may predate an edit
    /// somewhere else.
    pub fn prune(&mut self) {
        let ids: Vec<NodeId> = self.nodes.iter().map(|n| n.id).collect();
        let arity = |id: NodeId| {
            self.nodes
                .iter()
                .find(|n| n.id == id)
                .map_or(0, |n| n.kind.inputs().len() as u8)
        };
        let outputs = |id: NodeId| {
            self.nodes
                .iter()
                .find(|n| n.id == id)
                .is_some_and(|n| n.kind.has_output())
        };
        self.links.retain(|l| {
            ids.contains(&l.from) && ids.contains(&l.to) && l.input < arity(l.to) && outputs(l.from)
        });
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

        graph.connect(a, out, 0);
        graph.connect(b, out, 0);

        assert_eq!(graph.links.len(), 1);
        assert_eq!(graph.source_of(out, 0), Some(b));
    }

    #[test]
    fn removing_a_node_takes_its_links_with_it() {
        let mut graph = Graph::new();
        let a = graph.add(NodeKind::Constant { value: 1.0 }, [0.0, 0.0]);
        let out = graph.add(NodeKind::SlotOut { slot: 3 }, [0.0, 0.0]);
        graph.connect(a, out, 0);

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
        graph.connect(a, math, 1);

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
        graph.connect(lfo, out, 0);

        let json = serde_json::to_string(&graph).unwrap();
        assert_eq!(serde_json::from_str::<Graph>(&json).unwrap(), graph);
    }
}
