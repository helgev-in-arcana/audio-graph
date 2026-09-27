//! Integration tests for the unified plugin host facade.
//!
//! The CLAP half runs everywhere, because the fixture is built from this
//! workspace. The VST3 half runs against a VST3 the developer names in
//! `AUDIO_GRAPH_TEST_PLUGINS` (see `.env.example` at the repository root) and
//! skips itself when none is named, the same convention `vst3-host`'s own
//! tests use.

use std::path::PathBuf;
use std::sync::Arc;

use plugin_host::{Format, HostContext, Plugin, RestartReason, SubPluginMain, scan_module};

#[derive(Default)]
struct TestHost;

impl HostContext for TestHost {
    fn host_name(&self) -> &str {
        "plugin-host tests"
    }
    fn request_restart(&self, _reason: RestartReason) {}
}

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
    let target = build_dir.join("facade-fixture.clap");
    std::fs::copy(&source, &target).expect("the fixture can be copied");
    target
}

#[test]
fn the_facade_loads_a_clap_by_path_alone() {
    let _thread = plugin_host::init_thread().unwrap();
    let path = fixture_as_clap();

    // The extension is the only thing that says which backend answers.
    let classes = scan_module(&path).expect("scans");
    assert_eq!(classes.len(), 1);
    let class = classes[0].clone();
    assert_eq!(class.format, Format::Clap);
    assert_eq!(class.id, "dev.audio-graph.clap-test-plugin");
    assert!(!class.is_instrument);
    assert!(class.category.contains("audio-effect"));

    let mut plugin =
        Plugin::load(&path, Some(&class.id), Arc::new(TestHost)).expect("loads through the facade");
    assert_eq!(plugin.format(), Format::Clap);
    assert_eq!(SubPluginMain::params(&plugin).len(), 8);
    assert_eq!(SubPluginMain::io_layout(&plugin).inputs.len(), 2);
    // A tick with no editor open must still be safe, since the caller is told
    // to call it every frame regardless: CLAP's timers and main-thread
    // callbacks run whether or not anything is on screen.
    plugin.tick();

    assert!(plugin.has_editor());
    #[cfg(windows)]
    {
        // The whole point of the facade: the same three calls, whichever
        // format answered.
        plugin.open_editor(std::ptr::null_mut()).expect("opens");
        assert!(plugin.editor_is_open());
        plugin.tick();
        plugin.close_editor();
        assert!(!plugin.editor_is_open());
    }

    // The saved form round-trips back to the same file.
    let reference = plugin.reference();
    assert_eq!(reference.format, Format::Clap);
    assert_eq!(reference.path_hint, path);

    drop(plugin);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_facade_loads_a_listed_vst3() {
    let _thread = plugin_host::init_thread().unwrap();

    // First module that yields a class. A module may be a wrapper around a
    // scanner that exports nothing loadable, so this is a search rather than a
    // first-hit assertion.
    let _ = dotenvy::dotenv();
    let found = std::env::var_os("AUDIO_GRAPH_TEST_PLUGINS")
        .map(|list| std::env::split_paths(&list).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|path| Format::from_path(path) == Some(Format::Vst3))
        .find_map(|path| {
            let classes = scan_module(&path).ok()?;
            classes.into_iter().next()
        });

    let Some(class) = found else {
        eprintln!("no VST3 plugin in AUDIO_GRAPH_TEST_PLUGINS; skipping");
        return;
    };

    assert_eq!(class.format, Format::Vst3);
    let plugin = Plugin::load(&class.path, Some(&class.id), Arc::new(TestHost))
        .expect("loads through the facade");
    assert_eq!(plugin.format(), Format::Vst3);
    assert_eq!(plugin.name(), class.name);
    // Both backends answer the same question the same way; that is the
    // facade's whole job.
    let _ = SubPluginMain::io_layout(&plugin);
    let _ = SubPluginMain::capabilities(&plugin);
}
