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
    plugin_host::init_thread();
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
