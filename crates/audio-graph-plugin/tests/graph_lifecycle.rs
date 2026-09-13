mod harness;

use audio_graph_engine::{Graph, NodeKind, Plugin, PluginPorts};
use audio_graph_plugin::{GraphEdit, Wrapper, WrapperKind, WrapperState};
use harness::{Block, Daw, LIVE, fixture_as_clap, fx_layout};

fn saved(wrapper: &Wrapper) -> WrapperState {
    serde_json::from_str(&wrapper.wrapper_params().state.0.read().unwrap()).unwrap()
}

fn add(path: &std::path::Path) -> GraphEdit {
    GraphEdit::AddPlugin {
        path: path.to_owned(),
        pos: [0.0; 2],
    }
}

/// Node deletion and whole-document replacement release native and saved child ownership together.
#[test]
fn editing_child_nodes_releases_instances_and_preserves_explicit_empty_graphs() {
    let _thread = plugin_host::init_thread().unwrap();
    let mut wrapper = Wrapper::default();
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .unwrap();
    let path = fixture_as_clap("graph-child-lifetimes");
    let shared = wrapper.shared().clone();
    let document = shared.document_generation();
    for _ in 0..2 {
        let path = path.clone();
        shared.post_main(move |shared| {
            assert!(shared.edit_graph(document, add(&path)).unwrap());
        });
    }
    assert!(!shared.main().host.any_loaded());
    shared.run_posted();
    let nodes: Vec<_> = shared
        .patch()
        .graph
        .nodes
        .iter()
        .filter_map(|node| {
            if let NodeKind::Plugin(plugin) = &node.kind {
                Some((node.id, plugin.instance))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        nodes
            .iter()
            .map(|(_, instance)| *instance)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(
        saved(&wrapper).sub_plugins.len(),
        2,
        "unconnected nodes still own their children"
    );
    assert!(
        shared
            .edit_graph(document, GraphEdit::RemoveNode(nodes[0].0))
            .unwrap()
    );
    assert!(!shared.main().host.is_loaded(0));
    assert!(shared.main().host.is_loaded(1));
    assert_eq!(saved(&wrapper).sub_plugins.len(), 1);
    let mut orphaned = saved(&wrapper);
    orphaned.graph = Some(serde_json::to_value(Graph::new()).unwrap());

    assert!(shared.edit_graph(document, GraphEdit::Clear).unwrap());
    assert!(shared.patch().graph.is_empty());
    assert!(!shared.main().host.any_loaded());
    assert!(!shared.has_processors());
    assert!(saved(&wrapper).sub_plugins.is_empty());
    let clear = wrapper.wrapper_params().state.0.read().unwrap().clone();
    let mut block = Block::silent(128);
    block.fill(0.25).process(&mut wrapper, &mut Daw::playing());
    assert_eq!(block.peak(), 0.0);

    let document = shared.document_generation();
    shared.edit_graph(document, add(&path)).unwrap();
    shared.edit_graph(document, GraphEdit::Reset).unwrap();
    assert_eq!(shared.patch().graph, Graph::default_patch());
    assert!(!shared.main().host.any_loaded());
    assert!(saved(&wrapper).sub_plugins.is_empty());
    block.fill(0.25).process(&mut wrapper, &mut Daw::playing());
    assert_eq!(block.peak(), 0.25);

    *wrapper.wrapper_params().state.0.write().unwrap() = clear;
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .unwrap();
    assert!(shared.patch().graph.is_empty());
    block.fill(0.25).process(&mut wrapper, &mut Daw::playing());
    assert_eq!(block.peak(), 0.0);
    *wrapper.wrapper_params().state.0.write().unwrap() = serde_json::to_string(&orphaned).unwrap();
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .unwrap();
    assert!(shared.patch().graph.is_empty());
    assert!(!shared.main().host.any_loaded());
    assert!(saved(&wrapper).sub_plugins.is_empty());
    wrapper.deactivate();
}

/// An earlier document cannot enqueue a load, deletion, replacement, or save into its successor.
#[test]
fn stale_edits_cannot_affect_a_replacement_or_loaded_document() {
    let _thread = plugin_host::init_thread().unwrap();
    let mut wrapper = Wrapper::default();
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .unwrap();
    let shared = wrapper.shared().clone();
    let old = shared.document_generation();
    let path = fixture_as_clap("graph-stale-edits");
    shared.edit_graph(old, add(&path)).unwrap();
    let preset = wrapper.wrapper_params().state.0.read().unwrap().clone();
    let queued_path = path.clone();
    shared.post_main(move |shared| {
        assert!(!shared.edit_graph(old, add(&queued_path)).unwrap());
    });
    shared.edit_graph(old, GraphEdit::Clear).unwrap();
    shared.run_posted();
    for edit in [
        add(&path),
        GraphEdit::RemoveNode(0),
        GraphEdit::Reset,
        GraphEdit::Publish,
    ] {
        assert!(!shared.edit_graph(old, edit).unwrap());
        assert!(shared.patch().graph.is_empty());
        assert!(!shared.main().host.any_loaded());
    }
    let empty = shared.document_generation();
    *wrapper.wrapper_params().state.0.write().unwrap() = preset;
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .unwrap();
    let before = saved(&wrapper);
    for edit in [
        GraphEdit::Clear,
        GraphEdit::RemoveNode(2),
        add(&path),
        GraphEdit::Publish,
    ] {
        assert!(!shared.edit_graph(empty, edit).unwrap());
        assert!(shared.main().host.is_loaded(0));
        assert_eq!(
            serde_json::to_value(saved(&wrapper)).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
    }
    wrapper.deactivate();
}

/// Failed loads reserve their node's slot, and deleting unresolved nodes clears retained entries.
#[test]
fn failed_and_missing_children_share_the_same_ownership_rules() {
    let _thread = plugin_host::init_thread().unwrap();
    let mut wrapper = Wrapper::default();
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .unwrap();
    let shared = wrapper.shared().clone();
    let document = shared.document_generation();
    assert!(
        shared
            .edit_graph(
                document,
                add(std::path::Path::new("missing-graph-child.clap"))
            )
            .is_err()
    );
    let path = fixture_as_clap("graph-reserved-slots");
    shared.edit_graph(document, add(&path)).unwrap();
    assert!(!shared.main().host.is_loaded(0));
    assert!(shared.main().host.is_loaded(1));

    let mut state = saved(&wrapper);
    state.sub_plugins[0].reference.format = "unavailable-format".into();
    *wrapper.wrapper_params().state.0.write().unwrap() = serde_json::to_string(&state).unwrap();
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .unwrap();
    let missing = shared
        .patch()
        .graph
        .nodes
        .iter()
        .find_map(|node| {
            matches!(&node.kind, NodeKind::Plugin(Plugin { instance: 1, .. })).then_some(node.id)
        })
        .unwrap();
    assert!(shared.main().host.reference(1).is_some());
    let document = shared.document_generation();
    shared
        .edit_graph(document, GraphEdit::RemoveNode(missing))
        .unwrap();
    assert!(shared.main().host.reference(1).is_none());
    assert!(saved(&wrapper).sub_plugins.is_empty());

    let duplicate = shared.patch().graph.add(
        NodeKind::Plugin(Plugin {
            instance: 0,
            ports: PluginPorts::default(),
        }),
        [0.0; 2],
    );
    shared.load_into(0, &path).unwrap();
    shared
        .edit_graph(document, GraphEdit::RemoveNode(duplicate))
        .unwrap();
    assert!(
        shared.main().host.is_loaded(0),
        "the other node still owns instance zero"
    );
    shared.edit_graph(document, GraphEdit::Clear).unwrap();
    assert!(saved(&wrapper).sub_plugins.is_empty());
    wrapper.deactivate();
}
