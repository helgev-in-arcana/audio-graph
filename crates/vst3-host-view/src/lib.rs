//! VST3 plugin editor window hosting and frame management.
//!
//! Provides top-level window embedding for VST3 `IPlugView` instances, `IPlugFrame`
//! handling for host resize notifications, and lifecycle management.

mod editor;
mod frame;

pub use editor::{EditorWindow, can_resize};
pub use frame::PlugFrame;

// The container window and the key forwarder are both format-agnostic and live
// in `host-window`, where the CLAP backend can reach them without depending on
// VST3. Re-exported so a caller that speaks in this crate's names needs no
// dependency of its own on `host-window`.
pub use host_window::{
    ContainerWindow, Key, Size, WindowState, forward_key, poll, pump_events, root_window,
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
