// ============================================================================
//
// HUMAN REVIEW REQUIRED: THIS FILE HAS NOT BEEN REVIEWED BY A HUMAN.
//
// ============================================================================

//! A config file this build cannot read is never overwritten away.
//!
//! One `#[test]` per file, for the reason `config.rs` gives: the test sets
//! `AUDIO_GRAPH_CONFIG` with `std::env::set_var`, which is only sound while no
//! other thread is running.

use audio_graph_settings as config;

/// Saving over a config that does not parse keeps the unreadable original beside the new one.
#[test]
fn an_unreadable_config_survives_the_next_save() {
    let dir = std::env::temp_dir().join(format!(
        "audio-graph-settings-unreadable-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temp directory can be made");
    let file = dir.join("config.json");
    let original = b"{ \"directories\": [\"C:/Plugins\" ";
    std::fs::write(&file, original).expect("a temp file can be written");

    // SAFETY: no other thread is running; this is the only test in this binary.
    unsafe { std::env::set_var("AUDIO_GRAPH_CONFIG", &file) };

    assert!(
        config::directories().is_empty(),
        "an unreadable file reads as empty rather than as the conventions"
    );
    assert_eq!(std::fs::read(&file).unwrap(), original, "and is left alone");

    let added = dir.join("added");
    config::add_directory(&added).expect("a temp profile is writable");
    assert_eq!(config::directories(), [added]);

    let kept: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().contains("unreadable"))
        })
        .collect();
    assert_eq!(kept.len(), 1, "the original is kept beside the new file");
    assert_eq!(std::fs::read(&kept[0]).unwrap(), original);

    let _ = std::fs::remove_dir_all(&dir);
}
