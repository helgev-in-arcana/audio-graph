//! The user's plugin folders: saved, re-read, and reaching a scan.
//!
//! One test function on purpose. The config is process-wide state behind a
//! single cache, and `AUDIO_GRAPH_CONFIG` — the override that keeps this off
//! the real profile of whoever runs the suite — is process-wide too. A second
//! `#[test]` in this binary would run beside this one and they would fight over
//! both.

use std::path::PathBuf;

use plugin_host::config;

#[test]
fn folders_are_saved_reread_and_scanned() {
    let dir = std::env::temp_dir().join("audio-graph-config-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a temp directory can be made");
    let file = dir.join("config.json");

    // SAFETY: set before anything in this binary reads the config, and this is
    // the only test in it — see the module comment.
    unsafe { std::env::set_var("AUDIO_GRAPH_CONFIG", &file) };

    // Nothing saved yet is the default, not an error.
    assert_eq!(config::extra_directories(), Vec::<PathBuf>::new());

    // A folder that exists, because that is what the editor allows through: the
    // one this test's own config lives in will do.
    config::add_directory(&dir).expect("a temp profile is writable");
    assert!(file.is_file(), "adding a folder writes the file");
    assert_eq!(config::extra_directories(), vec![dir.clone()]);

    // Adding it twice leaves one entry: the user asked for it to be scanned,
    // and it is.
    config::add_directory(&dir).expect("saving again works");
    assert_eq!(config::extra_directories(), vec![dir.clone()]);

    // The saved folder reaches the directories a scan covers, paired with every
    // format rather than one.
    let scanned = plugin_host::plugin_directories();
    for format in plugin_host::FORMATS {
        assert!(
            scanned.contains(&(format, dir.clone())),
            "{format} should scan the folder the user added"
        );
    }

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
            r#"{{"extra_directories":[{}]}}"#,
            serde_json::to_string(&other).expect("a path encodes")
        ),
    )
    .expect("the file is writable");
    assert_eq!(
        config::extra_directories(),
        vec![other.clone()],
        "a config changed underneath us is re-read"
    );

    // Removing the last one leaves an empty list rather than an absent file.
    config::remove_directory(&other).expect("saving works");
    assert_eq!(config::extra_directories(), Vec::<PathBuf>::new());
    assert!(file.is_file());

    // Malformed content is not fatal: a wrapper that listed no plugins at all
    // because of one bad line would be worse than one that forgets a folder.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&file, "{ this is not json").expect("the file is writable");
    assert_eq!(config::extra_directories(), Vec::<PathBuf>::new());

    let _ = std::fs::remove_dir_all(&dir);
}
