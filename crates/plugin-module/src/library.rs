use std::ffi::CString;
use std::path::{Path, PathBuf};

use plugin_host_api::HostError;

/// An opened shared library.
///
/// Knows nothing about any plugin format: which symbol is the entry point and
/// how it is balanced is the backend's, and has to be done at module scope
/// rather than here, because a module is entered once however many times it
/// is opened (see [`Loaded`][crate::Loaded]).
pub struct Library {
    handle: Handle,
    path: PathBuf,
}

impl Library {
    /// Open the shared library at `binary` — the file itself, not a bundle;
    /// see [`bundle_binary`][crate::bundle_binary].
    pub fn open(binary: &Path) -> Result<Library, HostError> {
        Ok(Library {
            handle: Handle::open(binary)?,
            path: binary.to_path_buf(),
        })
    }

    /// The file that was opened.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Look up an exported symbol.
    pub fn symbol(&self, name: &str) -> Option<*mut std::ffi::c_void> {
        let c = CString::new(name).ok()?;
        self.handle.symbol(&c)
    }
}

// --- platform handles ------------------------------------------------------

#[cfg(windows)]
mod imp {
    use std::ffi::CStr;
    use std::path::Path;

    use plugin_host_api::HostError;
    use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW,
    };

    pub struct Handle(HMODULE);

    impl Handle {
        pub fn open(path: &Path) -> Result<Handle, HostError> {
            use std::os::windows::ffi::OsStrExt;
            let wide: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();

            // ALTERED_SEARCH_PATH so a plugin's private DLLs, which sit next to
            // it, resolve without polluting the host's own search order.
            let h = unsafe {
                LoadLibraryExW(
                    wide.as_ptr(),
                    std::ptr::null_mut(),
                    LOAD_WITH_ALTERED_SEARCH_PATH,
                )
            };
            if h.is_null() {
                let err = std::io::Error::last_os_error();
                return Err(HostError::ModuleLoad(format!(
                    "LoadLibraryEx failed for {}: {err}",
                    path.display()
                )));
            }
            Ok(Handle(h))
        }

        pub fn symbol(&self, name: &CStr) -> Option<*mut std::ffi::c_void> {
            let f = unsafe { GetProcAddress(self.0, name.as_ptr() as *const u8) };
            f.map(|f| f as *mut std::ffi::c_void)
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                FreeLibrary(self.0);
            }
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::ffi::{CStr, CString};
    use std::path::Path;

    use plugin_host_api::HostError;

    pub struct Handle(*mut std::ffi::c_void);

    impl Handle {
        pub fn open(path: &Path) -> Result<Handle, HostError> {
            use std::os::unix::ffi::OsStrExt;
            let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
                HostError::ModuleLoad(format!("path has interior nul: {}", path.display()))
            })?;
            // RTLD_LOCAL so plugin symbols cannot collide across libraries.
            let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if h.is_null() {
                let msg = unsafe {
                    let e = libc::dlerror();
                    if e.is_null() {
                        "unknown error".to_string()
                    } else {
                        CStr::from_ptr(e).to_string_lossy().into_owned()
                    }
                };
                return Err(HostError::ModuleLoad(format!(
                    "dlopen failed for {}: {msg}",
                    path.display()
                )));
            }
            Ok(Handle(h))
        }

        pub fn symbol(&self, name: &CStr) -> Option<*mut std::ffi::c_void> {
            let p = unsafe { libc::dlsym(self.0, name.as_ptr()) };
            (!p.is_null()).then_some(p)
        }
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe {
                libc::dlclose(self.0);
            }
        }
    }
}

use imp::Handle;
