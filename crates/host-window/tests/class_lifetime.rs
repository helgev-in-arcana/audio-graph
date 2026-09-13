#![cfg(windows)]

use host_window::{ContainerWindow, Size};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GCLP_HMODULE, GetClassInfoExW, GetClassLongPtrW, WNDCLASSEXW,
};

/// The last window releases its class, and a later window can register it again.
#[test]
fn the_class_lives_exactly_as_long_as_its_windows() {
    let name: Vec<u16> = "AudioGraphSubView".encode_utf16().chain(Some(0)).collect();
    for _ in 0..2 {
        let first =
            ContainerWindow::new("first", Size::new(160, 100), std::ptr::null_mut()).unwrap();
        let second =
            ContainerWindow::new("second", Size::new(160, 100), std::ptr::null_mut()).unwrap();
        let module = unsafe { GetClassLongPtrW(first.platform_handle(), GCLP_HMODULE) } as _;
        let exists = || {
            let mut info: WNDCLASSEXW = unsafe { std::mem::zeroed() };
            info.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;
            unsafe { GetClassInfoExW(module, name.as_ptr(), &mut info) != 0 }
        };
        assert!(exists());
        drop(first);
        assert!(exists());
        drop(second);
        assert!(!exists());
    }
}
