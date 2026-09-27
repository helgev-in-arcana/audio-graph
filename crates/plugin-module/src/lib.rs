//! Format-independent plugin module loading, shared by the backends.
//!
//! Everything here is the same for a VST3 and a CLAP: opening a shared
//! library, finding it inside a bundle, making sure one binary is owned by one
//! thread and entered once, and listing the modules in a folder. What differs —
//! which symbol is the entry point, how it is balanced, what a class is — stays
//! in `vst3-host` and `clap-host`.
//!
//! Nothing here decides where plugins live. A folder to list is always the
//! caller's.

mod bundle;
mod discovery;
mod lease;
mod library;
mod loaded;

pub use bundle::bundle_binary;
pub use discovery::{find_modules, list_modules};
pub use lease::{ModuleLease, identity};
pub use library::Library;
pub use loaded::Loaded;
