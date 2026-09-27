use std::path::{Path, PathBuf};

use plugin_host::{FORMATS, Format};

/// Every directory a scan should look in, as the user's settings have it.
///
/// The supplied directory list is the whole answer: which folders to look in
/// is the caller's to say, including the ones a platform conventionally keeps
/// plugins in. Nothing here adds any of its own.
///
/// Each directory is paired with every format, because the user pointed at a
/// folder of plugins and not at a folder of VST3s.
///
/// Directories that do not exist are dropped: a folder can be on a drive that
/// is not plugged in today, and a scan should be quiet about that rather than
/// fail.
pub fn plugin_directories(directories: &[PathBuf]) -> Vec<(Format, PathBuf)> {
    let mut out = Vec::new();
    for dir in directories {
        if !dir.is_dir() {
            continue;
        }
        for format in FORMATS {
            out.push((format, dir.clone()));
        }
    }
    // A list that names the same folder twice should not list every plugin in
    // it twice.
    out.sort();
    out.dedup();
    out
}

/// Returns all plugin modules of the given `format` found in `dir` or its immediate subdirectories.
pub fn find_modules(format: Format, dir: &Path) -> Vec<PathBuf> {
    plugin_module::find_modules(dir, format.extension())
}

/// Every module of every format in every directory a scan covers.
///
/// Paths only: enumerating the classes inside means loading third-party code,
/// which is a decision the caller should make deliberately.
pub fn installed_modules(directories: &[PathBuf]) -> Vec<(Format, PathBuf)> {
    let mut out = Vec::new();
    for (format, dir) in plugin_directories(directories) {
        for path in find_modules(format, &dir) {
            out.push((format, path));
        }
    }
    out.sort();
    out.dedup();
    out
}
