//! The plugin folders: seeded, edited, re-read, and reaching a scan.
//!
//! One test function on purpose. The config is process-wide state behind a
//! single cache, and `AUDIO_GRAPH_CONFIG` — the override that keeps this off
//! the real profile of whoever runs the suite — is process-wide too. A second
//! `#[test]` in this binary would run beside this one and they would fight over
//! both.

use std::path::PathBuf;

use plugin_host::config;

#[test]
fn folders_are_seeded_saved_reread_and_scanned() {
    let dir = std::env::temp_dir().join("audio-graph-config-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temp directory can be made");
    let file = dir.join("config.json");

    // SAFETY: set before anything in this binary reads the config, and this is
    // the only test in it — see the module comment.
    unsafe { std::env::set_var("AUDIO_GRAPH_CONFIG", &file) };

    // First run: no file, so the conventional folders are written into one.
    // Compared as a set of paths, because that is what the settings hold — the
    // format each is conventionally for is dropped on the way in.
    let expected: Vec<PathBuf> = {
        let mut dirs = Vec::new();
        for (_, d) in plugin_host::default_plugin_directories() {
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
        dirs
    };
    let seeded = config::directories();
    assert_eq!(seeded, expected, "a first run starts from the conventions");
    assert!(
        file.is_file(),
        "and writes them down rather than implying them"
    );

    // Every one of them is a scanned directory, for every format: past the
    // settings, a folder is a folder.
    let scanned = plugin_host::plugin_directories();
    for d in seeded.iter().filter(|d| d.is_dir()) {
        for format in plugin_host::FORMATS {
            assert!(
                scanned.contains(&(format, d.clone())),
                "{format} should scan {}",
                d.display()
            );
        }
    }

    // A seeded folder is the user's to remove, which is the whole point of
    // seeding rather than always scanning.
    if let Some(first) = seeded.first().cloned() {
        config::remove_directory(&first).expect("a temp profile is writable");
        assert!(
            !config::directories().contains(&first),
            "a conventional folder can be removed"
        );
        assert!(
            !plugin_host::plugin_directories()
                .iter()
                .any(|(_, d)| *d == first),
            "and removing it stops it being scanned"
        );
    }

    // Adding is idempotent: the user asked for that folder to be scanned, and
    // after the call it is, once.
    config::add_directory(&dir).expect("saving works");
    config::add_directory(&dir).expect("saving again works");
    assert_eq!(
        config::directories().iter().filter(|d| **d == dir).count(),
        1
    );

    // An emptied list stays empty. "Scan nothing" is a thing a user may ask
    // for, so re-seeding keys off the file being absent, never off the list
    // being short.
    config::store(&config::Config::default()).expect("saving works");
    assert!(
        config::directories().is_empty(),
        "an emptied list stays empty"
    );
    assert!(plugin_host::plugin_directories().is_empty());

    // ...and the button that puts the conventions back does so.
    config::restore_defaults().expect("saving works");
    assert_eq!(config::directories(), expected);

    // A change made by another process — written straight to the file, behind
    // the cache's back — is noticed by its modification time.
    let other = dir.join("elsewhere");
    std::fs::create_dir_all(&other).expect("a subdirectory can be made");
    // Stamps have coarse resolution on some filesystems, so make sure the
    // rewrite lands on a different one rather than testing the clock.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(
        &file,
        format!(
            r#"{{"directories":[{}]}}"#,
            serde_json::to_string(&other).expect("a path encodes")
        ),
    )
    .expect("the file is writable");
    assert_eq!(
        config::directories(),
        vec![other.clone()],
        "a config changed underneath us is re-read"
    );

    // A file from the first build of this feature, when the field was called
    // `extra_directories`, still loads.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(
        &file,
        format!(
            r#"{{"extra_directories":[{}]}}"#,
            serde_json::to_string(&dir).expect("a path encodes")
        ),
    )
    .expect("the file is writable");
    assert_eq!(config::directories(), vec![dir.clone()]);
    assert!(
        config::pinned().is_empty(),
        "a file from before pinning loads with nothing pinned"
    );

    // Pinning is by full path, saved, and asked for as a state rather than as a
    // toggle: pinning what is already pinned leaves one entry, not two.
    let plugin = dir.join("Raum.vst3");
    assert!(config::set_pinned(&plugin, true).expect("a temp profile is writable"));
    assert!(config::set_pinned(&plugin, true).expect("a temp profile is writable"));
    assert_eq!(config::pinned(), vec![plugin.clone()]);
    assert!(config::is_pinned(&plugin));
    assert!(
        !config::is_pinned(&dir.join("Other.vst3")),
        "a pin names one module, not every module beside it"
    );
    assert_eq!(
        config::directories(),
        vec![dir.clone()],
        "and saving a pin leaves the folder list alone"
    );
    config::set_pinned(&plugin, false).expect("a temp profile is writable");
    assert!(config::pinned().is_empty());

    // Malformed content is not fatal, and — the part that matters — is not
    // mistaken for a missing file, which would throw the user's list away and
    // re-seed over it.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&file, "{ this is not json").expect("the file is writable");
    assert!(
        config::directories().is_empty(),
        "a file we cannot parse gives an empty list, not the conventions"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
