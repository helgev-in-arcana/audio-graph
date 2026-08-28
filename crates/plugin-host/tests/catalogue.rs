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
//! This file holds exactly one `#[test]`, and that is load-bearing: the test
//! sets `AUDIO_GRAPH_CONFIG` with `std::env::set_var`, which is only sound
//! while no other thread is running. Cargo builds each integration-test file
//! as its own binary but runs the tests within one file on parallel threads,
//! so a second test added here would make the `set_var` below unsound. Add
//! further catalogue tests as a new file instead.

use std::path::PathBuf;

use plugin_host::catalogue;

#[test]
fn the_cache_is_stamped_written_beside_the_settings_and_survives_being_lost() {
    let dir = std::env::temp_dir().join("audio-graph-catalogue-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temp directory can be made");
    let config = dir.join("config.json");

    // SAFETY: no other thread is running. This is the only test in this
    // binary (see the module comment), and the variable is set before
    // anything here reads the config.
    unsafe { std::env::set_var("AUDIO_GRAPH_CONFIG", &config) };

    // Beside the settings, not inside them: a corrupt cache must not be able
    // to take the user's plugin folders with it.
    let path: PathBuf = catalogue::cache_path().expect("the override names a directory");
    assert_eq!(path, dir.join("plugins.json"));
    assert_ne!(path, config);

    // Nothing written yet. Not an error — the cache is derived data.
    assert!(
        catalogue::cached().is_empty(),
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
    plugin_host::config::store(&plugin_host::config::Config::default())
        .expect("a temp profile is writable");
    assert!(catalogue::refresh().is_empty(), "no folders, no modules");
    assert!(path.is_file(), "and the answer is written down");

    // A cache we cannot parse is an empty one, never a crash and never a
    // reason to touch the settings.
    std::fs::write(&path, "{ this is not json").expect("the file is writable");
    assert!(catalogue::cached().is_empty());
    assert!(config.is_file(), "and the settings are still there");

    // Forgetting is how "Rescan" gets everything opened again, and forgetting
    // twice is not an error.
    catalogue::forget().expect("a temp profile is writable");
    assert!(!path.exists());
    catalogue::forget().expect("forgetting nothing is fine");

    let _ = std::fs::remove_dir_all(&dir);
}
