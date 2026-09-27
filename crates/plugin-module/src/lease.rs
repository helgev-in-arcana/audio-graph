use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, PoisonError};

use plugin_host_api::HostError;

static CLAIMED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

/// The key a module's binary is known by: its canonical path, so two spellings
/// of one file are one module.
pub fn identity(binary: PathBuf) -> PathBuf {
    std::fs::canonicalize(&binary).unwrap_or(binary)
}

/// A claim on one module binary by the thread that loaded it, released on
/// drop.
///
/// A module's objects are pinned to the thread that created them, and its
/// entry point is balanced per thread's table of loaded modules (see
/// [`Loaded`][crate::Loaded]). A second thread loading the same binary would
/// run the entry point again underneath the first, so it is refused with
/// [`HostError::ModuleBusy`] instead. One set for every format: a binary is
/// one binary whichever format it answers to.
pub struct ModuleLease(PathBuf);

impl ModuleLease {
    /// Claim `key` — an [`identity`] — or say who has it.
    pub fn acquire(key: PathBuf) -> Result<ModuleLease, HostError> {
        let mut claimed = CLAIMED
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !claimed.insert(key.clone()) {
            return Err(HostError::ModuleBusy(key.display().to_string()));
        }
        Ok(ModuleLease(key))
    }
}

impl Drop for ModuleLease {
    fn drop(&mut self) {
        if let Some(claimed) = CLAIMED.get() {
            claimed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&self.0);
        }
    }
}
