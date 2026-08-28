//! Loaded plugin instance abstraction over format-specific backends.
//!
//! [`Plugin`] implements [`SubPluginMain`] by delegating calls to the underlying
//! VST3 or CLAP instance while managing lifecycle and format details:
//!
//! - **Module lifetime:** Ensures the shared library module outlives the plugin instance.
//! - **Editor management:** Unifies VST3 `IPlugView` and CLAP GUI extensions behind
//!   `open_editor`, `close_editor`, and `tick`.
//! - **Teardown sequence:** Manages destruction order so editor views and instances
//!   are released before the module is unloaded.

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

// The enum variants differ in size due to backend-specific event buffer storage.
// Large enum variant warning is suppressed as instance count is typically small.
#[allow(clippy::large_enum_variant)]
enum Backend {
    /// VST3 backend instance.
    ///
    /// Field order defines destruction order: the editor window holding the
    /// `IPlugView` is dropped before the plugin controller and component, which
    /// in turn are dropped before unloading the module shared library.
    Vst3 {
        editor: Option<vst3_host_view::EditorWindow>,
        plugin: vst3_host::Vst3Plugin,
        /// Held to keep the shared library loaded for the lifetime of the instance.
        #[allow(dead_code)]
        module: vst3_host::Module,
    },
    /// CLAP backend instance.
    ///
    /// The plugin instance manages its own editor extension and is dropped
    /// before the underlying module shared library.
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
    /// Infers the format from the file extension.
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
    /// Returns VST3 interface names or CLAP extension strings for diagnostic and inspection purposes.
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
    /// `owner` is an optional native parent window handle to attach or float above.
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
    /// Should be called on the main UI thread once per frame to handle window resizing,
    /// editor close requests, and background timer/task callbacks (e.g. for CLAP).
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
