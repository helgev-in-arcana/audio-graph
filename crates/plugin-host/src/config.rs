//! The user's own plugin folders, and where that list is kept.
//!
//! A host asks the OS-conventional directories on its own (see
//! [`crate::default_plugin_directories`]), and for most installs that is the
//! whole answer. It stops being the whole answer the moment someone keeps
//! plugins somewhere else — which is common enough, and which nothing in either
//! format lets us discover: neither VST3 nor CLAP has a way to ask the DAW what
//! *it* scans. So the user says so, from the editor, and this is where it is
//! remembered.
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
/// One field today. It is a struct rather than a bare `Vec` so that adding the
/// next setting does not invalidate everybody's file, and `serde(default)` is
/// what makes a file written by an older build still load.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Directories to scan on top of the conventional ones.
    ///
    /// Searched for every format, the same way a directory given to the CLI is:
    /// a user pointing at their plugin folder should not have to say which
    /// kinds of plugin are in it.
    pub extra_directories: Vec<PathBuf>,
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

/// The config as it stands, re-reading the file if it changed underneath us.
///
/// A file that cannot be read or does not parse gives the default rather than
/// an error: the alternative is a wrapper that lists no plugins at all because
/// one line of JSON is malformed.
pub fn load() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    let stamp = stamp_of(&path);

    {
        let cache = cache().read().unwrap();
        if cache.read && cache.stamp == stamp {
            return cache.config.clone();
        }
    }

    let config = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            log::warn!(
                "audio-graph: {} is not readable config: {e}",
                path.display()
            );
            Config::default()
        }),
        // Not an error. Nobody has opened the settings yet.
        Err(_) => Config::default(),
    };

    let mut cache = cache().write().unwrap();
    cache.config = config.clone();
    cache.stamp = stamp;
    cache.read = true;
    config
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
    // `load` does not read it back for nothing.
    let mut cache = cache().write().unwrap();
    cache.stamp = stamp_of(&path);
    Ok(())
}

/// The user's extra directories, in the order they added them.
pub fn extra_directories() -> Vec<PathBuf> {
    load().extra_directories
}

/// Add `dir` to the user's directories and save.
///
/// A duplicate is neither an error nor a second entry: the user asked for that
/// folder to be scanned, and after this call it is.
pub fn add_directory(dir: impl Into<PathBuf>) -> Result<(), String> {
    let dir = dir.into();
    let mut config = load();
    if config.extra_directories.contains(&dir) {
        return Ok(());
    }
    config.extra_directories.push(dir);
    store(&config)
}

/// Drop `dir` from the user's directories and save.
pub fn remove_directory(dir: &Path) -> Result<(), String> {
    let mut config = load();
    config.extra_directories.retain(|d| d != dir);
    store(&config)
}
