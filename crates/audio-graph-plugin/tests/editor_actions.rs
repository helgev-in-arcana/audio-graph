//! Everything the editor's buttons do, without the editor.
//!
//! The egui layer is a thin shell over [`Shared`]: each control resolves to one
//! call here. Driving those calls directly against a real installed plugin
//! covers the part that can actually be wrong — the load/bind/activate ordering
//! — and leaves only the drawing to be checked by eye.
//!
//! One `#[test]`, deliberately. VST3 pins its objects to the thread that created
//! them, and the test harness runs `#[test]` functions on a pool of threads;
//! splitting this would deadlock rather than fail.

use std::sync::Arc;

use audio_graph_plugin::{SLOT_COUNT, SUB_HOST};
use audio_graph_plugin::{Shared, WrapperParams};
use plugin_host::{AudioConfig, HostContext, RestartReason};
use subhost_adapter::SubHost;

struct SilentHost;

impl HostContext for SilentHost {
    fn host_name(&self) -> &str {
        "editor-actions test"
    }
    fn request_restart(&self, _reason: RestartReason) {}
    fn latency_changed(&self, _samples: u32) {}
    fn param_edited(&self, _id: plugin_host::ParamId, _value: f64) {}
}

/// Plugins this test will not instantiate.
///
/// Sampler and amp-sim hosts scan multi-gigabyte content libraries or display
/// authorization dialogs during first instantiation, which would stall headless tests.
///
/// Chroma creates a top-level window during instantiation prior to host UI initialization,
/// requiring an active message pump to avoid deadlocks.
///
/// `AUDIO_GRAPH_TEST_SUB` overrides the search entirely when a specific plugin is wanted.
const AVOID: &[&str] = &[
    "AmpliTube",
    "BBC Symphony",
    "Chroma",
    "Kontakt",
    "MODO",
    "Vienna",
    "Sine",
    "OTT",
];

/// How many modules to try before giving up.
const CANDIDATES: usize = 6;

/// A plugin to test against, chosen from whatever is installed.
///
/// Discovers any installed plugin with parameters to avoid machine-specific test fixtures.
fn a_plugin_with_parameters() -> Option<(std::path::PathBuf, Arc<Shared>)> {
    let mut tried = 0;
    for path in candidate_paths() {
        if tried >= CANDIDATES {
            break;
        }
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        if AVOID.iter().any(|a| name.contains(a)) {
            continue;
        }
        tried += 1;
        eprintln!("trying {name}");

        let params = WrapperParams::new();
        let shared = Shared::new(SubHost::new(Arc::new(SilentHost), SUB_HOST), params);
        if shared.load(&path).is_err() {
            continue;
        }
        if !shared.main().host.params(0).is_empty() {
            return Some((path, shared));
        }
    }
    None
}

fn candidate_paths() -> Vec<std::path::PathBuf> {
    // Initialize COM/STA apartment state on Windows.
    plugin_host::init_thread();
    if let Ok(explicit) = std::env::var("AUDIO_GRAPH_TEST_SUB") {
        return vec![std::path::PathBuf::from(explicit)];
    }
    // Both formats, exactly as the editor's own rescan sees them.
    plugin_host::installed_modules()
        .into_iter()
        .map(|(_, path)| path)
        .collect()
}

#[test]
fn the_editors_actions_work_against_an_installed_plugin() {
    let Some((path, shared)) = a_plugin_with_parameters() else {
        eprintln!("no installed plugin with parameters; skipping");
        return;
    };
    eprintln!("driving the editor's actions against {}", path.display());

    // The DAW has activated us, so the editor's later loads have a
    // configuration to activate against.
    shared.main().config = Some(AudioConfig {
        sample_rate: 48_000.0,
        max_block_size: 512,
        input_channels: 2,
        output_channels: 2,
        aux_inputs: Default::default(),
        aux_outputs: Default::default(),
        offline: true,
    });

    // "Rescan" — the list the editor draws.
    let installed = plugin_host::installed_modules().len();
    assert!(installed > 0, "the plugin list would be empty");

    // Clicking an entry in that list.
    shared.load(&path).expect("reload from the list");
    assert!(shared.main().host.is_loaded(0));
    assert!(
        shared.audio().processor.is_some(),
        "a load while the DAW is running has to leave the sub-plugin processing; \
         otherwise picking a plugin mid-session silently mutes the track"
    );

    // Clicking a parameter row: bind, then re-activate so the processor picks
    // the new target up.
    let first = shared.main().host.params(0)[0].clone();
    shared.main().host.bind_slot(0, 0, first.id).expect("bind");
    shared.rebind().expect("rebind");
    assert!(
        shared.main().host.slots().resolved(0).is_some(),
        "the binding has to resolve against the plugin it was just made from"
    );
    assert!(
        shared.audio().processor.is_some(),
        "still processing after a rebind"
    );

    // Every edit writes the state back, so the DAW always has something current
    // to save.
    shared.store_state();
    let saved = shared.params().state.0.read().unwrap().clone();
    assert!(
        saved.contains(&first.name),
        "the binding is missing from the saved state"
    );

    // The "Open plugin GUI" button is deliberately *not* exercised here.
    // Opening a real editor needs somebody pumping the Win32 message loop, and
    // a test binary is not pumping one; several plugins simply block. That path
    // already has a harness built for it — `host-cli sweep --gui` runs it across
    // every installed plugin, in both teardown orders, one child process each.

    // "x" on a slot row.
    shared.main().host.slots_mut().clear(0);
    shared.rebind().expect("rebind after clearing");
    assert!(shared.main().host.slots().resolved(0).is_none());

    // "Unload".
    shared.unload();
    assert!(!shared.main().host.is_loaded(0));
    assert!(
        shared.audio().processor.is_none(),
        "unloading has to take the processor with it, or the audio thread keeps \
         a processor whose plugin is gone"
    );
}

/// Somewhere for a parameter chain to go.
///
/// Helper to create a plugin node with a parameter port targeting `SINK_LANE`.
fn param_sink(graph: &mut audio_graph_engine::Graph) -> audio_graph_engine::NodeId {
    use audio_graph_engine::{ParamPort, Plugin, PluginPorts};
    graph.add(
        audio_graph_engine::NodeKind::Plugin(Plugin {
            instance: 0,
            ports: PluginPorts {
                params: vec![ParamPort {
                    id: 0,
                    name: "p".into(),
                }],
                ..PluginPorts::default()
            },
        }),
        [200.0, 0.0],
    )
}

/// The lane `param_sink`'s parameter is driven through.
const SINK_LANE: usize = SLOT_COUNT;

/// Verifies that an editor-constructed graph publishes and drives parameters in the engine.
#[test]
fn a_graph_built_the_way_the_editor_builds_one_drives_a_parameter() {
    use audio_graph_engine::{BlockContext, Engine, Lfo, Math, MathOp, NodeKind, Rate, Waveform};

    let params = WrapperParams::new();
    let shared = Shared::new(SubHost::new(Arc::new(SilentHost), SUB_HOST), params);

    // Dropping three nodes on the canvas and wiring them up.
    {
        let mut state = shared.main();
        let lfo = state.graph.add(
            NodeKind::Lfo(Lfo {
                waveform: Waveform::Saw,
                rate: Rate::Hz(2.0),
                phase: 0.0,
                depth: 0.5,
                offset: 0.5,
            }),
            [0.0, 0.0],
        );
        let half = state.graph.add(
            NodeKind::Math(Math {
                op: MathOp::Multiply,
                b: 0.5,
            }),
            [200.0, 0.0],
        );
        let out = param_sink(&mut state.graph);
        state.graph.connect(lfo, 0, half, 0);
        state.graph.connect(half, 0, out, 0);
    }
    shared.publish_graph();
    assert!(
        shared.main().compile_error.is_none(),
        "a valid graph must compile"
    );

    // The audio thread's side of the hand-off.
    let mut engine = Engine::new();
    assert!(
        engine.adopt(shared.programs()),
        "the program has to arrive without a lock"
    );
    assert!(engine.drives_lane(SINK_LANE));
    assert!(
        (0..SLOT_COUNT).all(|slot| !engine.drives_lane(slot)),
        "the DAW keeps every slot lane"
    );

    let mut slots = vec![0.9; audio_graph_plugin::LANES];
    let mut lowest = f64::INFINITY;
    let mut highest = f64::NEG_INFINITY;
    for _ in 0..2000 {
        slots[SINK_LANE] = 0.9;
        engine.run(
            &BlockContext {
                sample_rate: 48_000.0,
                tempo_bpm: 120.0,
                frames: 32,
            },
            &mut slots,
        );
        lowest = lowest.min(slots[SINK_LANE]);
        highest = highest.max(slots[SINK_LANE]);
    }
    assert!(
        highest > 0.45 && lowest < 0.05,
        "the parameter should sweep 0..0.5, got {lowest}..{highest}"
    );
    assert_eq!(
        slots[4], 0.9,
        "the graph must not touch a lane it does not drive"
    );

    // A graph the user has broken keeps the working program running.
    {
        let mut state = shared.main();
        let a = state.graph.add(
            NodeKind::Math(Math {
                op: MathOp::Add,
                b: 0.0,
            }),
            [0.0, 200.0],
        );
        let b = state.graph.add(
            NodeKind::Math(Math {
                op: MathOp::Add,
                b: 0.0,
            }),
            [0.0, 300.0],
        );
        let out = param_sink(&mut state.graph);
        state.graph.connect(a, 0, b, 0);
        state.graph.connect(b, 0, a, 0);
        state.graph.connect(b, 0, out, 0);
    }
    shared.publish_graph();
    assert!(
        shared.main().compile_error.is_some(),
        "a cycle has to be reported"
    );
    assert!(
        !engine.adopt(shared.programs()),
        "nothing new should have been published"
    );
    assert!(
        engine.drives_lane(SINK_LANE),
        "the last program that compiled keeps running"
    );
}

/// A project saved before the canvas showed the through-connection has to keep
/// making sound: what it was implicitly getting is drawn for it on open.
#[test]
fn a_patch_saved_without_a_graph_gets_the_default_one() {
    use audio_graph_engine::{AudioIn, AudioOut, Graph, NodeKind};

    let shared = Shared::new(
        SubHost::new(Arc::new(SilentHost), SUB_HOST),
        WrapperParams::new(),
    );
    shared.main().graph = Graph::new();
    shared.adopt_default_patch();

    let state = shared.main();
    assert_eq!(state.graph.nodes.len(), 2);
    assert!(matches!(
        state.graph.nodes[0].kind,
        NodeKind::AudioIn(AudioIn { bus: 0, .. })
    ));
    assert!(matches!(
        state.graph.nodes[1].kind,
        NodeKind::AudioOut(AudioOut { bus: 0, .. })
    ));
    assert_eq!(state.graph.links.len(), 1, "and it is wired");
}

/// A patch that was deliberately emptied is not a patch that never had a graph,
/// but nothing tells them apart in the file — so adoption only ever fires on a
/// graph with no nodes at all, and never touches one the user built.
#[test]
fn adoption_leaves_an_existing_graph_alone() {
    use audio_graph_engine::{Constant, NodeKind};

    let shared = Shared::new(
        SubHost::new(Arc::new(SilentHost), SUB_HOST),
        WrapperParams::new(),
    );
    let before = {
        let mut state = shared.main();
        state
            .graph
            .add(NodeKind::Constant(Constant { value: 0.5 }), [0.0, 0.0]);
        state.graph.clone()
    };
    shared.adopt_default_patch();
    assert_eq!(shared.main().graph, before);
}

/// Verify that graph structure and parameters survive state serialization round trips.
#[test]
fn a_graph_survives_the_state_round_trip() {
    use audio_graph_engine::{Constant, Graph, NodeKind};

    let params = WrapperParams::new();
    let shared = Shared::new(SubHost::new(Arc::new(SilentHost), SUB_HOST), params.clone());
    shared.set_quantum(64);
    {
        let mut state = shared.main();
        state.graph = Graph::default_patch();
        let c = state
            .graph
            .add(NodeKind::Constant(Constant { value: 0.25 }), [10.0, 20.0]);
        let out = param_sink(&mut state.graph);
        state.graph.connect(c, 0, out, 0);
    }
    shared.store_state();

    let json = params.state.0.read().unwrap().clone();
    let saved: audio_graph_plugin::WrapperState = serde_json::from_str(&json).unwrap();
    assert_eq!(saved.sub_block, 64);

    let restored: Graph = serde_json::from_value(saved.graph.expect("a graph was saved")).unwrap();
    assert_eq!(restored, shared.main().graph);
    let constant = restored
        .nodes
        .iter()
        .find(|n| matches!(n.kind, NodeKind::Constant(Constant { .. })))
        .expect("the constant was saved");
    assert_eq!(
        constant.pos,
        [10.0, 20.0],
        "positions are part of the patch"
    );
    assert!(
        saved.sub_plugin.is_none(),
        "a graph does not need a sub-plugin to be saved"
    );
}

/// End-to-end integration test: add a plugin node, discover its sockets, add a parameter
/// socket, and drive it from the graph against a real installed plugin.
#[test]
fn a_plugin_node_discovers_its_sockets_and_its_parameter_socket_drives_something() {
    use audio_graph_engine::{AudioOut, Constant, NodeKind, Plugin, PluginPorts};

    let Some((path, shared)) = a_plugin_with_parameters() else {
        eprintln!("no installed plugin with parameters; skipping");
        return;
    };
    eprintln!("building a plugin node for {}", path.display());

    shared.main().config = Some(AudioConfig {
        sample_rate: 48_000.0,
        max_block_size: 512,
        input_channels: 2,
        output_channels: 2,
        aux_inputs: Default::default(),
        aux_outputs: Default::default(),
        offline: true,
    });

    // The canvas adds the node first and the plugin arrives afterwards, so the
    // node starts with no sockets at all.
    let node = {
        let mut state = shared.main();
        state.graph.add(
            NodeKind::Plugin(Plugin {
                instance: 1,
                ports: PluginPorts::default(),
            }),
            [0.0, 0.0],
        )
    };
    assert!(
        shared.main().host.free_instance().is_some(),
        "a fresh wrapper has room"
    );

    shared.load_into(1, &path).expect("load into instance 1");
    shared.discover_ports(node);

    let ports = {
        let state = shared.main();
        let Some(NodeKind::Plugin(Plugin { ports, .. })) =
            state.graph.node(node).map(|n| n.kind.clone())
        else {
            panic!("the node should still be a plugin node");
        };
        ports
    };
    assert!(
        !ports.audio_out.is_empty(),
        "discovery has to find at least an output bus"
    );
    assert!(
        ports.params.is_empty(),
        "parameter sockets are user-configured, not created automatically by the plugin"
    );

    // "+ param" on the node, then a Constant wired into it. Port order is
    // audio inputs, then notes, then parameters.
    let first = shared.main().host.params(1)[0].clone();
    let socket = ports.audio_in.len() as u8 + u8::from(ports.accepts_notes);
    {
        let mut state = shared.main();
        let Some(node_mut) = state.graph.nodes.iter_mut().find(|n| n.id == node) else {
            panic!("node vanished")
        };
        if let NodeKind::Plugin(Plugin { ports, .. }) = &mut node_mut.kind {
            ports.params.push(audio_graph_engine::ParamPort {
                id: first.id.0,
                name: first.name.clone(),
            });
        }
        let constant = state
            .graph
            .add(NodeKind::Constant(Constant { value: 1.0 }), [-200.0, 0.0]);
        state.graph.connect(constant, 0, node, socket);
        // Something has to consume the plugin's audio or the node is not a
        // sink and never reaches the compiler.
        let out = state.graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [200.0, 0.0],
        );
        state.graph.connect(node, 0, out, 0);
    }
    shared.publish_graph();

    let state = shared.main();
    assert!(
        state.compile_error.is_none(),
        "the graph should compile: {:?}",
        state.compile_error
    );
    assert_eq!(
        state.graph_params.len(),
        1,
        "the wired socket should have become a parameter lane"
    );
    assert_eq!(state.graph_params[0].instance, 1);
    assert_eq!(state.graph_params[0].param, first.id.0);
    assert!(
        state.instance_io.iter().any(|i| i.instance == 1),
        "the plugin node should be activated with its discovered bus layout"
    );
}
