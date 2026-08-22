//! Window plumbing for hosting somebody else's editor, with no plugin format
//! in sight.
//!
//! Three concerns, and none of them are VST3's or CLAP's:
//!
//! * [`ContainerWindow`] — a bare top-level frame for a plugin's own editor to
//!   be attached to.
//! * [`Deferred`] — running work on the next turn of the host's message loop,
//!   because a draw callback is not a safe place to touch a window.
//! * [`forward_key`] — sending a key the plugin's window swallowed on to the
//!   DAW.
//!
//! The formats disagree only about the *name* they give a platform handle —
//! `"HWND"` versus `"win32"` — and that name belongs to the backend that speaks
//! it. `vst3-host-view` and `clap-host` both build on what is here.

mod deferred;
mod keys;
mod window;

pub use deferred::{Deferred, new as deferred};
pub use keys::forward as forward_key;
pub use window::{ContainerWindow, Size, WindowState, pump_events, root_window};
