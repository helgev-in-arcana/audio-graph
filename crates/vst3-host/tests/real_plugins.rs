//! Tests that need an actual VST3 plugin on the machine.
//!
//! They discover plugins through the OS-conventional directories and skip
//! themselves when there are none, so `cargo test` stays green on a bare CI
//! box while still doing real work on a developer machine.

use vst3_host::{Module, default_plugin_directories, find_modules};

fn installed_modules() -> Vec<std::path::PathBuf> {
    default_plugin_directories()
        .iter()
        .flat_map(|d| find_modules(d))
        .collect()
}

#[test]
fn every_installed_module_loads_and_enumerates() {
    let modules = installed_modules();
    if modules.is_empty() {
        eprintln!("no VST3 plugins installed; skipping");
        return;
    }

    let mut failures = Vec::new();
    let mut audio_classes = 0;

    for path in &modules {
        match Module::open(path) {
            Ok(module) => match module.classes() {
                Ok(classes) => {
                    // Every class must carry an identity we can persist and
                    // find again, since §8.3 binds slots by CID.
                    for c in &classes {
                        assert_eq!(
                            vst3_host::Cid::from_hex(&c.cid.to_hex()),
                            Some(c.cid),
                            "{}: CID does not round-trip through its string form",
                            path.display()
                        );
                        assert!(!c.category.is_empty(), "{}: empty category", path.display());
                    }
                    audio_classes += classes.iter().filter(|c| c.is_audio_module()).count();
                }
                Err(e) => failures.push(format!("{}: {e}", path.display())),
            },
            Err(e) => failures.push(format!("{}: {e}", path.display())),
        }
    }

    assert!(
        failures.is_empty(),
        "modules failed to load:\n{}",
        failures.join("\n")
    );
    assert!(
        audio_classes > 0,
        "no audio module classes found across {} modules",
        modules.len()
    );
}

#[test]
fn repeated_load_unload_is_stable() {
    let Some(path) = installed_modules().into_iter().next() else {
        eprintln!("no VST3 plugins installed; skipping");
        return;
    };

    // The exit function must run only after the factory pointer is released;
    // if that order is wrong, a plugin that frees global state on exit tends to
    // fault within a handful of cycles rather than at some later point.
    let first = {
        let m = Module::open(&path).expect("first load");
        m.classes().expect("first enumerate")
    };

    for i in 0..50 {
        let Ok(m) = Module::open(&path) else {
            panic!("cycle {i}: load failed")
        };
        let classes = m.classes().unwrap_or_else(|e| panic!("cycle {i}: {e}"));
        assert_eq!(
            classes, first,
            "cycle {i}: class list changed across reloads"
        );
    }
}

#[test]
fn a_missing_path_is_an_error_not_a_panic() {
    let err = Module::open("does-not-exist.vst3").unwrap_err();
    assert!(matches!(err, plugin_host_api::HostError::ModuleLoad(_)));
}
