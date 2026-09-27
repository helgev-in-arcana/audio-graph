//! Where a CLAP bundle keeps its binary. Opening it is `plugin_module`'s.

use std::path::{Path, PathBuf};

use plugin_host_api::HostError;

/// Subdirectory of `Contents/` a macOS CLAP bundle keeps its binary in.
///
/// Windows and Linux have no bundle: a `.clap` there is the shared library
/// itself, which is why this only matters on the platform that has bundles.
const MACOS_BUNDLE_DIR: &str = "MacOS";

/// Map a `.clap` path to the shared library that has to be loaded.
pub(crate) fn resolve_binary(path: &Path) -> Result<PathBuf, HostError> {
    plugin_module::bundle_binary(path, MACOS_BUNDLE_DIR, "")
}
