//! Handing keystrokes back to the DAW.
//!
//! A plugin editor is a child window, and child windows are the end of the line
//! for keyboard messages: Windows delivers `WM_KEYDOWN` to whatever has focus
//! and `DefWindowProc` does not pass it to the parent the way it does for, say,
//! a command. So while the editor has focus the DAW hears nothing, and the
//! space bar — which every DAW binds to transport — stops the user's tape
//! machine from responding.
//!
//! Neither VST3 nor the GUI stack has a route for this. `IPlugView::onKeyDown`
//! runs the other way (host to plugin), and baseview reports a key as consumed
//! whether or not anything did anything with it. So the editor decides for
//! itself: if egui had no use for the key, post it to the DAW's own window and
//! let the DAW's accelerators see it.
//!
//! Only the virtual key travels. Modifier state is left to `GetKeyState`, which
//! is still accurate because the user is physically holding the modifier down
//! while this runs.

/// Post a key up or down to `window`, as if it had been typed there.
///
/// `window` is a root window handle as returned by [`crate::root_window`], and
/// `vk` a Win32 virtual key code. A zero handle is ignored, so a caller that
/// never found the DAW's window needs no special case.
pub fn forward(window: usize, vk: u16, pressed: bool) {
    imp::forward(window, vk, pressed);
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::{HWND, LPARAM, WPARAM};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC, MapVirtualKeyW};
    use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_KEYDOWN, WM_KEYUP};

    pub fn forward(window: usize, vk: u16, pressed: bool) {
        if window == 0 || vk == 0 {
            return;
        }
        let scan = unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) };

        // The documented lParam layout: repeat count in 0..16, scan code in
        // 16..24, and for a release the "was down" and "transition" bits at 30
        // and 31. Some hosts read the scan code rather than the virtual key, so
        // it is worth filling in properly.
        let mut lparam: usize = 1 | ((scan as usize & 0xff) << 16);
        if !pressed {
            lparam |= (1 << 30) | (1 << 31);
        }

        let message = if pressed { WM_KEYDOWN } else { WM_KEYUP };
        unsafe {
            PostMessageW(
                window as HWND,
                message,
                vk as usize as WPARAM,
                lparam as LPARAM,
            )
        };
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn forward(_window: usize, _vk: u16, _pressed: bool) {}
}
