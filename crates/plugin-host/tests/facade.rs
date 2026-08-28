//! Integration tests for the unified plugin host facade, driven against
//! whatever is actually on the machine.
//!
//! The CLAP half runs everywhere, because the fixture is built from this
//! workspace. The VST3 half skips itself when no plugin is installed, which is
//! the same convention `vst3-host`'s own tests use.

use std::path::PathBuf;
use std::sync::Arc;

use plugin_host::{
    Format, HostContext, Plugin, RestartReason, SubPluginMain, resolve_reference, scan_module,
};

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
    plugin_host::init_thread();
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
    assert_eq!(SubPluginMain::params(&plugin).len(), 7);
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
    assert_eq!(
        resolve_reference(&reference).as_deref(),
        Some(path.as_path())
    );

    drop(plugin);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_facade_loads_an_installed_vst3() {
    plugin_host::init_thread();

    // First module that yields a class. Some installed plugins are wrappers
    // around a scanner and export nothing loadable, so this is a search rather
    // than a first-hit assertion.
    let found = plugin_host::installed_modules()
        .into_iter()
        .filter(|(format, _)| *format == Format::Vst3)
        // Known to corrupt its own heap on teardown, and excluded from
        // `vst3-host`'s tests for the same reason.
        .filter(|(_, path)| !path.ends_with("OTT.vst3"))
        .take(8)
        .find_map(|(_, path)| {
            let classes = scan_module(&path).ok()?;
            classes.into_iter().next()
        });

    let Some(class) = found else {
        eprintln!("no VST3 plugins installed; skipping");
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
