//! Format-agnostic window management and event plumbing for hosting plugin editors.
//!
//! Three concerns, and none of them is VST3's or CLAP's:
//!
//! * [`ContainerWindow`] — a bare top-level frame for a plugin's own editor to
//!   be attached to.
//! * [`forward_key`] — sending a key the plugin's window swallowed on to the
//!   DAW.
//! * [`watch`] — running a plugin's timers, and on Linux its file descriptors,
//!   because a plugin cannot wait on anything itself. Both formats ask for
//!   this; only the words differ.
//!
//! The formats disagree only about the *name* they give a platform handle —
//! `"HWND"` versus `"win32"`, `"X11EmbedWindowID"` versus `"x11"` — and that
//! name belongs to the backend that speaks it. `vst3-host-view` and `clap-host`
//! both build on what is here.
//!
//! # Who pumps
//!
//! On Windows a window belongs to the thread that created it and the DAW's own
//! pump delivers its messages, so there is no loop to run. On X11 the connection
//! is this crate's alone and nothing the host does will ever advance it, so
//! whoever owns a window has to call [`poll`] regularly. [`poll`] does nothing
//! on the platforms where the host already does the work, so the caller needs no
//! `cfg` of its own.

mod keys;
mod window;

pub mod watch;

#[cfg(windows)]
#[path = "win32/mod.rs"]
mod imp;

#[cfg(all(unix, not(target_os = "macos")))]
#[path = "x11/mod.rs"]
mod imp;

#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
#[path = "stub/mod.rs"]
mod imp;

pub use keys::{Key, forward as forward_key};
pub use window::{ContainerWindow, Size, WindowState, pump_events, root_window};

/// Advance our windows, if their event source is ours to advance.
///
/// Call once per UI tick from anything that owns a [`ContainerWindow`],
/// including inside a plugin. On Windows and macOS this does nothing — the
/// host's pump is already delivering — and on X11 it is the only thing that
/// delivers a close request or a resize.
///
/// Delivering means writing to [`WindowState`], and nothing else: no callback
/// of the caller's runs from here, so there is no turn of this that is unsafe
/// to be inside of.
///
/// Distinct from [`pump_events`], which a plugin must never call: that one
/// drains the *host's* queue as well, and on Windows that means dispatching the
/// DAW's messages on its behalf.
pub fn poll() {
    if !imp::HOST_DRIVES_EVENTS {
        imp::pump_events();
    }
}
