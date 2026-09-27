use std::path::PathBuf;

use plugin_host::PluginRef;

use crate::catalogue;

/// Where a saved [`PluginRef`] may be found, most likely first, without
/// loading anything.
///
/// The id is the authority and the path only a hint, so a project that moved
/// between machines still opens. The hint comes first when the file is still
/// there; after it, every module the catalogue says exports the id, those
/// whose file is unchanged since they were scanned ahead of those it has
/// changed under.
///
/// Nothing here opens a module, and nothing searches the disk: a module the
/// catalogue has not seen is not a candidate. Asking every module on the
/// machine whether it is the one would load third-party code by the hundred,
/// on whatever thread is restoring the project — the DAW's main thread — and
/// a scan belongs on a thread of its own. A reference with no candidate stays
/// unresolved until a scan has found its module.
///
/// The caller loads the candidates in turn with the id and keeps the first
/// that takes, which is also what checks the hint: a file at the hint's path
/// that no longer exports the id fails to load and the next candidate is
/// tried.
pub fn reference_candidates(reference: &PluginRef, known: &[catalogue::Module]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if reference.path_hint.exists() {
        out.push(reference.path_hint.clone());
    }
    let mut listed: Vec<(bool, &PathBuf)> = known
        .iter()
        .filter(|module| {
            module.format == reference.format
                && module.error.is_none()
                && module.classes.iter().any(|class| class.id == reference.id)
                && module.path.exists()
        })
        .map(|module| {
            (
                catalogue::stamp_of(&module.path) != module.stamp,
                &module.path,
            )
        })
        .collect();
    // Stable, so modules with the same freshness keep the catalogue's order.
    listed.sort_by_key(|&(stale, _)| stale);
    for (_, path) in listed {
        if !out.contains(path) {
            out.push(path.clone());
        }
    }
    out
}
