//! Format-agnostic window management and event plumbing for hosting plugin editors.
//!
//! Provides core utilities for managing editor container windows, deferring work
//! across platform message loops, and forwarding unhandled keyboard input to host windows.

mod deferred;
mod keys;
mod window;

pub use deferred::{Deferred, new as deferred};
pub use keys::forward as forward_key;
pub use window::{ContainerWindow, Size, WindowState, pump_events, root_window};
