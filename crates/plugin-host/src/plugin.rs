//! One loaded sub-plugin, whatever format it came in.
//!
//! [`Plugin`] implements [`SubPluginMain`] by delegating to whichever backend
//! actually loaded it, so everything above this crate is written once. Three
//! things are deliberately *not* delegated, because they are where the two
//! formats differ in shape rather than in detail:
//!
//! * **Module ownership.** A VST3 instance's vtables point into the module's
//!   code, so the module has to outlive it. That is a rule the caller used to
//!   have to know; here the module is a private field and the rule is the
//!   struct's field order.
//! * **The editor.** VST3 hands out an `IPlugView` the host attaches to a
//!   window; CLAP has the editor as an extension on the instance itself. Both
//!   are hidden behind `open_editor` / `close_editor` / `tick`.
//! * **The `Drop` order.** §5.3's teardown sequence is the crash source, and it
//!   is written here once for each backend rather than at every call site.

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
// less obvious drop order — and a host holds at most a handful of these, one per
// plugin node, none of them on a hot path.
#[allow(clippy::large_enum_variant)]
enum Backend {
    /// Field order **is** the teardown order (§5.3): the editor holds an
    /// `IPlugView` created by the controller, which must be removed and
    /// released before the controller terminates; and the instance's vtables
    /// live in the module, which must not be unloaded first.
    Vst3 {
        editor: Option<vst3_host_view::EditorWindow>,
        plugin: vst3_host::Vst3Plugin,
        /// Held, not used.
        #[allow(dead_code)]
        module: vst3_host::Module,
    },
    /// CLAP needs no such care here: the instance owns its own editor, so the
    /// order is guaranteed by `ClapPlugin::drop` rather than by this
    /// declaration. The module still has to outlive the instance.
    Clap {
        plugin: clap_host::ClapPlugin,
        #[allow(dead_code)]
        module: clap_host::Module,
    },
}

impl Plugin {
    /// Load a plugin from `path`, choosing `class_id` or the module's first
    /// offering.
    ///
    /// The format comes from the extension. Passing `None` for `class_id` is
    /// what a file browser does; passing `Some` is what restoring a saved
    /// project does, because the id is the identity and the path only a hint
    /// (§8.3).
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

    // --- editor ------------------------------------------------------------

    pub fn has_editor(&self) -> bool {
        match &self.inner {
            Backend::Vst3 { plugin, .. } => plugin.has_editor(),
            Backend::Clap { plugin, .. } => plugin.has_editor(),
        }
    }

    /// Open the sub-plugin's own editor in a top-level window (ADR-3).
    ///
    /// `owner` is the window it should float above: the DAW's root window when
    /// running as a plugin, null when standalone. An ownerless window is a peer
    /// of the DAW's, so clicking in the DAW buries it.
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

    pub fn close_editor(&mut self) {
        match &mut self.inner {
            // Dropping the window runs the §5.3 sequence; there is no way to
            // close one without it.
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

    /// Drive the plugin for one UI tick.
    ///
    /// Call from the host's UI thread, once per frame, whether or not an editor
    /// is open. A plugin must not pump messages itself — the DAW is already
    /// doing that — so this only handles the parts that are ours: applying
    /// resizes, closing an editor the user dismissed, and, for CLAP, running
    /// the main-thread callbacks and timers a plugin repaints from.
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
            // Not conditional on an editor being open: CLAP's `on_main_thread`
            // and its timers exist whether or not the plugin is showing
            // anything, and a plugin starved of them stalls its own worker.
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
