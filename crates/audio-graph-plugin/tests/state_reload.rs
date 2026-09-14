//! A project or a preset the DAW hands over while the wrapper is running.
//!
//! nice-plug answers a state load by calling `activate` again rather than
//! deactivating first, so an activation is where a project arrives — both the
//! one that opens a session and the one dropped on a patch that is already
//! playing. Driven against `clap-test-plugin`, because the path that breaks is
//! the one taken only when a sub-plugin is already loaded.

mod harness;

use harness::{LIVE, fixture_as_clap, fx_layout};

use audio_graph_plugin::{Wrapper, WrapperKind};

/// How many wires the patch is currently carrying.
fn wires(wrapper: &Wrapper) -> usize {
    wrapper.shared().patch().graph.links.len()
}

/// Native rediscovery preserves the parameter selected before an auxiliary input disappeared.
#[test]
fn native_port_refresh_preserves_parameter_targets() {
    use audio_graph_engine::{
        AudioOut, Constant, Graph, NodeKind, ParamPort, Plugin, PluginPorts, compile,
    };
    use audio_graph_plugin::SLOT_COUNT;

    let _thread = plugin_host::init_thread().unwrap();
    let mut wrapper = Wrapper::default();
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .unwrap();
    wrapper
        .shared()
        .load_into(0, &fixture_as_clap("port-refresh"))
        .unwrap();
    let mut ports = PluginPorts::from_layout(&wrapper.shared().main().host.io_layout(0), 0);
    ports.audio_in.push(2);
    ports.params = vec![
        ParamPort {
            id: 0,
            name: "Gain".into(),
        },
        ParamPort {
            id: 1,
            name: "Offset".into(),
        },
    ];
    let param = (ports.audio_in.len() + usize::from(ports.accepts_notes)) as u8;
    let mut graph = Graph::new();
    let value = graph.add(NodeKind::Constant(Constant { value: 0.75 }), [0.0; 2]);
    let child = graph.add(NodeKind::Plugin(Plugin { instance: 0, ports }), [0.0; 2]);
    let output = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0; 2],
    );
    graph.connect(value, 0, child, param);
    graph.connect(child, 0, output, 0);
    let before = compile(&graph, SLOT_COUNT)
        .unwrap()
        .param_targets()
        .to_vec();
    wrapper.shared().patch().graph = graph;
    wrapper.shared().discover_ports(child);
    let after = compile(&wrapper.shared().patch().graph, SLOT_COUNT).unwrap();
    assert_eq!(after.param_targets(), before);
    assert_eq!(after.param_targets()[0].param, 0);
    wrapper.deactivate();
}

/// A project the DAW loads over a patch that is already playing is read in.
///
/// The DAW writes the blob and activates again, without deactivating first, and
/// that activation is the only chance to notice. A wrapper that skips it keeps
/// whatever was on the canvas before and quietly discards the project the user
/// just opened.
#[test]
fn a_project_loaded_over_a_running_patch_is_read_in() {
    let _thread = plugin_host::init_thread().unwrap();
    let mut wrapper = Wrapper::default();
    let layout = fx_layout();

    wrapper
        .activate(WrapperKind::Effect, &layout, &LIVE)
        .expect("the first activation");

    // The user picks a plugin: input, the plugin, output.
    wrapper
        .shared()
        .load(&fixture_as_clap("state-reload-fixture"))
        .expect("the fixture loads");
    wrapper.shared().adopt_default_patch();
    wrapper.store_state();
    let saved = wrapper
        .shared()
        .params()
        .state
        .0
        .read()
        .expect("not poisoned")
        .clone();
    let wired = wires(&wrapper);
    assert!(
        wired > 0,
        "the patch has to be wired for this to mean anything"
    );

    // …and then pulls every wire out and saves over it.
    {
        let mut patch = wrapper.shared().patch();
        patch.graph.links.clear();
    }
    wrapper.shared().publish_graph();
    wrapper.store_state();
    assert_eq!(wires(&wrapper), 0, "the edit has to reach the patch");

    // The DAW opens the earlier project: it writes the blob and activates,
    // with the wrapper still running and the sub-plugin still loaded.
    *wrapper
        .shared()
        .params()
        .state
        .0
        .write()
        .expect("not poisoned") = saved;
    wrapper
        .activate(WrapperKind::Effect, &layout, &LIVE)
        .expect("the activation that follows a state load");

    assert_eq!(
        wires(&wrapper),
        wired,
        "the project the DAW just loaded was thrown away for the one on screen"
    );
    assert!(
        wrapper.shared().main().host.is_loaded(0),
        "the project names a sub-plugin, so restoring it has to bring one back"
    );

    wrapper.deactivate();
}

/// Unavailable plugins preserve every saved socket and wire until native processing can resume.
#[test]
fn unavailable_plugins_keep_their_wiring_through_save_and_recovery() {
    use audio_graph_engine::{Constant, Graph, NodeKind, ParamPort};
    use audio_graph_plugin::WrapperState;
    use harness::{Block, Daw};

    let _thread = plugin_host::init_thread().unwrap();
    let mut wrapper = Wrapper::default();
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .unwrap();
    wrapper
        .shared()
        .load(&fixture_as_clap("missing-wiring"))
        .unwrap();
    wrapper.shared().adopt_default_patch();
    {
        let mut patch = wrapper.shared().patch();
        let (id, note_port, param_port) = patch
            .graph
            .nodes
            .iter_mut()
            .find_map(|node| {
                let NodeKind::Plugin(plugin) = &mut node.kind else {
                    return None;
                };
                assert!(plugin.ports.accepts_notes);
                plugin.ports.params.push(ParamPort {
                    id: 0,
                    name: "Gain".into(),
                });
                let note_port = plugin.ports.audio_in.len() as u8;
                Some((node.id, note_port, note_port + 1))
            })
            .unwrap();
        let notes = patch.graph.add(NodeKind::NoteIn, [0.0; 2]);
        let value = patch
            .graph
            .add(NodeKind::Constant(Constant { value: 0.75 }), [0.0; 2]);
        patch.graph.connect(notes, 0, id, note_port);
        patch.graph.connect(value, 0, id, param_port);
    }
    wrapper.shared().publish_graph();
    wrapper.store_state();
    let original: WrapperState =
        serde_json::from_str(&wrapper.wrapper_params().state.0.read().unwrap()).unwrap();
    let original_graph: Graph = serde_json::from_value(original.graph.clone().unwrap()).unwrap();
    assert_eq!(original_graph.links.len(), 4);

    for unsupported_format in [true, false] {
        let mut missing = original.clone();
        if unsupported_format {
            missing.sub_plugins[0].reference.format = "unavailable-format".into();
        } else {
            missing.sub_plugins[0].state = Some("AQID".into());
        }
        *wrapper.wrapper_params().state.0.write().unwrap() =
            serde_json::to_string(&missing).unwrap();
        wrapper
            .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
            .unwrap();
        assert!(!wrapper.shared().main().host.is_loaded(0));
        let mut block = Block::silent(64);
        block.fill(0.25).process(&mut wrapper, &mut Daw::playing());
        assert_eq!(block.peak(), 0.0);
        wrapper.store_state();
        let retained: WrapperState =
            serde_json::from_str(&wrapper.wrapper_params().state.0.read().unwrap()).unwrap();
        assert_eq!(retained.sub_plugins, missing.sub_plugins);
        assert_eq!(retained.graph, original.graph);

        *wrapper.wrapper_params().state.0.write().unwrap() =
            serde_json::to_string(&original).unwrap();
        wrapper
            .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
            .unwrap();
        assert!(wrapper.shared().main().host.is_loaded(0));
        block.fill(0.25).process(&mut wrapper, &mut Daw::playing());
        assert_eq!(block.peak(), 0.375);
        assert_eq!(wrapper.shared().patch().graph, original_graph);
    }
    wrapper.deactivate();
}

/// Unreadable documents remain byte-for-byte intact, silent and replaceable by a valid preset.
#[test]
fn unreadable_documents_are_retained_until_a_valid_load_or_explicit_replacement() {
    use audio_graph_engine::Graph;
    use audio_graph_plugin::{STATE_VERSION, WrapperState};
    use harness::{Block, Daw};

    let _thread = plugin_host::init_thread().unwrap();
    let mut wrapper = Wrapper::default();
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .unwrap();
    wrapper
        .shared()
        .load(&fixture_as_clap("unreadable-document"))
        .unwrap();
    wrapper.shared().adopt_default_patch();
    wrapper.store_state();
    let valid = wrapper.wrapper_params().state.0.read().unwrap().clone();
    let mut future: WrapperState = serde_json::from_str(&valid).unwrap();
    future.version = STATE_VERSION + 1;
    let mut unknown = future.clone();
    unknown.version = STATE_VERSION;
    unknown.graph = Some(serde_json::json!({
        "nodes": [{"id": 0, "kind": {"UnrecognizedNode": {"value": 0.5}}}],
        "links": [], "next_id": 1,
    }));

    for unreadable in [
        " { not valid JSON\n".to_owned(),
        serde_json::to_string_pretty(&future).unwrap(),
        serde_json::to_string_pretty(&unknown).unwrap(),
    ] {
        *wrapper.wrapper_params().state.0.write().unwrap() = unreadable.clone();
        wrapper
            .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
            .unwrap();
        assert!(wrapper.shared().restore_error().is_some());
        assert_eq!(
            wrapper
                .shared()
                .error_message(audio_graph_plugin::ErrorSource::State),
            wrapper.shared().restore_error()
        );
        assert!(!wrapper.shared().main().host.any_loaded());
        wrapper.shared().patch().graph = Graph::default_patch();
        wrapper.shared().publish_graph();
        let mut block = Block::silent(64);
        block.fill(0.25).process(&mut wrapper, &mut Daw::playing());
        assert_eq!(block.peak(), 0.0);
        wrapper.store_state();
        assert_eq!(
            *wrapper.wrapper_params().state.0.read().unwrap(),
            unreadable
        );
        wrapper
            .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
            .unwrap();
        wrapper.store_state();
        assert!(wrapper.shared().restore_error().is_some());
        assert_eq!(
            *wrapper.wrapper_params().state.0.read().unwrap(),
            unreadable
        );

        *wrapper.wrapper_params().state.0.write().unwrap() = valid.clone();
        wrapper
            .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
            .unwrap();
        assert!(wrapper.shared().restore_error().is_none());
        assert!(
            wrapper
                .shared()
                .error_message(audio_graph_plugin::ErrorSource::State)
                .is_none()
        );
        assert!(wrapper.shared().main().host.is_loaded(0));
        block.fill(0.25).process(&mut wrapper, &mut Daw::playing());
        assert_eq!(block.peak(), 0.25);
    }

    *wrapper.wrapper_params().state.0.write().unwrap() = "unreadable".into();
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .unwrap();
    wrapper.shared().start_new_graph().unwrap();
    assert!(wrapper.shared().restore_error().is_none());
    let fresh: WrapperState =
        serde_json::from_str(&wrapper.wrapper_params().state.0.read().unwrap()).unwrap();
    assert!(fresh.sub_plugins.is_empty());
    assert_eq!(
        fresh.graph,
        Some(serde_json::to_value(Graph::default_patch()).unwrap())
    );
    let mut block = Block::silent(64);
    block.fill(0.25).process(&mut wrapper, &mut Daw::playing());
    assert_eq!(block.peak(), 0.25);
    wrapper.deactivate();
}
