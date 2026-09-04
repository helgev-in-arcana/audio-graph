//! A project or a preset the DAW hands over while the wrapper is running.
//!
//! nice-plug answers a state load by calling `activate` again rather than
//! deactivating first, so an activation is where a project arrives — both the
//! one that opens a session and the one dropped on a patch that is already
//! playing. Driven against `clap-test-plugin`, because the path that breaks is
//! the one taken only when a sub-plugin is already loaded.

use std::path::PathBuf;

use audio_graph_plugin::{Wrapper, WrapperKind};
use nice_plug::prelude::*;

/// Locates the built CLAP test fixture and copies it under a `.clap` name.
///
/// The facade infers the format from the extension and cargo's artefact is
/// named `.dll`, so it is copied rather than renamed: the original belongs to
/// cargo and the next build would replace it anyway.
///
/// Panics when the fixture is missing rather than skipping, because a skip
/// would make a green run mean nothing.
fn fixture_as_clap() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary has a path");
    let build_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the test binary is two levels below the build directory");
    let source = [
        "clap_test_plugin.dll",
        "libclap_test_plugin.so",
        "libclap_test_plugin.dylib",
    ]
    .iter()
    .map(|n| build_dir.join(n))
    .find(|p| p.is_file())
    .unwrap_or_else(|| {
        panic!(
            "clap-test-plugin is not in {}.\n\
             Run `cargo build --workspace` before `cargo test --workspace`: \
             cargo does not build another package's cdylib on its own.",
            build_dir.display()
        )
    });

    // A distinct name per test binary, so two of them cannot copy over each
    // other's file while the other has it loaded.
    let target = build_dir.join("state-reload-fixture.clap");
    std::fs::copy(&source, &target).expect("the fixture can be copied");
    target
}

const STEREO: NonZeroU32 = new_nonzero_u32(2);

/// The effect form's layout, as the DAW hands it over.
fn fx_layout() -> AudioIOLayout {
    AudioIOLayout {
        main_input_channels: Some(STEREO),
        main_output_channels: Some(STEREO),
        aux_input_ports: &[STEREO],
        ..AudioIOLayout::const_default()
    }
}

const LIVE: BufferConfig = BufferConfig {
    sample_rate: 48_000.0,
    min_buffer_size: None,
    max_buffer_size: 512,
    process_mode: ProcessMode::Realtime,
};

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
        .load(&fixture_as_clap())
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
