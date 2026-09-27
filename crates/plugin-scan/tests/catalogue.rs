//! Tests for the plugin catalogue cache storage, stamping, and invalidation.
//!
//! Verifies timestamp and size calculation for files and directory bundles,
//! JSON serialization and persistence, and resilience against corrupted cache
//! files.
//!
//! Real modules are not scanned here: there may be none on the machine running
//! this, and opening whatever is installed is exactly what a test must not do.
//! What is tested is everything around the scan.
//!
use std::path::PathBuf;

use plugin_scan::catalogue;

/// A busy native module retains old metadata and remains eligible for a later scan.
#[test]
fn busy_modules_are_not_cached_as_broken() {
    let profile = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    let source = profile.join(format!(
        "{}clap_test_plugin{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = profile.join(format!("catalogue-busy-{}-{unique}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();
    let binary = dir.join("fixture.clap");
    std::fs::copy(source, &binary).unwrap();
    let cache = dir.join("cache.json");
    let scan = || {
        std::thread::scope(|scope| {
            scope
                .spawn(|| catalogue::refresh(std::slice::from_ref(&dir), Some(&cache)))
                .join()
                .unwrap()
        })
    };
    let owner = clap_host::Module::open(&binary).unwrap();
    assert!(scan().is_empty());
    assert!(catalogue::cached(&cache).is_empty());
    drop(owner);
    assert_eq!(scan().len(), 1);
    let mut saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&cache).unwrap()).unwrap();
    saved["modules"][0]["stamp"]["size"] = 0.into();
    std::fs::write(&cache, serde_json::to_vec(&saved).unwrap()).unwrap();
    let owner = clap_host::Module::open(&binary).unwrap();
    let deferred = scan();
    assert_eq!(deferred.len(), 1);
    assert!(deferred[0].error.is_none());
    assert_eq!(deferred[0].stamp.size, 0);
    drop(owner);
    assert!(scan()[0].stamp.size > 0);
    std::fs::remove_file(binary).unwrap();
    std::fs::remove_file(cache).unwrap();
    std::fs::remove_dir(dir).unwrap();
}

/// Cache persistence and invalidation do not depend on a product settings location.
#[test]
fn the_cache_is_stamped_stored_at_the_chosen_path_and_survives_being_lost() {
    let dir =
        std::env::temp_dir().join(format!("plugin-host-catalogue-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temp directory can be made");
    let config = dir.join("config.json");

    // Independent of settings: a corrupt cache must not be able
    // to take the user's plugin folders with it.
    let path: PathBuf = dir.join("catalogues").join("inventory.json");
    assert_eq!(path, dir.join("catalogues").join("inventory.json"));
    assert_ne!(path, config);

    // Nothing written yet. Not an error — the cache is derived data.
    assert!(
        catalogue::cached(&path).is_empty(),
        "no file means nothing known"
    );

    // A file's stamp is its own; a bundle's is the newest thing inside it,
    // which is the whole point: an installer replacing a binary two levels
    // down does not touch the directory's own mtime.
    let file = dir.join("Plain.clap");
    std::fs::write(&file, b"not really a plugin").expect("the directory is writable");
    let file_stamp = catalogue::stamp_of(&file);
    assert_eq!(file_stamp.size, 19, "a file's stamp is its own size");
    assert!(file_stamp.modified > 0);

    let bundle = dir.join("Bundle.vst3");
    let inner = bundle.join("Contents").join("x86_64-win");
    std::fs::create_dir_all(&inner).expect("the directory is writable");
    let binary = inner.join("Bundle.vst3");
    std::fs::write(&binary, b"abc").expect("the directory is writable");
    let before = catalogue::stamp_of(&bundle);
    assert_eq!(before.size, 3, "a bundle's stamp sums what is inside it");

    std::fs::write(&binary, b"abcdef").expect("the directory is writable");
    let after = catalogue::stamp_of(&bundle);
    assert_ne!(
        before, after,
        "replacing a file inside a bundle invalidates it"
    );

    assert_eq!(
        catalogue::stamp_of(&dir.join("nothing-here.clap")),
        catalogue::stamp_of(&dir.join("nor-here.clap")),
        "a stamp that cannot be taken is not mistaken for a change"
    );

    // A refresh over a scan list that finds nothing still writes the file, so
    // that "nothing installed" is an answer rather than an unanswered
    // question.
    std::fs::write(&config, r#"{"directories":[]}"#).expect("a temp profile is writable");
    assert!(
        catalogue::refresh(&[], Some(&path)).is_empty(),
        "no folders, no modules"
    );
    assert!(path.is_file(), "and the answer is written down");

    // A cache we cannot parse is an empty one, never a crash and never a
    // reason to touch the settings.
    std::fs::write(&path, "{ this is not json").expect("the file is writable");
    assert!(catalogue::cached(&path).is_empty());
    assert!(config.is_file(), "and the settings are still there");

    // Forgetting is how "Rescan" gets everything opened again, and forgetting
    // twice is not an error.
    catalogue::forget(&path).expect("a temp profile is writable");
    assert!(!path.exists());
    catalogue::forget(&path).expect("forgetting nothing is fine");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A temporary directory of empty files standing in for modules.
///
/// `reference_candidates` never opens a module, so a file that exists is all a
/// candidate needs to be.
fn stand_ins(name: &str, files: &[&str]) -> (PathBuf, Vec<PathBuf>) {
    let dir = std::env::temp_dir().join(format!(
        "plugin-host-candidates-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let paths = files
        .iter()
        .map(|file| {
            let path = dir.join(file);
            std::fs::write(&path, b"module").unwrap();
            path
        })
        .collect();
    (dir, paths)
}

fn module(path: &std::path::Path, format: plugin_host::Format, ids: &[&str]) -> catalogue::Module {
    catalogue::Module {
        path: path.to_path_buf(),
        format,
        stamp: catalogue::stamp_of(path),
        classes: ids
            .iter()
            .map(|id| catalogue::Class {
                id: (*id).into(),
                name: (*id).into(),
                category: String::new(),
                is_instrument: false,
            })
            .collect(),
        error: None,
    }
}

fn reference(format: plugin_host::Format, id: &str, hint: PathBuf) -> plugin_host::PluginRef {
    plugin_host::PluginRef {
        format,
        id: id.into(),
        path_hint: hint,
        display_name: id.into(),
    }
}

/// The saved path is tried first while it exists, and the catalogue's modules
/// that export the id after it — so a project on the machine it was saved on
/// loads what it loaded before.
#[test]
fn the_hint_comes_before_the_catalogue() {
    use plugin_host::Format;
    let (_dir, paths) = stand_ins("hint-first", &["saved.clap", "elsewhere.clap"]);
    let known = [module(&paths[1], Format::Clap, &["com.example.a"])];
    let wanted = reference(Format::Clap, "com.example.a", paths[0].clone());
    assert_eq!(
        plugin_scan::reference_candidates(&wanted, &known),
        vec![paths[0].clone(), paths[1].clone()]
    );
}

/// A plugin that moved is found through the catalogue by its id alone, and
/// only among modules of its own format that export that id.
#[test]
fn a_moved_plugin_is_found_by_its_id() {
    use plugin_host::Format;
    let (dir, paths) = stand_ins(
        "moved",
        &["other.clap", "moved.clap", "moved.vst3", "broken.clap"],
    );
    let mut broken = module(&paths[3], Format::Clap, &["com.example.a"]);
    broken.error = Some("would not open".into());
    let known = [
        module(&paths[0], Format::Clap, &["com.example.b"]),
        module(&paths[1], Format::Clap, &["com.example.b", "com.example.a"]),
        module(&paths[2], Format::Vst3, &["com.example.a"]),
        broken,
    ];
    let wanted = reference(Format::Clap, "com.example.a", dir.join("gone.clap"));
    assert_eq!(
        plugin_scan::reference_candidates(&wanted, &known),
        vec![paths[1].clone()]
    );
}

/// A module the catalogue has not seen is not a candidate, even one sitting
/// next to the saved path: finding it would mean opening modules to ask, on
/// the thread restoring the project.
#[test]
fn nothing_outside_the_catalogue_is_a_candidate() {
    use plugin_host::Format;
    let (dir, _paths) = stand_ins("unscanned", &["unscanned.clap"]);
    let wanted = reference(Format::Clap, "com.example.a", dir.join("gone.clap"));
    assert!(plugin_scan::reference_candidates(&wanted, &[]).is_empty());
}

/// A module whose file changed since it was scanned may no longer export the
/// id, so it is tried after the ones the catalogue still vouches for.
#[test]
fn a_changed_module_is_tried_last() {
    use plugin_host::Format;
    let (dir, paths) = stand_ins("changed", &["changed.clap", "unchanged.clap"]);
    let mut changed = module(&paths[0], Format::Clap, &["com.example.a"]);
    changed.stamp.size += 1;
    let known = [changed, module(&paths[1], Format::Clap, &["com.example.a"])];
    let wanted = reference(Format::Clap, "com.example.a", dir.join("gone.clap"));
    assert_eq!(
        plugin_scan::reference_candidates(&wanted, &known),
        vec![paths[1].clone(), paths[0].clone()]
    );
}
