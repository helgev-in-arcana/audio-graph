//! What can be said about loading a module without loading one: where its
//! binary is, which folders hold modules, and who may hold a binary.

use std::path::{Path, PathBuf};

use plugin_host_api::HostError;
use plugin_module::{ModuleLease, bundle_binary, find_modules, identity};

/// A fresh folder for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("plugin-module-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn touch(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"binary").unwrap();
}

/// A plain file is its own binary; a bundle's is the one named after it, or
/// failing that the only file where the format keeps it.
#[test]
fn a_bundle_is_resolved_to_the_binary_inside_it() {
    let dir = scratch("bundle");
    let plain = dir.join("Plain.clap");
    touch(&plain);
    assert_eq!(bundle_binary(&plain, "MacOS", "").unwrap(), plain);

    let named = dir.join("Named.vst3");
    touch(&named.join("Contents/x86_64-win/Named.vst3"));
    touch(&named.join("Contents/x86_64-win/Other.vst3"));
    assert_eq!(
        bundle_binary(&named, "x86_64-win", "vst3").unwrap(),
        named.join("Contents/x86_64-win/Named.vst3")
    );

    let renamed = dir.join("Renamed.vst3");
    touch(&renamed.join("Contents/x86_64-win/Inner.vst3"));
    assert_eq!(
        bundle_binary(&renamed, "x86_64-win", "vst3").unwrap(),
        renamed.join("Contents/x86_64-win/Inner.vst3")
    );

    assert!(matches!(
        bundle_binary(&dir.join("Missing.vst3"), "x86_64-win", "vst3"),
        Err(HostError::ModuleLoad(_))
    ));
}

/// Modules are found in a folder and one level down, and a bundle is a module
/// rather than a folder to look inside.
#[test]
fn modules_are_found_one_level_down_but_not_inside_bundles() {
    let dir = scratch("find");
    touch(&dir.join("A.vst3"));
    touch(&dir.join("Vendor/B.vst3"));
    touch(&dir.join("Vendor/Deeper/C.vst3"));
    touch(&dir.join("Bundle.vst3/Contents/x86_64-win/Bundle.vst3"));
    touch(&dir.join("D.clap"));

    let found = find_modules(&dir, "vst3");
    assert_eq!(
        found,
        vec![
            dir.join("A.vst3"),
            dir.join("Bundle.vst3"),
            dir.join("Vendor/B.vst3")
        ]
    );
}

/// One binary belongs to one holder at a time, and is free again once let go.
#[test]
fn a_binary_is_leased_to_one_holder_at_a_time() {
    let dir = scratch("lease");
    let binary = dir.join("Held.clap");
    touch(&binary);
    let key = identity(binary.clone());

    let held = ModuleLease::acquire(key.clone()).unwrap();
    assert!(matches!(
        ModuleLease::acquire(identity(binary.clone())),
        Err(HostError::ModuleBusy(_))
    ));
    drop(held);
    assert!(ModuleLease::acquire(key).is_ok());
}
