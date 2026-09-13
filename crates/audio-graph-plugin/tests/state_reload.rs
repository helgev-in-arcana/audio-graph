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
