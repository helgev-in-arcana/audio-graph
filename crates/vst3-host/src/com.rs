//! COM apartment initialization for standalone host processes.
//!
//! Plugins that draw with Direct2D, use the Windows shell, or interact with COM
//! objects assume the thread creating them is an initialized Single-Threaded
//! Apartment (STA). DAW main threads typically initialize COM, but standalone
//! host executables and test harnesses must initialize it explicitly.
//!
//! This is deliberately not called automatically inside the library. Calling
//! `OleInitialize` on a thread already initialized by a host DAW can modify the
//! apartment configuration. Therefore, initialization is left to the process owner.

/// Initializes the calling thread into a Single-Threaded Apartment (STA).
///
/// Call once on the main thread from a standalone host binary or test harness.
/// Do not call this when running as a plugin inside a host DAW.
/// Calling this multiple times is safe: if the thread is already in an STA,
/// it succeeds; if it is in a different apartment mode, `RPC_E_CHANGED_MODE`
/// is returned and treated as non-fatal.
///
/// No matching `OleUninitialize` is provided, as the apartment is intended
/// to persist for the lifetime of the process.
#[cfg(windows)]
pub fn init_apartment() {
    #[link(name = "ole32")]
    unsafe extern "system" {
        fn OleInitialize(reserved: *const std::ffi::c_void) -> i32;
    }

    const S_FALSE: i32 = 1;
    const RPC_E_CHANGED_MODE: i32 = 0x8001_0106_u32 as i32;

    let hr = unsafe { OleInitialize(std::ptr::null()) };
    if hr != 0 && hr != S_FALSE && hr != RPC_E_CHANGED_MODE {
        log::warn!("OleInitialize failed: 0x{hr:08X}; COM-using plugins may fault");
    }
}

#[cfg(not(windows))]
pub fn init_apartment() {}
