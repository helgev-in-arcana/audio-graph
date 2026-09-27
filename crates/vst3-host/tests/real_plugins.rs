//! Tests that need an actual VST3 plugin on the machine.
//!
//! Which plugins is the developer's to say, in `AUDIO_GRAPH_TEST_PLUGINS` (see
//! `.env.example` at the repository root). Picking from whatever is installed
//! would make the result depend on the machine, and would need a list of that
//! machine's troublesome plugins kept in the repository. Unset, these skip, so
//! `cargo test` stays green on a bare CI box.

use std::sync::{Mutex, MutexGuard, PoisonError};

use std::path::PathBuf;

use vst3_host::Module;

/// Serialises the tests that open installed modules.
///
/// A module belongs to one thread at a time, and the harness runs each test on
/// a thread of its own: two tests opening the same module at once would see
/// `ModuleBusy`, which is the host working as designed rather than a failure.
fn installed() -> MutexGuard<'static, ()> {
    static INSTALLED: Mutex<()> = Mutex::new(());
    INSTALLED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The VST3 modules named in `AUDIO_GRAPH_TEST_PLUGINS`.
fn listed_modules() -> Vec<PathBuf> {
    let _ = dotenvy::dotenv();
    std::env::var_os("AUDIO_GRAPH_TEST_PLUGINS")
        .map(|list| std::env::split_paths(&list).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|path| {
            path.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("vst3"))
        })
        .collect()
}

#[test]
fn every_listed_module_loads_and_enumerates() {
    let _installed = installed();
    let _thread = vst3_host::init_apartment().unwrap();
    let modules = listed_modules();
    if modules.is_empty() {
        eprintln!("no VST3 plugin in AUDIO_GRAPH_TEST_PLUGINS; skipping");
        return;
    }

    let mut failures = Vec::new();
    let mut audio_classes = 0;

    for path in &modules {
        match Module::open(path) {
            Ok(module) => match module.classes() {
                Ok(classes) => {
                    // Every class must carry an identity that can be persisted
                    // and resolved by CID.
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
    let _installed = installed();
    let _thread = vst3_host::init_apartment().unwrap();
    let Some(path) = listed_modules().into_iter().next() else {
        eprintln!("no VST3 plugin in AUDIO_GRAPH_TEST_PLUGINS; skipping");
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
    let _thread = vst3_host::init_apartment().unwrap();
    let err = Module::open("does-not-exist.vst3").unwrap_err();
    assert!(matches!(err, plugin_host_api::HostError::ModuleLoad(_)));
}
