//! The facade, driven against whatever is actually on the machine.
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

fn fixture_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let build_dir = exe.parent()?.parent()?;
    [
        "clap_test_plugin.dll",
        "libclap_test_plugin.so",
        "libclap_test_plugin.dylib",
    ]
    .iter()
    .map(|n| build_dir.join(n))
    .find(|p| p.is_file())
}

/// The fixture with a `.clap` extension, since the facade infers the format
/// from the path and a build artefact is named `.dll`.
fn fixture_as_clap() -> Option<PathBuf> {
    let source = fixture_path()?;
    let target = source.with_extension("clap");
    // Copied rather than renamed: the original is cargo's artefact and the next
    // build would replace it anyway.
    std::fs::copy(&source, &target).ok()?;
    Some(target)
}

#[test]
fn the_facade_loads_a_clap_by_path_alone() {
    plugin_host::init_thread();
    let Some(path) = fixture_as_clap() else {
        eprintln!("clap-test-plugin has not been built; skipping");
        return;
    };

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
    assert_eq!(SubPluginMain::params(&plugin).len(), 4);
    assert_eq!(SubPluginMain::io_layout(&plugin).inputs.len(), 2);
    assert!(!plugin.has_editor(), "the fixture has no gui extension");

    // A tick with no editor open must still be safe, since the caller is told
    // to call it every frame regardless.
    plugin.tick();

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
    // Both backends answer the same question the same way; that is the facade's
    // whole job.
    let _ = SubPluginMain::io_layout(&plugin);
    let _ = SubPluginMain::capabilities(&plugin);
}
