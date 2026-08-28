//! Configuration and directory management for plugin scanning.
//!
//! Stores user-configured search directories and pinned plugins in `config.json`
//! in the platform's local application data directory.
//!
//! # Seeding and Defaults
//!
//! On first run when no configuration file exists, the configuration is initialized
//! with default OS-conventional plugin directories. The configuration file becomes
//! the authoritative source of directories to scan; entries can be freely added
//! or removed by the user.
//!
//! # Process and File Caching
//!
//! An in-memory cache synchronized across threads caches the loaded configuration,
//! checking the file modification timestamp before re-reading from disk to detect
//! external updates.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

/// Configuration options persisted across sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Directories to scan for plugins, in user-configured order.
    ///
    /// An alias `extra_directories` is accepted for backwards compatibility with
    /// earlier configuration formats.
    #[serde(alias = "extra_directories")]
    pub directories: Vec<PathBuf>,

    /// Plugin module paths pinned by the user.
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

/// Returns the path to `config.json`, respecting the `AUDIO_GRAPH_CONFIG` environment override.
pub fn config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("AUDIO_GRAPH_CONFIG").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(path));
    }
    Some(config_directory()?.join("config.json"))
}

fn stamp_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Returns the standard default plugin directories without format associations.
fn conventional() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    for (_, dir) in crate::scan::default_plugin_directories() {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// Reads the config directly from disk, updating the cache if modified.
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

/// Loads the active configuration, initializing defaults on first run if missing.
pub fn load() -> Config {
    if let Some(config) = read_file() {
        return config;
    }
    let seeded = Config {
        directories: conventional(),
        ..Config::default()
    };
    if let Err(e) = store(&seeded) {
        log::warn!("audio-graph: the plugin folders could not be saved: {e}");
    }
    seeded
}

/// Updates configuration in memory and writes it to disk atomically.
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

    let mut cache = cache().write().unwrap();
    cache.stamp = stamp_of(&path);
    Ok(())
}

/// Returns all configured scan directories.
pub fn directories() -> Vec<PathBuf> {
    load().directories
}

/// Adds `dir` to the scan directory list and saves configuration.
pub fn add_directory(dir: impl Into<PathBuf>) -> Result<(), String> {
    let dir = dir.into();
    let mut config = load();
    if config.directories.contains(&dir) {
        return Ok(());
    }
    config.directories.push(dir);
    store(&config)
}

/// Removes `dir` from the scan directory list and saves configuration.
pub fn remove_directory(dir: &Path) -> Result<(), String> {
    let mut config = load();
    config.directories.retain(|d| d != dir);
    store(&config)
}

/// Restores any missing default conventional plugin directories and saves configuration.
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

/// Returns all pinned module paths.
pub fn pinned() -> Vec<PathBuf> {
    load().pinned
}

/// Returns whether `path` is currently pinned.
pub fn is_pinned(path: &Path) -> bool {
    load().pinned.iter().any(|p| p == path)
}

/// Sets the pinned status of `path` and saves configuration, returning the new pinned state.
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
