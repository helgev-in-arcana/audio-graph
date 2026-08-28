//! Platform dynamic-library loading and VST3 bundle layout.
//!
//! Handles loading `.vst3` binaries, whether laid out as a standalone shared
//! library or as a bundle directory (`Name.vst3/Contents/<arch>/Name.<ext>`).

use std::ffi::{CString, OsStr};
use std::path::{Path, PathBuf};

use plugin_host_api::HostError;

/// An opened shared library, plus the VST3 entry/exit contract.
///
/// The exit function must run *after* every COM pointer into the module has
/// been released, so this type is only ever dropped by `Module`, which owns
/// the factory and declares it first.
pub struct Library {
    handle: Handle,
    /// Path of the actual binary, not the bundle directory.
    binary_path: PathBuf,
    /// Set once the entry point has succeeded, so we never call exit without a
    /// matching entry.
    entered: bool,
}

impl Library {
    /// Load the binary for `path`, which may be a bundle directory or a plain
    /// shared library, and run the VST3 module entry point.
    pub fn open(path: &Path) -> Result<Library, HostError> {
        let binary_path = resolve_binary(path)?;
        let handle = Handle::open(&binary_path)?;
        let mut lib = Library {
            handle,
            binary_path,
            entered: false,
        };
        lib.enter()?;
        Ok(lib)
    }

    /// Look up an exported symbol. Returns `None` if it is absent, which is a
    /// normal outcome — the entry points are all optional in practice.
    pub(crate) fn lookup(&self, name: &str) -> Option<*mut std::ffi::c_void> {
        let c = CString::new(name).ok()?;
        self.handle.symbol(&c)
    }

    fn enter(&mut self) -> Result<(), HostError> {
        // Naming differs per platform, and plugins built from older SDKs may
        // omit the entry point entirely, in which case there is nothing to do.
        #[cfg(target_os = "windows")]
        let names = ["InitDll"];
        #[cfg(target_os = "macos")]
        let names = ["bundleEntry"];
        #[cfg(all(unix, not(target_os = "macos")))]
        let names = ["ModuleEntry"];

        for name in names {
            let Some(sym) = self.lookup(name) else {
                continue;
            };

            let ok = unsafe {
                #[cfg(target_os = "windows")]
                {
                    let f: extern "system" fn() -> bool = std::mem::transmute(sym);
                    f()
                }
                #[cfg(unix)]
                {
                    // Both `bundleEntry` and `ModuleEntry` take the platform
                    // handle for the module. The SDK passes a `CFBundleRef` on
                    // macOS; plugins use it only to locate their own resources
                    // and tolerate null, which is what a dlopen-based loader
                    // can offer.
                    let f: extern "C" fn(*mut std::ffi::c_void) -> bool = std::mem::transmute(sym);
                    f(std::ptr::null_mut())
                }
            };

            if !ok {
                return Err(HostError::ModuleLoad(format!(
                    "{name} returned false for {}",
                    self.binary_path.display()
                )));
            }
            self.entered = true;
            return Ok(());
        }

        Ok(())
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        if self.entered {
            #[cfg(target_os = "windows")]
            let names = ["ExitDll"];
            #[cfg(target_os = "macos")]
            let names = ["bundleExit"];
            #[cfg(all(unix, not(target_os = "macos")))]
            let names = ["ModuleExit"];

            for name in names {
                if let Some(sym) = self.lookup(name) {
                    unsafe {
                        let f: extern "system" fn() -> bool = std::mem::transmute(sym);
                        f();
                    }
                    break;
                }
            }
        }
    }
}

/// Subdirectory of `Contents/` for the architecture we were built for, and the
/// extension the binary inside it carries.
const fn platform_dir() -> (&'static str, &'static str) {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        ("x86_64-win", "vst3")
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64"))]
    {
        ("arm64-win", "vst3")
    }
    #[cfg(all(target_os = "windows", target_arch = "x86"))]
    {
        ("x86-win", "vst3")
    }
    #[cfg(target_os = "macos")]
    {
        ("MacOS", "")
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        ("x86_64-linux", "so")
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        ("aarch64-linux", "so")
    }
}

/// Map a `.vst3` path to the shared library that actually has to be `dlopen`ed.
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

    let (dir, ext) = platform_dir();
    let contents = path.join("Contents").join(dir);

    // The binary is normally named after the bundle, but not always; fall back
    // to the single candidate in the directory rather than failing outright.
    let stem = path
        .file_stem()
        .unwrap_or_else(|| OsStr::new("plugin"))
        .to_os_string();
    let named = if ext.is_empty() {
        contents.join(&stem)
    } else {
        contents.join(&stem).with_extension(ext)
    };
    if named.is_file() {
        return Ok(named);
    }

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

/// Where a `moduleinfo.json` would live for this path, if the plugin ships one.
pub fn moduleinfo_path(path: &Path) -> Option<PathBuf> {
    if !path.is_dir() {
        return None;
    }
    let p = path.join("Contents").join("moduleinfo.json");
    p.is_file().then_some(p)
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
            // it in the bundle, resolve without polluting the host's own search
            // order.
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
            // RTLD_LOCAL ensures plugin symbols do not pollute the host or
            // collide with other plugins' symbol tables.
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
