use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use plugin_host_api::HostError;

/// Map a module path to the shared library that has to be loaded.
///
/// A plain file is that library. A bundle directory keeps it in
/// `Contents/<contents_dir>/`, named after the bundle with `extension` when
/// one is given. The name is only a convention, so a bundle without a binary
/// of that name falls back to the first file in the directory rather than
/// failing outright.
pub fn bundle_binary(
    path: &Path,
    contents_dir: &str,
    extension: &str,
) -> Result<PathBuf, HostError> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    if !path.is_dir() {
        return Err(HostError::ModuleLoad(format!(
            "{} does not exist",
            path.display()
        )));
    }

    let contents = path.join("Contents").join(contents_dir);
    let stem = path
        .file_stem()
        .unwrap_or_else(|| OsStr::new("plugin"))
        .to_os_string();
    let named = if extension.is_empty() {
        contents.join(&stem)
    } else {
        contents.join(&stem).with_extension(extension)
    };
    if named.is_file() {
        return Ok(named);
    }

    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&contents)
        .map_err(|e| HostError::ModuleLoad(format!("cannot read {}: {e}", contents.display())))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    candidates.sort();

    candidates
        .into_iter()
        .next()
        .ok_or_else(|| HostError::ModuleLoad(format!("no binary found in {}", contents.display())))
}
