use audio_graph_engine::{
    AudioIn, AudioOut, Constant, Graph, NodeId, NodeKind, ParamPort, Plugin, PluginPorts,
};

fn plugin(graph: &mut Graph, ports: PluginPorts) -> NodeId {
    graph.add(NodeKind::Plugin(Plugin { instance: 0, ports }), [0.0; 2])
}

fn layout(inputs: usize, outputs: usize, notes: bool) -> PluginPorts {
    PluginPorts {
        audio_in: vec![2; inputs],
        audio_out: vec![2; outputs],
        audio_out_shown: vec![0],
        accepts_notes: notes,
        ..PluginPorts::default()
    }
}

/// Native bus and note changes cannot retarget a wire to a different parameter row.
#[test]
fn inputs_follow_their_roles_through_layout_changes() {
    let mut graph = Graph::new();
    let mut ports = layout(1, 1, true);
    ports.params = vec![
        ParamPort {
            id: 7,
            name: "first".into(),
        },
        ParamPort {
            id: 7,
            name: "second".into(),
        },
    ];
    let child = plugin(&mut graph, ports);
    let audio = graph.add(
        NodeKind::AudioIn(AudioIn {
            bus: 0,
            channels: 2,
        }),
        [0.0; 2],
    );
    let notes = graph.add(NodeKind::NoteIn, [0.0; 2]);
    let a = graph.add(NodeKind::Constant(Constant { value: 0.25 }), [0.0; 2]);
    let b = graph.add(NodeKind::Constant(Constant { value: 0.75 }), [0.0; 2]);
    graph.connect(audio, 0, child, 0);
    graph.connect(notes, 0, child, 1);
    graph.connect(a, 0, child, 2);
    graph.connect(b, 0, child, 3);

    graph.update_plugin_ports(child, layout(3, 1, false));
    assert_eq!(graph.source_of(child, 0), Some((audio, 0)));
    assert_eq!(graph.source_of(child, 3), Some((a, 0)));
    assert_eq!(graph.source_of(child, 4), Some((b, 0)));
    assert_eq!(graph.pending_link_count(child), 1);

    graph.update_plugin_ports(child, layout(0, 1, false));
    assert_eq!(graph.source_of(child, 0), Some((a, 0)));
    assert_eq!(graph.source_of(child, 1), Some((b, 0)));
    assert_eq!(graph.pending_link_count(child), 2);
    graph = serde_json::from_str(&serde_json::to_string(&graph).unwrap()).unwrap();
    graph.prune();
    graph.update_plugin_ports(child, layout(1, 1, true));
    for (port, source) in [audio, notes, a, b].into_iter().enumerate() {
        assert_eq!(graph.source_of(child, port as u8), Some((source, 0)));
    }
    assert_eq!(graph.pending_link_count(child), 0);
}

/// Both ends can disappear independently and recover in either order after a save.
#[test]
fn missing_buses_recover_without_repointing_surviving_outputs() {
    for output_first in [false, true] {
        let mut graph = Graph::new();
        let mut ports = layout(0, 3, false);
        ports.audio_out_shown = vec![2, 0];
        let source = plugin(&mut graph, ports);
        let sink = plugin(&mut graph, layout(2, 1, false));
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [0.0; 2],
        );
        graph.connect(source, 0, sink, 1);
        graph.connect(source, 1, out, 0);
        graph.update_plugin_ports(source, layout(0, 1, false));
        assert_eq!(graph.source_of(out, 0), Some((source, 0)));
        graph.update_plugin_ports(sink, layout(0, 1, false));
        graph = serde_json::from_str(&serde_json::to_string(&graph).unwrap()).unwrap();
        graph.prune();
        let changes = [(source, layout(0, 3, false)), (sink, layout(2, 1, false))];
        for index in if output_first { [0, 1] } else { [1, 0] } {
            graph.update_plugin_ports(changes[index].0, changes[index].1.clone());
        }
        assert_eq!(graph.source_of(sink, 1), Some((source, 0)));
        assert_eq!(graph.source_of(out, 0), Some((source, 1)));
        assert_eq!(graph.pending_link_count(source), 0);
    }
}

/// Replacing, deleting, or disconnecting the surviving end cancels a pending wire.
#[test]
fn user_edits_cancel_pending_connections() {
    for action in 0..4 {
        let mut graph = Graph::new();
        let source = plugin(&mut graph, layout(0, 1, false));
        let sink = plugin(&mut graph, layout(1, 1, false));
        graph.connect(source, 0, sink, 0);
        graph.update_plugin_ports(source, layout(0, 0, false));
        assert_eq!(graph.pending_link_count(source), 1);
        match action {
            0 => graph.disconnect(sink, 0),
            1 => graph.discard_pending_links(sink),
            2 => graph.remove(sink),
            _ => {
                let new = plugin(&mut graph, layout(0, 1, false));
                graph.connect(new, 0, sink, 0);
            }
        }
        graph.update_plugin_ports(source, layout(0, 1, false));
        assert_eq!(graph.pending_link_count(source), 0);
        assert!(graph.links.iter().all(|link| link.from != source));
    }
}

/// Removing visible sockets does not remove a different socket hidden by native metadata.
#[cfg(feature = "ui")]
#[test]
fn removing_sockets_updates_pending_endpoints() {
    let mut graph = Graph::new();
    let source = graph.add(NodeKind::Constant(Constant { value: 0.5 }), [0.0; 2]);
    let mut ports = layout(0, 1, false);
    ports.params = (0..2)
        .map(|id| ParamPort {
            id,
            name: String::new(),
        })
        .collect();
    let sink = plugin(&mut graph, ports);
    graph.connect(source, 0, sink, 1);
    let notes = graph.add(NodeKind::NoteIn, [0.0; 2]);
    graph.update_plugin_ports(sink, layout(0, 1, true));
    graph.connect(notes, 0, sink, 0);
    graph.update_plugin_ports(sink, layout(0, 1, false));
    assert_eq!(graph.node_mut(sink).unwrap().kind.remove_input(0), 1);
    graph.drop_inputs(sink, 0, 1);
    graph.update_plugin_ports(sink, layout(0, 1, true));
    assert_eq!(graph.source_of(sink, 0), Some((notes, 0)));
    assert_eq!(graph.source_of(sink, 1), Some((source, 0)));

    let mut ports = layout(0, 3, false);
    ports.audio_out_shown = vec![2, 0, 1];
    let child = plugin(&mut graph, ports);
    graph.update_plugin_ports(child, layout(0, 2, false));
    assert_eq!(graph.node_mut(child).unwrap().kind.remove_output(0), 1);
    graph.drop_outputs(child, 0, 1);
    let NodeKind::Plugin(plugin) = &graph.node(child).unwrap().kind else {
        unreachable!()
    };
    assert_eq!(plugin.ports.audio_out_shown, vec![2, 1]);
}

/// A pending wire follows socket removal at its available end, just like an active wire.
#[cfg(feature = "ui")]
#[test]
fn pending_wires_follow_socket_removal_at_either_end() {
    use audio_graph_engine::Mix;
    let mut graph = Graph::new();
    let source = plugin(&mut graph, layout(0, 1, false));
    let sink = graph.add(
        NodeKind::Mix(Mix {
            channels: 2,
            inputs: 2,
            gains: vec![0.0; 2],
        }),
        [0.0; 2],
    );
    graph.connect(source, 0, sink, 2);
    graph.update_plugin_ports(source, layout(0, 0, false));
    assert_eq!(graph.pending_link_count(source), 1);
    assert_eq!(graph.node_mut(sink).unwrap().kind.remove_input(0), 2);
    graph.drop_inputs(sink, 0, 2);
    graph.update_plugin_ports(source, layout(0, 1, false));
    assert_eq!(graph.source_of(sink, 0), Some((source, 0)));

    let mut ports = layout(0, 2, false);
    ports.audio_out_shown = vec![0, 1];
    let source = plugin(&mut graph, ports);
    let sink = plugin(&mut graph, layout(1, 1, false));
    graph.connect(source, 1, sink, 0);
    graph.update_plugin_ports(sink, layout(0, 1, false));
    assert_eq!(graph.node_mut(source).unwrap().kind.remove_output(0), 1);
    graph.drop_outputs(source, 0, 1);
    graph.update_plugin_ports(sink, layout(1, 1, false));
    assert_eq!(graph.source_of(sink, 0), Some((source, 0)));
}
