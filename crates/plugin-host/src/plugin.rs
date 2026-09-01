//! Loaded plugin instance abstraction over format-specific backends.
//!
//! [`Plugin`] implements [`SubPluginMain`] by delegating calls to the underlying
//! VST3 or CLAP instance while managing lifecycle and format details:
//!
//! - **Module lifetime:** a VST3 instance's vtables point into the module's
//!   code, so the module has to outlive it. That used to be a rule the caller
//!   had to know; here the module is a private field and the rule is the
//!   struct's field order.
//! - **Editor management:** VST3 hands out an `IPlugView` the host attaches to
//!   a window; CLAP has the editor as an extension on the instance itself.
//!   Both are hidden behind `open_editor`, `close_editor`, and `tick`.
//! - **Teardown sequence:** getting the destruction order wrong is a crash
//!   source, so it is written here once per backend rather than at every call
//!   site.

use std::sync::Arc;

use plugin_host_api::{
    AudioConfig, Capabilities, HostContext, HostError, IoLayout, ParamId, ParamInfo, ParamSnapshot,
    Result, SubPluginMain, SubPluginProcessor,
};

use crate::format::Format;
use crate::scan::{ClassInfo, PluginRef, scan_module_as};

/// A loaded sub-plugin. Main thread only, like both backends underneath it.
pub struct Plugin {
    inner: Backend,
    class: ClassInfo,
}

// The two variants differ in size by a few hundred bytes, because a
// `ClapPlugin` carries its own event buffers. Boxing one of them would buy back
// that difference at the cost of an indirection on every main-thread call and a
// less obvious drop order — and a host holds at most a handful of these, one
// per plugin node, none of them on a hot path.
#[allow(clippy::large_enum_variant)]
enum Backend {
    /// VST3 backend instance.
    ///
    /// Field order **is** the teardown order: the editor holds an `IPlugView`
    /// created by the controller, which must be removed and released before
    /// the controller terminates; and the instance's vtables live in the
    /// module, which must not be unloaded first.
    Vst3 {
        editor: Option<vst3_host_view::EditorWindow>,
        plugin: vst3_host::Vst3Plugin,
        /// Held to keep the shared library loaded for the lifetime of the instance.
        #[allow(dead_code)]
        module: vst3_host::Module,
    },
    /// CLAP backend instance.
    ///
    /// CLAP needs no such care here: the instance owns its own editor, so that
    /// order is guaranteed by `ClapPlugin::drop` rather than by this
    /// declaration. The module still has to outlive the instance.
    Clap {
        plugin: clap_host::ClapPlugin,
        #[allow(dead_code)]
        module: clap_host::Module,
    },
}

impl Plugin {
    /// Loads a plugin from `path`, selecting the class matching `class_id` or the
    /// first available class in the module.
    ///
    /// The format comes from the extension. Passing `None` for `class_id` is
    /// what a file browser does; passing `Some` is what restoring a saved
    /// project does, because the id is the identity and the path only a hint.
    pub fn load(
        path: &std::path::Path,
        class_id: Option<&str>,
        context: Arc<dyn HostContext>,
    ) -> Result<Plugin> {
        let format = Format::from_path(path).ok_or_else(|| {
            HostError::ModuleLoad(format!("{} is not a plugin module", path.display()))
        })?;
        Plugin::load_as(format, path, class_id, context)
    }

    /// As [`Plugin::load`], for a caller that already knows the format.
    pub fn load_as(
        format: Format,
        path: &std::path::Path,
        class_id: Option<&str>,
        context: Arc<dyn HostContext>,
    ) -> Result<Plugin> {
        let classes = scan_module_as(format, path)?;
        let class = match class_id {
            Some(id) => classes.into_iter().find(|c| c.id == id).ok_or_else(|| {
                HostError::ClassNotFound(format!("{} has no {id}", path.display()))
            })?,
            None => classes.into_iter().next().ok_or_else(|| {
                HostError::ClassNotFound(format!("{} exports no plugin", path.display()))
            })?,
        };

        let inner = match format {
            Format::Vst3 => {
                let module = vst3_host::Module::open(path)?;
                let cid = vst3_host::Cid::from_hex(&class.id).ok_or_else(|| {
                    HostError::ClassNotFound(format!("{} is not a class id", class.id))
                })?;
                let plugin = vst3_host::Vst3Plugin::create(&module, cid, context)?;
                Backend::Vst3 {
                    editor: None,
                    plugin,
                    module,
                }
            }
            Format::Clap => {
                let module = clap_host::Module::open(path)?;
                let plugin = clap_host::ClapPlugin::create(&module, &class.id, context)?;
                Backend::Clap { plugin, module }
            }
        };

        Ok(Plugin { inner, class })
    }

    pub fn format(&self) -> Format {
        self.class.format
    }

    /// What was loaded, for display and for saving.
    pub fn class(&self) -> &ClassInfo {
        &self.class
    }

    pub fn reference(&self) -> PluginRef {
        self.class.reference()
    }

    pub fn name(&self) -> &str {
        &self.class.name
    }

    /// Returns format-specific interface or extension identifiers implemented by this plugin.
    ///
    /// The one place the facade hands back something format-specific on
    /// purpose. It is a diagnostic for `host-cli info`, not something to
    /// branch on: a caller matching on these strings would be reintroducing
    /// the format split this crate exists to contain. The strings are not
    /// stable and must never be persisted.
    pub fn format_interfaces(&self) -> Vec<&'static str> {
        match &self.inner {
            Backend::Vst3 { plugin, .. } => plugin.interfaces(),
            Backend::Clap { plugin, .. } => plugin.extensions(),
        }
    }

    // --- editor ------------------------------------------------------------

    pub fn has_editor(&self) -> bool {
        match &self.inner {
            Backend::Vst3 { plugin, .. } => plugin.has_editor(),
            Backend::Clap { plugin, .. } => plugin.has_editor(),
        }
    }

    /// Opens the plugin's graphical editor in a window.
    ///
    /// `owner` is the window it should float above: the DAW's root window
    /// when running as a plugin, null when standalone. An ownerless window is
    /// a peer of the DAW's, so clicking in the DAW would bury it.
    pub fn open_editor(&mut self, owner: *mut std::ffi::c_void) -> std::result::Result<(), String> {
        match &mut self.inner {
            Backend::Vst3 {
                editor,
                plugin,
                module: _,
            } => {
                if editor.is_some() {
                    return Ok(());
                }
                let view = plugin.create_view().ok_or("this plugin has no editor")?;
                *editor = Some(vst3_host_view::EditorWindow::open(
                    view,
                    &self.class.name,
                    owner,
                )?);
                Ok(())
            }
            Backend::Clap { plugin, .. } => plugin.open_editor(owner),
        }
    }

    /// Closes the plugin's graphical editor window if open.
    pub fn close_editor(&mut self) {
        match &mut self.inner {
            // Dropping the window runs the teardown sequence; there is no way
            // to close one without it.
            Backend::Vst3 { editor, .. } => *editor = None,
            Backend::Clap { plugin, .. } => plugin.close_editor(),
        }
    }

    pub fn editor_is_open(&self) -> bool {
        match &self.inner {
            Backend::Vst3 { editor, .. } => editor.is_some(),
            Backend::Clap { plugin, .. } => plugin.editor_is_open(),
        }
    }

    /// The editor's container window, for a harness that pumps messages itself.
    pub fn editor_window(&self) -> Option<&host_window::ContainerWindow> {
        match &self.inner {
            Backend::Vst3 { editor, .. } => {
                editor.as_ref().map(vst3_host_view::EditorWindow::window)
            }
            Backend::Clap { plugin, .. } => plugin.editor_window(),
        }
    }

    /// Performs periodic main-thread maintenance for the plugin and editor.
    ///
    /// Call from the host's UI thread once per frame, whether or not an
    /// editor is open. A plugin must not pump messages itself — the DAW is
    /// already doing that — so this only handles the parts that are ours:
    /// applying resizes, closing an editor the user dismissed, and, for CLAP,
    /// running the main-thread callbacks and timers a plugin repaints from.
    pub fn tick(&mut self) {
        match &mut self.inner {
            Backend::Vst3 { editor, .. } => {
                let Some(window) = editor.as_mut() else {
                    return;
                };
                window.sync_size();
                if window.close_requested() {
                    *editor = None;
                }
            }
            // Not conditional on an editor being open: CLAP's
            // `on_main_thread` and its timers exist whether or not the plugin
            // is showing anything, and a plugin starved of them stalls its own
            // worker.
            Backend::Clap { plugin, .. } => plugin.tick(),
        }
    }
}

macro_rules! delegate {
    ($self:ident, $plugin:ident => $body:expr) => {
        match &$self.inner {
            Backend::Vst3 {
                plugin: $plugin, ..
            } => $body,
            Backend::Clap {
                plugin: $plugin, ..
            } => $body,
        }
    };
    (mut $self:ident, $plugin:ident => $body:expr) => {
        match &mut $self.inner {
            Backend::Vst3 {
                plugin: $plugin, ..
            } => $body,
            Backend::Clap {
                plugin: $plugin, ..
            } => $body,
        }
    };
}

impl SubPluginMain for Plugin {
    fn params(&self) -> &[ParamInfo] {
        delegate!(self, p => SubPluginMain::params(p))
    }

    fn capabilities(&self) -> Capabilities {
        delegate!(self, p => SubPluginMain::capabilities(p))
    }

    fn voice_info(&self) -> Option<plugin_host_api::VoiceInfo> {
        delegate!(self, p => SubPluginMain::voice_info(p))
    }

    fn note_dialects(&self) -> Vec<&'static str> {
        delegate!(self, p => SubPluginMain::note_dialects(p))
    }

    fn io_layout(&self) -> IoLayout {
        delegate!(self, p => SubPluginMain::io_layout(p))
    }

    fn snapshot(&self) -> ParamSnapshot {
        delegate!(self, p => SubPluginMain::snapshot(p))
    }

    fn param_to_text(&self, id: ParamId, plain: f64) -> Option<String> {
        delegate!(self, p => SubPluginMain::param_to_text(p, id, plain))
    }

    fn param_from_text(&self, id: ParamId, text: &str) -> Option<f64> {
        delegate!(self, p => SubPluginMain::param_from_text(p, id, text))
    }

    fn set_param(&mut self, id: ParamId, plain: f64) -> Result<()> {
        delegate!(mut self, p => SubPluginMain::set_param(p, id, plain))
    }

    fn save_state(&self) -> Result<Vec<u8>> {
        delegate!(self, p => SubPluginMain::save_state(p))
    }

    fn load_state(&mut self, data: &[u8]) -> Result<()> {
        delegate!(mut self, p => SubPluginMain::load_state(p, data))
    }

    fn latency_samples(&self) -> u32 {
        delegate!(self, p => SubPluginMain::latency_samples(p))
    }

    fn activate(&mut self, config: AudioConfig) -> Result<Box<dyn SubPluginProcessor>> {
        delegate!(mut self, p => SubPluginMain::activate(p, config))
    }

    fn deactivate(&mut self, processor: Box<dyn SubPluginProcessor>) {
        delegate!(mut self, p => SubPluginMain::deactivate(p, processor))
    }
}

impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("format", &self.class.format)
            .field("name", &self.class.name)
            .finish_non_exhaustive()
    }
}
