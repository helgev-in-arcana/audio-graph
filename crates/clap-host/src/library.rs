//! Platform dynamic-library loading and CLAP bundle layout.

use std::ffi::{CString, OsStr};
use std::path::{Path, PathBuf};

use plugin_host_api::HostError;

/// An opened shared library. Knows nothing about CLAP; see
/// [`crate::module::Module`] for the entry-point contract, which has to be
/// balanced at module scope rather than here.
pub struct Library {
    handle: Handle,
    /// Path of the actual binary, not the bundle directory.
    binary_path: PathBuf,
}

impl Library {
    pub fn open(path: &Path) -> Result<Library, HostError> {
        let binary_path = resolve_binary(path)?;
        let handle = Handle::open(&binary_path)?;
        Ok(Library {
            handle,
            binary_path,
        })
    }

    pub fn binary_path(&self) -> &Path {
        &self.binary_path
    }

    /// Look up an exported symbol.
    pub fn symbol(&self, name: &str) -> Option<*mut std::ffi::c_void> {
        let c = CString::new(name).ok()?;
        self.handle.symbol(&c)
    }
}

/// Subdirectory of `Contents/` a macOS CLAP bundle keeps its binary in.
///
/// Windows and Linux have no bundle: a `.clap` there is the shared library
/// itself, which is why this is the only platform that needs the constant.
const MACOS_BUNDLE_DIR: &str = "MacOS";

/// Map a `.clap` path to the shared library that has to be loaded.
///
/// On Windows and Linux the two are the same file. On macOS the path is a
/// bundle directory, so this walks into it.
fn resolve_binary(path: &Path) -> Result<PathBuf, HostError> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    if !path.is_dir() {
        return Err(HostError::ModuleLoad(format!(
            "{} does not exist",
            path.display()
        )));
    }

    let contents = path.join("Contents").join(MACOS_BUNDLE_DIR);
    let stem = path
        .file_stem()
        .unwrap_or_else(|| OsStr::new("plugin"))
        .to_os_string();
    let named = contents.join(&stem);
    if named.is_file() {
        return Ok(named);
    }

    // The binary is normally named after the bundle, but not always; fall back
    // to the single candidate in the directory rather than failing outright.
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(&contents)
        .map_err(|e| HostError::ModuleLoad(format!("cannot read {}: {e}", contents.display())))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    candidates.sort();

    candidates
        .into_iter()
        .next()
        .ok_or_else(|| HostError::ModuleLoad(format!("no binary found in {}", contents.display())))
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
