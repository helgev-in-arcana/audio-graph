//! Win32 backend.
//!
//! The DAW's own message pump delivers everything: a window belongs to the
//! thread that created it, and inside a plugin that thread is the host's UI
//! thread. There is no loop of ours to run.

mod deferred;
mod keys;
mod window;

pub(crate) use deferred::{DeferredHandle, destroy_deferred, new_deferred, wake_deferred};
pub(crate) use keys::forward_key;
pub(crate) use window::{Window, pump_events, root_window};

/// Whether the host, rather than this crate, drives the event source our
/// windows live on. See [`crate::poll`].
pub(crate) const HOST_DRIVES_EVENTS: bool = true;
