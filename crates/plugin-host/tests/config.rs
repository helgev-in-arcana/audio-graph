//! Tests for plugin directory and pinning configuration management.
//!
//! Verifies default directory seeding, persistence, modification timestamp reload,
//! legacy alias compatibility, and pinning operations.

use std::path::PathBuf;

use plugin_host::config;

#[test]
fn folders_are_seeded_saved_reread_and_scanned() {
    let dir = std::env::temp_dir().join("audio-graph-config-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temp directory can be made");
    let file = dir.join("config.json");

    // SAFETY: set before anything in this binary reads the config.
    unsafe { std::env::set_var("AUDIO_GRAPH_CONFIG", &file) };

    // First run with no configuration file seeds standard default directories.
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

    // Every configured folder is included in directory scanning.
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

    // Removing a seeded directory removes it from the configuration.
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

    // Adding is idempotent.
    config::add_directory(&dir).expect("saving works");
    config::add_directory(&dir).expect("saving again works");
    assert_eq!(
        config::directories().iter().filter(|d| **d == dir).count(),
        1
    );

    // An emptied list is preserved and not re-seeded.
    config::store(&config::Config::default()).expect("saving works");
    assert!(
        config::directories().is_empty(),
        "an emptied list stays empty"
    );
    assert!(plugin_host::plugin_directories().is_empty());

    // Restore defaults appends conventional directories.
    config::restore_defaults().expect("saving works");
    assert_eq!(config::directories(), expected);

    // External modifications are detected by timestamp comparison.
    let other = dir.join("elsewhere");
    std::fs::create_dir_all(&other).expect("a subdirectory can be made");
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

    // Backwards compatibility with the legacy `extra_directories` field.
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

    // Pinning operates on exact module paths and is idempotent.
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

    // Malformed JSON content falls back to empty defaults without overwriting.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&file, "{ this is not json").expect("the file is writable");
    assert!(
        config::directories().is_empty(),
        "a file we cannot parse gives an empty list, not the conventions"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
