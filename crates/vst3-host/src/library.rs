//! The VST3 module entry contract and bundle layout.
//!
//! A `.vst3` is a standalone shared library or a bundle directory
//! (`Name.vst3/Contents/<arch>/Name.<ext>`). Opening the library is
//! `plugin_module`'s; what is VST3 about it — the entry and exit points — is
//! here.

use std::path::{Path, PathBuf};

use plugin_host_api::HostError;

/// An opened shared library, plus the VST3 entry/exit contract.
///
/// The exit function must run *after* every COM pointer into the module has
/// been released, so this type is only ever dropped by `Module`, which owns
/// the factory and declares it first.
pub struct Library {
    library: plugin_module::Library,
    /// Set once the entry point has succeeded, so we never call exit without a
    /// matching entry.
    entered: bool,
}

impl Library {
    /// Load the binary for `path`, which may be a bundle directory or a plain
    /// shared library, and run the VST3 module entry point.
    pub fn open(path: &Path) -> Result<Library, HostError> {
        let binary_path = resolve_binary(path)?;
        let mut lib = Library {
            library: plugin_module::Library::open(&binary_path)?,
            entered: false,
        };
        lib.enter()?;
        Ok(lib)
    }

    /// Look up an exported symbol. Returns `None` if it is absent, which is a
    /// normal outcome — the entry points are all optional in practice.
    pub(crate) fn lookup(&self, name: &str) -> Option<*mut std::ffi::c_void> {
        self.library.symbol(name)
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
                    self.library.path().display()
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

/// Map a `.vst3` path to the shared library that actually has to be loaded.
pub(crate) fn resolve_binary(path: &Path) -> Result<PathBuf, HostError> {
    let (dir, ext) = platform_dir();
    plugin_module::bundle_binary(path, dir, ext)
}

/// Where a `moduleinfo.json` would live for this path, if the plugin ships one.
pub fn moduleinfo_path(path: &Path) -> Option<PathBuf> {
    if !path.is_dir() {
        return None;
    }
    let p = path.join("Contents").join("moduleinfo.json");
    p.is_file().then_some(p)
}
