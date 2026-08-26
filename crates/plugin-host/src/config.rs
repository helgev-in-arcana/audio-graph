//! The folders sub-plugins are looked for in, and where that list is kept.
//!
//! Nothing in either format lets a plugin discover where the DAW looks: neither
//! VST3 nor CLAP has a way to ask. So the list is the user's, and this is where
//! it lives.
//!
//! # One list, seeded once
//!
//! The first time anything asks, there is no file, and one is written holding
//! the OS-conventional directories — the ones a plugin would have been found in
//! anyway. From then on the file is the whole answer: a folder the user adds and
//! a folder that came from the conventions are the same kind of thing, and
//! either can be removed.
//!
//! That is deliberate, and it is what every DAW's own plugin-paths dialog does.
//! The alternative — conventions always scanned, user folders on top — means a
//! folder the user can see in the list but cannot remove, and no way to stop
//! scanning somewhere slow or broken. Seeding costs one file write and makes
//! the file say the truth: this, and only this, is what gets scanned.
//!
//! A list that is empty because the user emptied it stays empty. Re-seeding
//! keys off the file being absent, never off the list being short, so "scan
//! nothing" remains something a user can ask for. [`restore_defaults`] is how
//! they get the conventions back, and it adds rather than replaces.
//!
//! # Where
//!
//! The per-user local config directory, never next to the module. The bundle
//! usually sits somewhere only an administrator can write (`Common Files\VST3`
//! on Windows, `/Library/Audio/Plug-Ins/VST3` on macOS), writing inside a macOS
//! bundle invalidates its signature, and a reinstall would take the file with
//! it.
//!
//! Local rather than roaming, because the content is a list of absolute paths:
//! `D:\Plugins\VST3` means nothing on the other machine a roaming profile would
//! carry it to.
//!
//! # Shared between instances
//!
//! Every instance in the process reads one cache, so a folder added in one
//! wrapper's editor is there for the next one that scans without either of them
//! knowing the other exists. Across processes — two DAWs open at once — the
//! file's modification time settles it: a scan re-reads when the file changed
//! underneath it, which costs one `stat` per rescan and nothing per frame.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Everything this crate keeps between sessions.
///
/// It is a struct rather than a bare `Vec` so that adding a setting does not
/// invalidate everybody's file, and `serde(default)` is what makes a file
/// written by an older build — one from before pinning, say — still load.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Every folder to scan, in the order they appear to the user.
    ///
    /// Each is searched for every format, whatever it is called: a user
    /// pointing at their plugin folder should not have to say which kinds of
    /// plugin are in it, and a conventional `VST3` folder with a stray `.clap`
    /// in it is the user's business rather than ours.
    ///
    /// The alias is for files written by the first build of this feature, which
    /// called the field `extra_directories` back when the conventions were
    /// scanned separately.
    #[serde(alias = "extra_directories")]
    pub directories: Vec<PathBuf>,

    /// The modules the user pinned to the top of the add-node menu.
    ///
    /// Full paths, because that is what identifies a module: two folders can
    /// hold a `Raum.vst3` each, and pinning one of them must not pin the other.
    /// A path that no longer exists is kept rather than pruned — an unplugged
    /// drive or a plugin folder temporarily off the scan list should not quietly
    /// cost the user their pins.
    pub pinned: Vec<PathBuf>,
}

/// The process-wide copy, and what it was read from.
struct Cache {
    config: Config,
    /// The modification time the file had when it was last read; `None` when
    /// there is no file, so that one appearing later is noticed too.
    stamp: Option<SystemTime>,
    /// Whether the file has ever been read. Distinguishes "no file" from "not
    /// looked yet", which otherwise look identical.
    read: bool,
}

fn cache() -> &'static RwLock<Cache> {
    static CACHE: OnceLock<RwLock<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        RwLock::new(Cache {
            config: Config::default(),
            stamp: None,
            read: false,
        })
    })
}

/// The directory the config file lives in, or `None` if the platform will not
/// say where that is.
///
/// Hand-rolled rather than pulled from a crate on purpose: it is one
/// environment variable per platform, and this crate is on the path of
/// everything that loads a plugin.
fn config_directory() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        // Local, not Roaming: see the module comment.
        std::env::var_os("LOCALAPPDATA").map(|v| PathBuf::from(v).join("AudioGraph"))
    }

    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(|v| PathBuf::from(v).join("Library/Application Support/AudioGraph"))
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
            return Some(PathBuf::from(xdg).join("audio-graph"));
        }
        std::env::var_os("HOME").map(|v| PathBuf::from(v).join(".config/audio-graph"))
    }
}

/// The config file itself.
///
/// Public because a user who has to be told where their settings went is owed
/// the actual path, and the CLI prints it.
pub fn config_path() -> Option<PathBuf> {
    // An override, which is what makes any of this testable: a test must not
    // touch the profile of whoever runs it.
    if let Some(path) = std::env::var_os("AUDIO_GRAPH_CONFIG").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(path));
    }
    Some(config_directory()?.join("config.json"))
}

fn stamp_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// The conventional directories, as plain paths.
///
/// The format each is conventionally for is dropped on the way in, because past
/// this point a folder is a folder — see [`Config::directories`].
fn conventional() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for (_, dir) in crate::scan::default_plugin_directories() {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// The config exactly as the file has it, with no seeding.
///
/// A file that cannot be read or does not parse gives the default rather than
/// an error: the alternative is a wrapper that lists no plugins at all because
/// one line of JSON is malformed. `None` means there is no file yet, which is
/// what [`load`] acts on and what a parse failure deliberately does *not* look
/// like — re-seeding over a file we merely failed to understand would throw the
/// user's list away.
fn read_file() -> Option<Config> {
    let path = config_path()?;
    let stamp = stamp_of(&path);

    {
        let cache = cache().read().unwrap();
        if cache.read && cache.stamp == stamp {
            return Some(cache.config.clone());
        }
    }

    let bytes = std::fs::read(&path).ok()?;
    let config = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        log::warn!(
            "audio-graph: {} is not readable config: {e}",
            path.display()
        );
        Config::default()
    });

    let mut cache = cache().write().unwrap();
    cache.config = config.clone();
    cache.stamp = stamp;
    cache.read = true;
    Some(config)
}

/// The config as it stands, writing one out first if there is none.
///
/// Re-reads the file when it changed underneath us, so a second DAW's edits are
/// picked up.
pub fn load() -> Config {
    if let Some(config) = read_file() {
        return config;
    }
    // No file: first run. Write down the folders a plugin would have been found
    // in anyway, so that the list the user is shown is complete from the start
    // and every line of it is theirs to remove.
    let seeded = Config {
        directories: conventional(),
        ..Config::default()
    };
    if let Err(e) = store(&seeded) {
        // Not fatal. The list is right for this session; it just will not
        // survive, and the editor says so when the user changes something.
        log::warn!("audio-graph: the plugin folders could not be saved: {e}");
    }
    seeded
}

/// Replace the config, in this process and on disk.
///
/// The in-memory copy is updated whether or not the write succeeds, so a user
/// whose profile is read-only still gets the folder they just added for as long
/// as the DAW stays open — and is told, by the returned error, that it will not
/// outlive it.
pub fn store(config: &Config) -> Result<(), String> {
    {
        let mut cache = cache().write().unwrap();
        cache.config = config.clone();
        cache.read = true;
    }

    let path = config_path().ok_or_else(|| "no config directory on this platform".to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(config).map_err(|e| format!("encoding config: {e}"))?;

    // Written beside the target and renamed over it, so that a crash halfway
    // through leaves the previous settings rather than half of the new ones.
    let temp = path.with_extension("json.new");
    {
        let mut file =
            std::fs::File::create(&temp).map_err(|e| format!("writing {}: {e}", temp.display()))?;
        file.write_all(&json)
            .and_then(|()| file.sync_all())
            .map_err(|e| format!("writing {}: {e}", temp.display()))?;
    }
    std::fs::rename(&temp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        format!("replacing {}: {e}", path.display())
    })?;

    // The file just written is the newest one; record its stamp so the next
    // read does not fetch it back for nothing.
    let mut cache = cache().write().unwrap();
    cache.stamp = stamp_of(&path);
    Ok(())
}

/// Every folder to scan, in the order the user sees them.
pub fn directories() -> Vec<PathBuf> {
    load().directories
}

/// Add `dir` to the list and save.
///
/// A duplicate is neither an error nor a second entry: the user asked for that
/// folder to be scanned, and after this call it is.
pub fn add_directory(dir: impl Into<PathBuf>) -> Result<(), String> {
    let dir = dir.into();
    let mut config = load();
    if config.directories.contains(&dir) {
        return Ok(());
    }
    config.directories.push(dir);
    store(&config)
}

/// Drop `dir` from the list and save.
///
/// Removing a folder that came from the conventions is allowed, and is the
/// point of seeding them in: a folder full of plugins that crash the scanner is
/// something the user must be able to stop looking at.
pub fn remove_directory(dir: &Path) -> Result<(), String> {
    let mut config = load();
    config.directories.retain(|d| d != dir);
    store(&config)
}

/// Put back any conventional folder that is not in the list, and save.
///
/// Adds rather than replaces: the user's own folders are not what they asked to
/// undo. Also the way a folder that appeared after the list was seeded — a
/// plugin format installed later, a `CLAP_PATH` set since — gets picked up.
pub fn restore_defaults() -> Result<(), String> {
    let mut config = load();
    let mut added = false;
    for dir in conventional() {
        if !config.directories.contains(&dir) {
            config.directories.push(dir);
            added = true;
        }
    }
    if !added {
        return Ok(());
    }
    store(&config)
}

/// The modules pinned to the top of the add-node menu.
pub fn pinned() -> Vec<PathBuf> {
    load().pinned
}

/// Whether `path` is pinned.
pub fn is_pinned(path: &Path) -> bool {
    load().pinned.iter().any(|p| p == path)
}

/// Pin or unpin `path`, and save. Returns whether it is pinned afterwards.
///
/// Idempotent in both directions: the caller asks for a state rather than for a
/// change, so two editors toggling the same plugin cannot leave it pinned twice
/// or unpin what the other just pinned.
pub fn set_pinned(path: &Path, pin: bool) -> Result<bool, String> {
    let mut config = load();
    let had = config.pinned.iter().any(|p| p == path);
    if had == pin {
        return Ok(pin);
    }
    if pin {
        config.pinned.push(path.to_path_buf());
    } else {
        config.pinned.retain(|p| p != path);
    }
    store(&config)?;
    Ok(pin)
}
