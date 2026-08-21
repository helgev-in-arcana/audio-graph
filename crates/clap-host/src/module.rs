//! Loading a `.clap` module and enumerating the plugins it offers.
//!
//! The CLAP counterpart of `vst3-host::module`, and structurally the same
//! shape: a reference-counted handle onto one loaded library, cached by path so
//! the entry point runs exactly once (ADR-7), plus a batched read of everything
//! a scanner wants to know.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};

use clap_sys::entry::clap_plugin_entry;
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::plugin::clap_plugin_descriptor;
use clap_sys::version::clap_version_is_compatible;
use plugin_host_api::{HostError, Result};

use crate::library::Library;
use crate::util::from_cstr;

/// One plugin exported by a module.
///
/// CLAP has no notion of a class category the way VST3 does — everything a
/// factory exports is a plugin — so `is_audio_module` is unconditionally true
/// and exists only so callers can be written once against both backends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassInfo {
    /// The plugin's reverse-DNS identity, e.g. `com.surge-synth-team.surge-xt`.
    /// Stable across versions and machines; this is what a saved binding
    /// records (ARCHITECTURE.md §8.3).
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    pub description: String,
    /// The plugin's declared feature tags: `audio-effect`, `instrument`,
    /// `synthesizer`, `stereo`, …
    pub features: Vec<String>,
}

impl ClassInfo {
    /// Whether the plugin declares itself an instrument.
    ///
    /// Matters for the same reason as its VST3 twin: the wrapper's own category
    /// is static while the sub-plugin's is not (§6).
    pub fn is_instrument(&self) -> bool {
        self.features
            .iter()
            .any(|f| f == "instrument" || f == "synthesizer" || f == "sampler" || f == "drum")
    }

    /// Present so callers can treat both backends alike; see the type comment.
    pub fn is_audio_module(&self) -> bool {
        true
    }
}

/// Everything a scanner wants to know about the module as a whole.
///
/// CLAP puts vendor and URL on each plugin rather than on the factory, so this
/// is the first plugin's answer — enough to group a module in a browser, which
/// is all a factory-level record is for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FactoryInfo {
    pub vendor: String,
    pub url: String,
    /// CLAP version the module was built against.
    pub clap_version: (u32, u32, u32),
}

/// A loaded `.clap` module.
///
/// Not `Send`/`Sync`: `clap_plugin_entry::init` and the factory calls are
/// `[main-thread]` in the format's own annotations, and `Rc` here makes that a
/// property of the type rather than a comment.
pub struct Module {
    inner: Rc<ModuleInner>,
}

/// Field order is the drop order, and the drop order is the contract: `deinit`
/// must run before the library is unmapped.
pub(crate) struct ModuleInner {
    /// Borrowed from the library, valid until it is unloaded.
    entry: *const clap_plugin_entry,
    factory: *const clap_plugin_factory,
    library: Library,
    path: PathBuf,
}

impl Drop for ModuleInner {
    fn drop(&mut self) {
        // Balances the `init` in `Module::open`. The factory pointer belongs to
        // the module and must not be touched afterwards, which is why nothing
        // here releases it separately.
        unsafe {
            if let Some(deinit) = (*self.entry).deinit {
                deinit();
            }
        }
        // `library` drops next, unmapping the code `entry` pointed into.
        let _ = &self.library;
    }
}

thread_local! {
    /// Modules already loaded on this thread, keyed by canonical path.
    ///
    /// `clap_entry.init` must be balanced exactly once per module no matter how
    /// many times a host asks for it — the same rule VST3's `InitDll` has, and
    /// the same reason ADR-7 exists. `Weak`, so a module is genuinely unloaded
    /// once nothing refers to it.
    static LOADED: RefCell<HashMap<PathBuf, Weak<ModuleInner>>> =
        RefCell::new(HashMap::new());
}

impl Module {
    /// Load `path` and obtain its plugin factory.
    ///
    /// Opening the same path twice returns handles onto one underlying module,
    /// so the entry point runs once.
    pub fn open(path: impl AsRef<Path>) -> Result<Module> {
        let path = path.as_ref();
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

        if let Some(existing) =
            LOADED.with(|loaded| loaded.borrow().get(&key).and_then(Weak::upgrade))
        {
            return Ok(Module { inner: existing });
        }

        let library = Library::open(path)?;

        let Some(sym) = library.symbol("clap_entry") else {
            return Err(HostError::NoFactory(format!(
                "{} exports no clap_entry",
                path.display()
            )));
        };
        let entry = sym as *const clap_plugin_entry;

        // Checked before anything is called through it: a module built against
        // a future major version has a layout we cannot read, and reading it
        // anyway turns a scan into a crash.
        let version = unsafe { (*entry).clap_version };
        if !clap_version_is_compatible(version) {
            return Err(HostError::ModuleLoad(format!(
                "{} declares CLAP {}.{}.{}, which this host cannot read",
                path.display(),
                version.major,
                version.minor,
                version.revision
            )));
        }

        // CLAP hands the module its own path so it can find its resources. The
        // *bundle* path, not the binary inside it, is what the spec names.
        let c_path = CString::new(path.to_string_lossy().as_ref())
            .map_err(|_| HostError::ModuleLoad("path has an interior nul".into()))?;
        let init_ok = match unsafe { (*entry).init } {
            Some(init) => unsafe { init(c_path.as_ptr()) },
            // `init` is not optional in the spec, but a null here is a plugin
            // bug we can survive; refusing to load would be worse.
            None => true,
        };
        if !init_ok {
            return Err(HostError::ModuleLoad(format!(
                "clap_entry.init returned false for {}",
                path.display()
            )));
        }

        let factory = match unsafe { (*entry).get_factory } {
            Some(get) => {
                (unsafe { get(CLAP_PLUGIN_FACTORY_ID.as_ptr()) }) as *const clap_plugin_factory
            }
            None => std::ptr::null(),
        };
        if factory.is_null() {
            // Balance the init that just ran; nothing else will.
            unsafe {
                if let Some(deinit) = (*entry).deinit {
                    deinit();
                }
            }
            return Err(HostError::NoFactory(format!(
                "{} has no {} factory",
                path.display(),
                CLAP_PLUGIN_FACTORY_ID.to_string_lossy()
            )));
        }

        let inner = Rc::new(ModuleInner {
            entry,
            factory,
            library,
            path: path.to_path_buf(),
        });
        LOADED.with(|loaded| loaded.borrow_mut().insert(key, Rc::downgrade(&inner)));

        Ok(Module { inner })
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// The binary actually loaded — the same file as [`Module::path`] except on
    /// macOS, where the path is a bundle directory.
    pub fn binary_path(&self) -> &Path {
        self.inner.library.binary_path()
    }

    pub(crate) fn factory(&self) -> *const clap_plugin_factory {
        self.inner.factory
    }

    /// Share the module with an instance created from it, so the library
    /// cannot be unloaded while a plugin object is still alive.
    pub(crate) fn handle(&self) -> Rc<ModuleInner> {
        Rc::clone(&self.inner)
    }

    /// Every plugin the factory exports.
    pub fn classes(&self) -> Result<Vec<ClassInfo>> {
        let factory = self.inner.factory;
        let count = match unsafe { (*factory).get_plugin_count } {
            Some(f) => unsafe { f(factory) },
            None => 0,
        };
        let Some(get) = (unsafe { (*factory).get_plugin_descriptor }) else {
            return Ok(Vec::new());
        };

        let mut out = Vec::with_capacity(count as usize);
        for index in 0..count {
            let desc = unsafe { get(factory, index) };
            if desc.is_null() {
                continue;
            }
            if let Some(info) = unsafe { describe(desc) } {
                out.push(info);
            }
        }
        Ok(out)
    }

    /// Present for symmetry with `vst3-host`: every CLAP plugin is one.
    pub fn audio_modules(&self) -> Result<Vec<ClassInfo>> {
        self.classes()
    }

    pub fn factory_info(&self) -> Result<FactoryInfo> {
        let version = unsafe { (*self.inner.entry).clap_version };
        let factory = self.inner.factory;
        let first = unsafe { (*factory).get_plugin_descriptor }.and_then(|get| {
            let desc = unsafe { get(factory, 0) };
            (!desc.is_null()).then_some(desc)
        });
        let (vendor, url) = match first {
            Some(desc) => unsafe { (from_cstr((*desc).vendor), from_cstr((*desc).url)) },
            None => (String::new(), String::new()),
        };
        Ok(FactoryInfo {
            vendor,
            url,
            clap_version: (version.major, version.minor, version.revision),
        })
    }
}

/// Read one descriptor, rejecting anything built against an unreadable version.
///
/// # Safety
/// `desc` must point at a live `clap_plugin_descriptor`.
unsafe fn describe(desc: *const clap_plugin_descriptor) -> Option<ClassInfo> {
    let d = unsafe { &*desc };
    if !clap_version_is_compatible(d.clap_version) {
        return None;
    }
    let id = unsafe { from_cstr(d.id) };
    if id.is_empty() {
        // The id is the identity a binding is saved against (§8.3). A plugin
        // without one cannot be found again, so it is not offerable.
        return None;
    }
    Some(ClassInfo {
        id,
        name: unsafe { from_cstr(d.name) },
        vendor: unsafe { from_cstr(d.vendor) },
        version: unsafe { from_cstr(d.version) },
        description: unsafe { from_cstr(d.description) },
        features: unsafe { read_features(d.features) },
    })
}

/// How many feature tags one plugin may declare before the array is treated as
/// unterminated. A plugin whose array has no NULL would otherwise walk off the
/// end of its own memory and take the scan with it.
const MAX_FEATURES: usize = 64;

/// Read the NULL-terminated array of feature strings.
///
/// # Safety
/// `features` must be null or a NULL-terminated array of C strings.
unsafe fn read_features(features: *const *const c_char) -> Vec<String> {
    if features.is_null() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut at = features;
    for _ in 0..MAX_FEATURES {
        let p = unsafe { *at };
        if p.is_null() {
            break;
        }
        out.push(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned());
        at = unsafe { at.add(1) };
    }
    out
}

impl std::fmt::Debug for Module {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Module")
            .field("path", &self.inner.path)
            .finish_non_exhaustive()
    }
}
