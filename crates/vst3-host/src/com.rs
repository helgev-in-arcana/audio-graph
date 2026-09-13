//! Explicit ownership of the calling thread's COM initialization.

use std::marker::PhantomData;
use std::rc::Rc;

#[cfg(windows)]
#[link(name = "ole32")]
unsafe extern "system" {
    fn OleInitialize(reserved: *const std::ffi::c_void) -> i32;
    fn OleUninitialize();
}

/// Keeps the calling thread prepared until every hosted plugin and returned processor is released.
///
/// Each successful initialization, including an existing STA, owns one matching uninitialization.
/// A DAW-provided thread must already support STA; an incompatible MTA is an error.
/// Other platforms require no apartment initialization.
///
/// ```compile_fail
/// let guard = vst3_host::init_apartment().unwrap();
/// std::thread::spawn(move || drop(guard));
/// ```
#[must_use = "hold the guard until plugins and returned processors have been released"]
pub struct ApartmentGuard(PhantomData<Rc<()>>);

impl Drop for ApartmentGuard {
    fn drop(&mut self) {
        plugin_host_api::reclaim_main_thread();
        #[cfg(windows)]
        unsafe {
            OleUninitialize();
        }
    }
}

/// Prepares this thread for plugin hosting. The guard must outlive all native resources it uses.
pub fn init_apartment() -> plugin_host_api::Result<ApartmentGuard> {
    #[cfg(windows)]
    {
        let result = unsafe { OleInitialize(std::ptr::null()) };
        if result < 0 {
            return Err(plugin_host_api::HostError::Backend {
                context: "OleInitialize".into(),
                code: result,
            });
        }
    }
    Ok(ApartmentGuard(PhantomData))
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};

    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CoInitializeEx(reserved: *const std::ffi::c_void, flags: u32) -> i32;
        fn CoUninitialize();
    }

    fn assert_ole_initialization(expected: i32) {
        // Apartment queries can succeed on an uninitialized thread through the process-wide MTA.
        let result = unsafe { OleInitialize(std::ptr::null()) };
        if result >= 0 {
            unsafe { OleUninitialize() };
        }
        assert_eq!(result, expected);
    }

    /// Every guard releases exactly its own reference even while another thread owns an MTA.
    #[test]
    fn nested_guards_balance_initialization() {
        assert_eq!(unsafe { CoInitializeEx(std::ptr::null(), 0) }, S_OK);
        let result = std::thread::spawn(|| {
            assert_ole_initialization(S_OK);
            let first = init_apartment().unwrap();
            let second = init_apartment().unwrap();
            drop(first);
            assert_ole_initialization(S_FALSE);
            drop(second);
            assert_ole_initialization(S_OK);
        })
        .join();
        unsafe { CoUninitialize() };
        result.unwrap();
    }

    /// Refusing MTA does not uninitialize the apartment owned by the caller.
    #[test]
    fn an_incompatible_apartment_is_an_error() {
        std::thread::spawn(|| {
            assert_eq!(unsafe { CoInitializeEx(std::ptr::null(), 0) }, S_OK);
            assert!(init_apartment().is_err());
            assert_ole_initialization(RPC_E_CHANGED_MODE);
            unsafe {
                CoUninitialize();
            }
            assert_ole_initialization(S_OK);
        })
        .join()
        .unwrap();
    }
}
