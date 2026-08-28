//! Golden snapshots of compiled programs for regression testing.
//!
//! Snapshot tests compare compiled [`Program`] outputs (including instruction order,
//! register allocations, buffer indices, and lane mappings) against expected
//! golden fixtures in `tests/golden/`.
//!
//! Golden snapshots can be regenerated using:
//! ```text
//! BLESS_GOLDEN=1 cargo test -p audio-graph-engine --test golden
//! ```

use std::path::PathBuf;

use audio_graph_engine::{
    AudioIn, AudioOut, Constant, ExprSource, Expression, Gate, Graph, Lfo, Math, MathOp, Mix,
    NodeId, NodeKind, ParamPort, Plugin, PluginPorts, PortType, RangeMap, Rate, SlotIn, Waveform,
    compile, linear_to_db,
};

const SLOTS: usize = 32;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// Compile `graph` and compare the pretty-printed program with `tests/golden/<name>.txt`.
fn check(name: &str, graph: &Graph) {
    let program =
        compile(graph, SLOTS).unwrap_or_else(|e| panic!("{name} failed to compile: {e:?}"));
    let actual = format!("{program:#?}\n");
    let path = golden_dir().join(format!("{name}.txt"));

    if std::env::var_os("BLESS_GOLDEN").is_some() {
        std::fs::create_dir_all(golden_dir()).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing golden {}; re-run with BLESS_GOLDEN=1",
            path.display()
        )
    });
    assert_eq!(
        expected, actual,
        "compiled program for `{name}` changed; \
         if that was intended, re-run with BLESS_GOLDEN=1 and read the diff"
    );
}

// --- patch builders -------------------------------------------------------

fn lfo(rate: Rate) -> NodeKind {
    NodeKind::Lfo(Lfo {
        waveform: Waveform::Sine,
        rate,
        phase: 0.0,
        depth: 0.5,
        offset: 0.5,
    })
}

fn stereo_plugin(instance: usize, latency: u32) -> NodeKind {
    NodeKind::Plugin(Plugin {
        instance,
        ports: PluginPorts {
            audio_in: vec![2],
            audio_out: vec![2],
            audio_out_shown: Vec::new(),
            accepts_notes: false,
            params: vec![ParamPort {
                id: 7,
                name: "Drive".into(),
            }],
            latency,
        },
    })
}

/// Helper creating a parameter sink plugin node with one parameter socket.
fn param_sink(graph: &mut Graph) -> NodeId {
    graph.add(
        NodeKind::Plugin(Plugin {
            instance: 0,
            ports: PluginPorts {
                params: vec![ParamPort {
                    id: 7,
                    name: "Drive".into(),
                }],
                ..PluginPorts::default()
            },
        }),
        [0.0, 0.0],
    )
}

fn audio_in(graph: &mut Graph, bus: usize, channels: u16) -> NodeId {
    graph.add(NodeKind::AudioIn(AudioIn { bus, channels }), [0.0, 0.0])
}

fn audio_out(graph: &mut Graph, bus: usize, channels: u16) -> NodeId {
    graph.add(NodeKind::AudioOut(AudioOut { bus, channels }), [0.0, 0.0])
}

// --- the shapes -----------------------------------------------------------

#[test]
fn default_patch() {
    check("default_patch", &Graph::default_patch());
}

#[test]
fn lfo_into_a_parameter() {
    let mut graph = Graph::new();
    let osc = graph.add(lfo(Rate::Beats(4.0)), [0.0, 0.0]);
    let out = param_sink(&mut graph);
    graph.connect(osc, 0, out, 0);
    check("lfo_into_a_parameter", &graph);
}

/// Both halves of `Math`'s fallback rule: input `b` wired, and input `b` left
/// to the node's own number.
#[test]
fn math_chain() {
    let mut graph = Graph::new();
    let slot = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
    let expr = graph.add(
        NodeKind::Expression(Expression {
            source: ExprSource::Velocity,
        }),
        [0.0, 0.0],
    );
    // Two inputs wired: `b` is ignored.
    let both = graph.add(
        NodeKind::Math(Math {
            op: MathOp::Multiply,
            b: 0.25,
        }),
        [0.0, 0.0],
    );
    graph.connect(slot, 0, both, 0);
    graph.connect(expr, 0, both, 1);
    // One input wired: `b` is the constant.
    let fallback = graph.add(
        NodeKind::Math(Math {
            op: MathOp::Curve,
            b: 2.0,
        }),
        [0.0, 0.0],
    );
    graph.connect(both, 0, fallback, 0);
    let shaped = graph.add(
        NodeKind::RangeMap(RangeMap {
            in_lo: 0.0,
            in_hi: 1.0,
            out_lo: -1.0,
            out_hi: 1.0,
            clamp: true,
        }),
        [0.0, 0.0],
    );
    graph.connect(fallback, 0, shaped, 0);
    let out = param_sink(&mut graph);
    graph.connect(shaped, 0, out, 0);
    check("math_chain", &graph);
}

/// A gate: the parameter half switches the gain, the audio half is a `Mix` of
/// one. Pinned because the whole node is that arrangement.
#[test]
fn gated_audio() {
    let mut graph = Graph::new();
    let src = audio_in(&mut graph, 0, 2);
    let control = graph.add(NodeKind::SlotIn(SlotIn { slot: 0 }), [0.0, 0.0]);
    let gate = graph.add(
        NodeKind::Gate(Gate {
            channels: 2,
            threshold: 0.5,
            invert: false,
        }),
        [0.0, 0.0],
    );
    let out = audio_out(&mut graph, 0, 2);
    graph.connect(src, 0, gate, 0);
    graph.connect(control, 0, gate, 1);
    graph.connect(gate, 0, out, 0);
    check("gated_audio", &graph);
}

/// A plugin with a sidechain bus fed by a source of a different width, which is
/// what makes `Gather` do a conversion rather than a copy.
#[test]
fn plugin_with_sidechain() {
    let mut graph = Graph::new();
    let main = audio_in(&mut graph, 0, 2);
    let side = audio_in(&mut graph, 1, 1);
    let plugin = graph.add(
        NodeKind::Plugin(Plugin {
            instance: 0,
            ports: PluginPorts {
                audio_in: vec![2, 2],
                audio_out: vec![2],
                audio_out_shown: Vec::new(),
                accepts_notes: false,
                params: vec![ParamPort {
                    id: 12,
                    name: "Threshold".into(),
                }],
                latency: 0,
            },
        }),
        [0.0, 0.0],
    );
    let out = audio_out(&mut graph, 0, 2);
    graph.connect(main, 0, plugin, 0);
    // Port 1 is the aux input; the mono source is widened into it.
    graph.connect(side, 0, plugin, 1);
    graph.connect(plugin, 0, out, 0);

    let param = graph.add(NodeKind::Constant(Constant { value: 0.75 }), [0.0, 0.0]);
    let param_port = plugin_param_port(&graph, plugin);
    graph.connect(param, 0, plugin, param_port);

    check("plugin_with_sidechain", &graph);
}

/// The second output bus is only reachable through `Split`.
#[test]
fn plugin_with_two_outputs() {
    let mut graph = Graph::new();
    let src = audio_in(&mut graph, 0, 2);
    let plugin = graph.add(
        NodeKind::Plugin(Plugin {
            instance: 1,
            ports: PluginPorts {
                audio_in: vec![2],
                audio_out: vec![2, 2],
                audio_out_shown: Vec::new(),
                accepts_notes: false,
                params: Vec::new(),
                latency: 0,
            },
        }),
        [0.0, 0.0],
    );
    let main = audio_out(&mut graph, 0, 2);
    let aux = audio_out(&mut graph, 1, 2);
    graph.connect(src, 0, plugin, 0);
    graph.connect(plugin, 0, main, 0);
    graph.connect(plugin, 1, aux, 0);
    check("plugin_with_two_outputs", &graph);
}

/// An instrument: notes routed by name, no audio input.
#[test]
fn instrument_with_notes() {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0, 0.0]);
    let plugin = graph.add(
        NodeKind::Plugin(Plugin {
            instance: 0,
            ports: PluginPorts {
                audio_in: Vec::new(),
                audio_out: vec![2],
                audio_out_shown: Vec::new(),
                accepts_notes: true,
                params: Vec::new(),
                latency: 0,
            },
        }),
        [0.0, 0.0],
    );
    let out = audio_out(&mut graph, 0, 2);
    // The note socket comes first when the plugin accepts notes.
    graph.connect(notes, 0, plugin, 0);
    graph.connect(plugin, 0, out, 0);
    check("instrument_with_notes", &graph);
}

/// Three audio inputs, one of the gains driven by a param source: the path
/// that puts a value on an audio lane.
#[test]
fn mix_with_a_driven_gain() {
    let mut graph = Graph::new();
    let a = audio_in(&mut graph, 0, 2);
    let b = audio_in(&mut graph, 1, 2);
    let c = audio_in(&mut graph, 2, 2);
    let mix = graph.add(
        NodeKind::Mix(Mix {
            channels: 2,
            inputs: 3,
            gains: vec![0.0, linear_to_db(0.5), linear_to_db(0.25)],
        }),
        [0.0, 0.0],
    );
    let out = audio_out(&mut graph, 0, 2);
    // Sockets alternate: in 1, gain 1, in 2, gain 2, in 3, gain 3.
    graph.connect(a, 0, mix, 0);
    graph.connect(b, 0, mix, 2);
    graph.connect(c, 0, mix, 4);
    let osc = graph.add(lfo(Rate::Hz(2.0)), [0.0, 0.0]);
    graph.connect(osc, 0, mix, 3);
    graph.connect(mix, 0, out, 0);
    check("mix_with_a_driven_gain", &graph);
}

/// Audio fed back through a delay line. The loop is what forces sub-block
/// chunking.
#[test]
fn audio_feedback_delay() {
    let mut graph = Graph::new();
    let src = audio_in(&mut graph, 0, 2);
    let (write, read) = graph.add_delay(PortType::STEREO, [0.0, 0.0]);
    let mix = graph.add(
        NodeKind::Mix(Mix {
            channels: 2,
            inputs: 2,
            gains: vec![0.0, linear_to_db(0.5)],
        }),
        [0.0, 0.0],
    );
    let out = audio_out(&mut graph, 0, 2);
    graph.connect(src, 0, mix, 0);
    graph.connect(read, 0, mix, 2);
    graph.connect(mix, 0, write, 0);
    graph.connect(mix, 0, out, 0);
    check("audio_feedback_delay", &graph);
}

/// A param-rate delay line, with its time driven rather than fixed.
#[test]
fn param_delay() {
    let mut graph = Graph::new();
    let (write, read) = graph.add_delay(PortType::Param, [0.0, 0.0]);
    let src = graph.add(NodeKind::SlotIn(SlotIn { slot: 2 }), [0.0, 0.0]);
    graph.connect(src, 0, write, 0);
    let time = graph.add(NodeKind::Constant(Constant { value: 0.1 }), [0.0, 0.0]);
    graph.connect(time, 0, read, 0);
    let out = param_sink(&mut graph);
    graph.connect(read, 0, out, 0);
    check("param_delay", &graph);
}

/// Two parallel paths of different latency meeting at a mix: the merge point
/// is where compensation gets inserted.
#[test]
fn latency_compensation() {
    let mut graph = Graph::new();
    let src = audio_in(&mut graph, 0, 2);
    let slow = graph.add(stereo_plugin(0, 512), [0.0, 0.0]);
    let mix = graph.add(
        NodeKind::Mix(Mix {
            channels: 2,
            inputs: 2,
            gains: vec![linear_to_db(0.5), linear_to_db(0.5)],
        }),
        [0.0, 0.0],
    );
    let out = audio_out(&mut graph, 0, 2);
    graph.connect(src, 0, slow, 0);
    graph.connect(slow, 0, mix, 0);
    // The dry path has no latency at all, so it is the one that gets delayed.
    graph.connect(src, 0, mix, 2);
    graph.connect(mix, 0, out, 0);
    check("latency_compensation", &graph);
}

/// Verifies compilation of a node explicitly marked always_on.
#[test]
fn always_on_node() {
    let mut graph = Graph::new();
    let osc = graph.add(lfo(Rate::Hz(1.0)), [0.0, 0.0]);
    graph.node_mut(osc).unwrap().always_on = true;
    check("always_on_node", &graph);
}

// --- the patch format -----------------------------------------------------

/// JSON patch fixture exercising serialization across all node kinds.
const EVERY_KIND: &str = r#"{
  "nodes": [
    {"id": 0, "pos": [0.0, 0.0], "kind": {"Constant": {"value": 0.5}}},
    {"id": 1, "pos": [0.0, 0.0], "kind": {"SlotIn": {"slot": 0}}},
    {"id": 2, "pos": [0.0, 0.0], "kind": {"Lfo": {
      "waveform": "Triangle", "rate": {"Beats": 2.0},
      "phase": 0.25, "depth": 0.5, "offset": 0.5}}},
    {"id": 3, "pos": [0.0, 0.0], "kind": {"Expression": {"source": "Pressure"}}},
    {"id": 4, "pos": [0.0, 0.0], "kind": {"Math": {"op": "Multiply", "b": 0.75}}},
    {"id": 5, "pos": [0.0, 0.0], "kind": {"RangeMap": {
      "in_lo": 0.0, "in_hi": 1.0, "out_lo": -1.0, "out_hi": 1.0, "clamp": true}}},
    {"id": 7, "pos": [0.0, 0.0], "kind": {"AudioIn": {"bus": 0, "channels": 2}}},
    {"id": 8, "pos": [0.0, 0.0], "kind": {"AudioOut": {"bus": 0, "channels": 2}}},
    {"id": 9, "pos": [0.0, 0.0], "kind": "NoteIn"},
    {"id": 10, "pos": [0.0, 0.0], "kind": {"Plugin": {"instance": 0, "ports": {
      "audio_in": [2], "audio_out": [2], "accepts_notes": true,
      "params": [{"id": 7, "name": "Drive"}], "latency": 128}}}},
    {"id": 11, "pos": [0.0, 0.0], "kind": {"DelayWrite": {
      "line": 0, "ty": {"Audio": {"channels": 2}}}}},
    {"id": 12, "pos": [0.0, 0.0], "kind": {"DelayRead": {
      "line": 0, "ty": {"Audio": {"channels": 2}}, "max_time": 2.0, "time": 0.25}}},
    {"id": 13, "pos": [0.0, 0.0], "always_on": true, "kind": {"Mix": {
      "channels": 2, "inputs": 2, "gains": [1.0, 0.5]}}}
  ],
  "links": [
    {"from": 0, "from_port": 0, "to": 4, "to_port": 1},
    {"from": 1, "from_port": 0, "to": 4, "to_port": 0},
    {"from": 4, "from_port": 0, "to": 5, "to_port": 0},
    {"from": 5, "from_port": 0, "to": 10, "to_port": 2}
  ],
  "next_id": 14
}"#;

#[test]
fn every_kind_survives_a_round_trip() {
    let graph: Graph = serde_json::from_str(EVERY_KIND).expect("the literal patch still loads");
    assert_eq!(
        graph.nodes.len(),
        13,
        "one node per kind, plus both delay halves"
    );

    let text = serde_json::to_string(&graph).unwrap();
    let again: Graph = serde_json::from_str(&text).unwrap();
    assert_eq!(graph, again, "a saved patch reopens as itself");
}

/// The wire form itself, so a change to how a kind is spelled has to be
/// deliberate. Kept separate from the round trip above: that one would still
/// pass if every name changed at once.
#[test]
fn the_wire_form_is_pinned() {
    let graph: Graph = serde_json::from_str(EVERY_KIND).unwrap();
    let value: serde_json::Value = serde_json::to_value(&graph).unwrap();
    let kinds: Vec<String> = value["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| match &n["kind"] {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Object(o) => o.keys().next().unwrap().clone(),
            other => panic!("unexpected kind encoding: {other}"),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "Constant",
            "SlotIn",
            "Lfo",
            "Expression",
            "Math",
            "RangeMap",
            "AudioIn",
            "AudioOut",
            "NoteIn",
            "Plugin",
            "DelayWrite",
            "DelayRead",
            "Mix",
        ]
    );

    // The one thing a struct-to-newtype change could plausibly move: the
    // payload sits directly under the variant name, not under a nested field.
    assert_eq!(value["nodes"][0]["kind"]["Constant"]["value"], 0.5);
    assert_eq!(value["nodes"][8]["kind"], "NoteIn");
}

/// Where a plugin node's first parameter socket is, given its buses.
fn plugin_param_port(graph: &Graph, plugin: NodeId) -> u8 {
    let kind = &graph.node(plugin).unwrap().kind;
    let ports = kind.input_ports();
    ports
        .iter()
        .position(|p| p.ty == PortType::Param)
        .expect("the plugin has a parameter socket") as u8
}
