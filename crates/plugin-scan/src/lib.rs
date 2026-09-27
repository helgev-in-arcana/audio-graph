//! Finding plugins: which modules are in the caller's folders, what they
//! contain as last scanned, and where a saved reference may now be.
//!
//! Above `plugin-host`, which loads one module it is given. Nothing here is
//! needed to load a plugin whose path is known — an offline renderer handed a
//! path, or the sub-host restoring a project with the candidates it is given,
//! does without it. What is here is everything that has to look at more than
//! one module: listing folders, the catalogue a scan builds, and resolving a
//! reference against that catalogue.
//!
//! Which folders is always the caller's to say.

mod candidates;
pub mod catalogue;
mod discovery;

pub use candidates::reference_candidates;
pub use discovery::{find_modules, installed_modules, plugin_directories};
