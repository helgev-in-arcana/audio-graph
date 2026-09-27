use std::path::{Path, PathBuf};

/// The modules with `extension` directly inside `dir`, sorted.
///
/// Not recursive: vendors nest their own subfolders, and a deep walk turns a
/// scan into a filesystem crawl. [`find_modules`] goes one level down, which
/// is how vendors group them.
pub fn list_modules(dir: &Path, extension: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == extension))
        .collect();
    out.sort();
    out
}

/// The modules with `extension` in `dir` and in the folders one level below
/// it. A bundle directory is a module, not a folder to look inside.
pub fn find_modules(dir: &Path, extension: &str) -> Vec<PathBuf> {
    let mut out = list_modules(dir, extension);
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut subdirs: Vec<_> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p.extension().is_none_or(|e| e != extension))
            .collect();
        subdirs.sort();
        for sub in subdirs {
            out.extend(list_modules(&sub, extension));
        }
    }
    out
}
