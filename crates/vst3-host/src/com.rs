//! COM apartment initialisation, for hosts that own their process.
//!
//! Plugins that draw with Direct2D, use the shell, or hold any other COM object
//! assume the thread they were created on is an initialised STA — a DAW's main
//! thread always is, so plugins never check. A bare `main` or a `cargo test`
//! harness is not, and the failure surfaces far from the cause: OTT faults on
//! its *second* instantiation, not its first (§13).
//!
//! This is deliberately not called anywhere inside the library. Calling
//! `OleInitialize` on a thread a DAW already initialised is at best a no-op and
//! at worst changes the apartment out from under the host, so the decision
//! belongs to whoever owns the process — never to a plugin we are loaded into.

/// Put the calling thread into a single-threaded apartment.
///
/// Call once, first thing on the main thread, from a standalone host binary or
/// a test harness. **Never call this from a plugin**, including our own wrapper
/// — see the module docs. Idempotent enough to call twice; a thread already in
/// an STA reports success, and a thread already in a different apartment
/// reports `RPC_E_CHANGED_MODE`, which is not an error for our purposes.
///
/// No matching `OleUninitialize`: the apartment is meant to outlive everything
/// the process does with plugins.
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
