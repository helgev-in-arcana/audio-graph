//! Loading a VST3 module and enumerating the classes it offers.
//!
//! This is M0: on its own it is a working plugin scanner, with no notion of
//! instantiation, audio, or the nested-wrapper use case.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use plugin_host_api::{HostError, Result};
use vst3::Steinberg::{
    IPluginFactory, IPluginFactory2, IPluginFactory2Trait, IPluginFactory3, IPluginFactory3Trait,
    IPluginFactoryTrait, PClassInfo, PClassInfo2, PClassInfoW, PFactoryInfo, kResultOk,
};
use vst3::ComPtr;

use crate::cid::Cid;
use crate::library::{self, Library};
use crate::moduleinfo::ModuleInfo;
use crate::util::{from_char16, from_char8};

/// Everything a scanner wants to know about the module as a whole.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FactoryInfo {
    pub vendor: String,
    pub url: String,
    pub email: String,
    /// The factory declared its strings as UTF-16 (`PClassInfoW` is usable).
    pub unicode: bool,
}

/// One exported class. Both processors and controllers show up here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassInfo {
    pub cid: Cid,
    /// `"Audio Module Class"`, `"Component Controller Class"`, …
    pub category: String,
    pub name: String,
    /// `|`-separated tags: `Fx`, `Instrument|Synth`, `Fx|Delay`, …
    pub subcategories: String,
    pub vendor: String,
    pub version: String,
    pub sdk_version: String,
}

impl ClassInfo {
    /// The category string for classes that can actually be instantiated as a
    /// processor. Controllers and other helpers use different categories.
    pub const AUDIO_MODULE: &'static str = "Audio Module Class";

    pub fn is_audio_module(&self) -> bool {
        self.category == Self::AUDIO_MODULE
    }

    /// Whether the class declares itself an instrument.
    ///
    /// Matters because the wrapper's own category is static while the
    /// sub-plugin's is not, which is why two wrapper classes are exported (§6).
    pub fn is_instrument(&self) -> bool {
        self.subcategories
            .split('|')
            .any(|s| s.eq_ignore_ascii_case("Instrument") || s.eq_ignore_ascii_case("Synth"))
    }
}

/// A loaded `.vst3` module.
///
/// Not `Send`/`Sync`: VST3 pins factory use to the thread that loaded it, and
/// `Rc` here makes that explicit rather than relying on a comment.
pub struct Module {
    inner: Rc<ModuleInner>,
}

/// Field order is the drop order, and the drop order is the contract: the
/// factory pointer must be released before the module's exit function runs,
/// which happens when `Library` drops.
pub(crate) struct ModuleInner {
    factory: ComPtr<IPluginFactory>,
    #[allow(dead_code)]
    library: Library,
    path: PathBuf,
}

impl Module {
    /// Load `path` (a bundle directory or a plain shared library) and obtain
    /// its plugin factory.
    pub fn open(path: impl AsRef<Path>) -> Result<Module> {
        let path = path.as_ref();
        let library = Library::open(path)?;

        let Some(sym) = library.lookup("GetPluginFactory") else {
            return Err(HostError::NoFactory(format!(
                "{} exports no GetPluginFactory",
                path.display()
            )));
        };

        let factory = unsafe {
            let get: extern "system" fn() -> *mut IPluginFactory = std::mem::transmute(sym);
            // GetPluginFactory returns an already-addref'd pointer, so this
            // takes ownership without an extra retain.
            ComPtr::from_raw(get())
        };

        let factory = factory.ok_or_else(|| {
            HostError::NoFactory(format!("GetPluginFactory returned null for {}", path.display()))
        })?;

        Ok(Module {
            inner: Rc::new(ModuleInner {
                factory,
                library,
                path: path.to_path_buf(),
            }),
        })
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    pub(crate) fn factory(&self) -> &ComPtr<IPluginFactory> {
        &self.inner.factory
    }

    /// Share the module with an instance created from it, so the library
    /// cannot be unloaded while a plugin object is still alive.
    pub(crate) fn handle(&self) -> Rc<ModuleInner> {
        Rc::clone(&self.inner)
    }

    pub fn factory_info(&self) -> Result<FactoryInfo> {
        let mut raw = PFactoryInfo {
            vendor: [0; 64],
            url: [0; 256],
            email: [0; 128],
            flags: 0,
        };
        let res = unsafe { self.inner.factory.getFactoryInfo(&mut raw) };
        if res != kResultOk {
            return Err(HostError::Backend {
                context: "IPluginFactory::getFactoryInfo".into(),
                code: res,
            });
        }
        use vst3::Steinberg::PFactoryInfo_::FactoryFlags_::kUnicode;
        Ok(FactoryInfo {
            vendor: from_char8(&raw.vendor),
            url: from_char8(&raw.url),
            email: from_char8(&raw.email),
            unicode: raw.flags & kUnicode != 0,
        })
    }

    /// Every class the factory exports, richest description available.
    ///
    /// Tries `PClassInfoW`, then `PClassInfo2`, then plain `PClassInfo`. Older
    /// plugins only implement the last of those, and a scanner that demands
    /// `IPluginFactory3` would silently skip them.
    pub fn classes(&self) -> Result<Vec<ClassInfo>> {
        let factory = &self.inner.factory;
        let count = unsafe { factory.countClasses() };
        if count < 0 {
            return Err(HostError::Backend {
                context: "IPluginFactory::countClasses".into(),
                code: count,
            });
        }

        let factory3 = factory.cast::<IPluginFactory3>();
        let factory2 = factory.cast::<IPluginFactory2>();

        let mut out = Vec::with_capacity(count as usize);
        for index in 0..count {
            if let Some(f3) = &factory3 {
                let mut raw = zeroed_class_info_w();
                if unsafe { f3.getClassInfoUnicode(index, &mut raw) } == kResultOk {
                    out.push(ClassInfo {
                        cid: Cid::from_tuid(&raw.cid),
                        category: from_char8(&raw.category),
                        name: from_char16(&raw.name),
                        subcategories: from_char8(&raw.subCategories),
                        vendor: from_char16(&raw.vendor),
                        version: from_char16(&raw.version),
                        sdk_version: from_char16(&raw.sdkVersion),
                    });
                    continue;
                }
            }

            if let Some(f2) = &factory2 {
                let mut raw = zeroed_class_info_2();
                if unsafe { f2.getClassInfo2(index, &mut raw) } == kResultOk {
                    out.push(ClassInfo {
                        cid: Cid::from_tuid(&raw.cid),
                        category: from_char8(&raw.category),
                        name: from_char8(&raw.name),
                        subcategories: from_char8(&raw.subCategories),
                        vendor: from_char8(&raw.vendor),
                        version: from_char8(&raw.version),
                        sdk_version: from_char8(&raw.sdkVersion),
                    });
                    continue;
                }
            }

            let mut raw = PClassInfo {
                cid: [0; 16],
                cardinality: 0,
                category: [0; 32],
                name: [0; 64],
            };
            let res = unsafe { factory.getClassInfo(index, &mut raw) };
            if res != kResultOk {
                return Err(HostError::Backend {
                    context: format!("IPluginFactory::getClassInfo({index})"),
                    code: res,
                });
            }
            out.push(ClassInfo {
                cid: Cid::from_tuid(&raw.cid),
                category: from_char8(&raw.category),
                name: from_char8(&raw.name),
                subcategories: String::new(),
                vendor: String::new(),
                version: String::new(),
                sdk_version: String::new(),
            });
        }

        Ok(out)
    }

    /// Audio modules only — the classes worth showing a user picking a plugin.
    pub fn audio_modules(&self) -> Result<Vec<ClassInfo>> {
        Ok(self.classes()?.into_iter().filter(ClassInfo::is_audio_module).collect())
    }

    /// The bundle's `moduleinfo.json`, when it ships one.
    ///
    /// Only useful for scanning without loading; the factory remains the
    /// authority, so nothing here depends on it being present.
    pub fn module_info(&self) -> Option<ModuleInfo> {
        let path = library::moduleinfo_path(&self.inner.path)?;
        let text = std::fs::read_to_string(path).ok()?;
        ModuleInfo::parse(&text).ok()
    }
}

impl std::fmt::Debug for Module {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Module").field("path", &self.inner.path).finish_non_exhaustive()
    }
}

/// Read `moduleinfo.json` for a bundle without loading any code.
///
/// This is the point of the file: a scanner can enumerate a plugin's classes
/// without running third-party code in its own process.
pub fn scan_without_loading(path: impl AsRef<Path>) -> Option<ModuleInfo> {
    let p = library::moduleinfo_path(path.as_ref())?;
    ModuleInfo::parse(&std::fs::read_to_string(p).ok()?).ok()
}

fn zeroed_class_info_2() -> PClassInfo2 {
    PClassInfo2 {
        cid: [0; 16],
        cardinality: 0,
        category: [0; 32],
        name: [0; 64],
        classFlags: 0,
        subCategories: [0; 128],
        vendor: [0; 64],
        version: [0; 64],
        sdkVersion: [0; 64],
    }
}

fn zeroed_class_info_w() -> PClassInfoW {
    PClassInfoW {
        cid: [0; 16],
        cardinality: 0,
        category: [0; 32],
        name: [0; 64],
        classFlags: 0,
        subCategories: [0; 128],
        vendor: [0; 64],
        version: [0; 64],
        sdkVersion: [0; 64],
    }
}
