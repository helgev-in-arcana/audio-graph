//! Sub-plugin editor windows.
//!
//! Split out from `vst3-host` because it is the part that disappears when the
//! backend is used for anything else — an offline renderer or a scanner needs
//! none of it (ARCHITECTURE.md §2). It is also, per ADR-3, the piece that moves
//! wholesale into the child process if ADR-6 is ever triggered, since a child
//! process would create its own top-level window and no cross-process window
//! embedding would be needed at all.
//!
//! Three concerns, one per module: the container window, the `IPlugFrame` the
//! plugin talks back through, and the ordering rules that keep teardown from
//! crashing.

mod editor;
mod frame;

pub use editor::{EditorWindow, can_resize};
pub use frame::PlugFrame;

// The container window, the deferred queue and the key forwarder are all
// format-agnostic and live in `host-window`, where the CLAP backend can reach
// them without depending on VST3. Re-exported so callers that already speak in
// this crate's names do not have to change.
pub use host_window::{
    ContainerWindow, Deferred, Size, WindowState, deferred, forward_key, pump_events, root_window,
};

/// What the platform handle means to a VST3 plugin.
///
/// VST3 identifies the parent it is given by a string constant, and passing the
/// wrong one to a plugin that supports several is how an editor ends up
/// attached to nothing. CLAP names the same handles differently (`"win32"`,
/// `"cocoa"`, `"x11"`), which is why this constant belongs to the backend that
/// speaks it and not to [`ContainerWindow`].
pub const PLATFORM_TYPE: &str = {
    #[cfg(windows)]
    {
        "HWND"
    }
    #[cfg(target_os = "macos")]
    {
        "NSView"
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        "X11EmbedWindowID"
    }
};

#[cfg(test)]
mod tests {
    #[test]
    fn platform_type_matches_what_we_pass_to_attached() {
        // Handing a plugin the wrong platform string is how an editor attaches
        // to nothing at all, silently.
        #[cfg(windows)]
        assert_eq!(super::PLATFORM_TYPE, "HWND");
        #[cfg(target_os = "macos")]
        assert_eq!(super::PLATFORM_TYPE, "NSView");
    }
}
