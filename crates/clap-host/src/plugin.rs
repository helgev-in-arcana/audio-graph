//! Instantiating a CLAP plugin and driving it.
//!
//! The lifecycle is short next to VST3's, and the format states it plainly:
//!
//! ```text
//! create_plugin(factory, host, id) -> init() -> [read extensions]
//!   -> activate(rate, min, max) -> start_processing() -> process...
//!   -> stop_processing() -> deactivate() -> destroy()
//! ```
//!
//! What CLAP does *not* have is bus negotiation. A plugin's audio ports are
//! whatever it declares; there is no `setBusArrangements` to argue with. That
//! moves the work from persuading the plugin to matching what it already is,
//! which is why [`PortLayout`] is read once at load and treated as authority
//! from then on.

use std::cell::{Cell, RefCell};
use std::ffi::{CString, c_char};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::ext::audio_ports::{
    CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS, clap_audio_port_info, clap_plugin_audio_ports,
};
use clap_sys::ext::audio_ports_activation::{
    CLAP_EXT_AUDIO_PORTS_ACTIVATION, CLAP_EXT_AUDIO_PORTS_ACTIVATION_COMPAT,
    clap_plugin_audio_ports_activation,
};
use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_plugin_gui};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_plugin_latency};
use clap_sys::ext::note_ports::{
    CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_CLAP, clap_note_port_info, clap_plugin_note_ports,
};
use clap_sys::ext::params::{
    CLAP_EXT_PARAMS, CLAP_PARAM_IS_AUTOMATABLE, CLAP_PARAM_IS_BYPASS, CLAP_PARAM_IS_HIDDEN,
    CLAP_PARAM_IS_MODULATABLE, CLAP_PARAM_IS_MODULATABLE_PER_CHANNEL,
    CLAP_PARAM_IS_MODULATABLE_PER_KEY, CLAP_PARAM_IS_MODULATABLE_PER_NOTE_ID,
    CLAP_PARAM_IS_PERIODIC, CLAP_PARAM_IS_READONLY, CLAP_PARAM_IS_STEPPED, clap_param_info,
    clap_plugin_params,
};
use clap_sys::ext::render::{
    CLAP_EXT_RENDER, CLAP_RENDER_OFFLINE, CLAP_RENDER_REALTIME, clap_plugin_render,
};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use clap_sys::plugin::clap_plugin;
use clap_sys::process::{
    CLAP_PROCESS_ERROR, CLAP_PROCESS_SLEEP, clap_process, clap_process_status,
};
use clap_sys::string_sizes::CLAP_NAME_SIZE;
use plugin_host_api::{
    AudioBuffers, AudioConfig, BusInfo, Capabilities, Event, EventSink, HostContext, HostError,
    IoLayout, ParamFlags, ParamId, ParamInfo, ParamSnapshot, ParamValue, ProcessStatus, Result,
    SubPluginMain, SubPluginProcessor, TimeContext,
};

use crate::events::{InputEvents, OutputEvents, to_transport};
use crate::gui::ClapEditor;
use crate::host::{AudioThreadGuard, HostShim, PendingRequests};
use crate::module::{ClassInfo, Module, ModuleInner};
use crate::stream::{InStream, OutStream};
use crate::util::from_char_array;

/// Per-block ceilings for the pre-allocated event buffers.
///
/// Fixed rather than derived, for the same reason as the VST3 backend's: they
/// have to be decided before any audio runs, and the sub-block quantiser
/// (§14.9) bounds how many points one block can carry.
const MAX_EVENTS_PER_BLOCK: usize = 2048;

/// How long a parameter's formatted text may be.
///
/// CLAP leaves it to the host; every plugin in practice writes far less, and a
/// value that needs more than this to display is not one a knob can show.
const PARAM_TEXT_CAPACITY: usize = 256;

/// One audio port a plugin declares.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Port {
    name: String,
    channels: u16,
    is_main: bool,
}

/// Every audio port of one plugin, in declaration order.
///
/// Read once at load and never re-read while an instance is active: CLAP allows
/// a plugin to rescan its ports, but only while inactive, and the host is told
/// through `clap.audio-ports` when it happens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PortLayout {
    inputs: Vec<Port>,
    outputs: Vec<Port>,
}

/// Where one declared port's audio comes from, or goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Binding {
    /// Channels start at this offset in the caller's flat region.
    Caller(usize),
    /// The graph wired nothing here. Inputs read silence; outputs are written
    /// and thrown away.
    ///
    /// CLAP has no inactive port — `audio_inputs_count` is the declared count
    /// and a plugin will read every entry — so an unwired port is given real
    /// memory rather than a null pointer.
    Scratch(usize),
}

/// A loaded, initialised CLAP plugin instance. Main thread only.
pub struct ClapPlugin {
    /// Keeps the module (and therefore the code behind every pointer below)
    /// alive for as long as the instance exists.
    _module: Rc<ModuleInner>,
    /// Declared before `plugin` so it outlives it: the instance holds the raw
    /// pointer it was created with and may call back during `destroy`.
    host: Box<HostShim>,
    plugin: *const clap_plugin,

    class: ClassInfo,
    params: Vec<ParamInfo>,
    ports: PortLayout,
    note_inputs: usize,
    note_outputs: usize,
    /// True when the plugin's note input speaks CLAP's own dialect, which is
    /// what per-voice work needs (§3.2).
    clap_notes: bool,

    ext_params: *const clap_plugin_params,
    ext_state: *const clap_plugin_state,
    ext_latency: *const clap_plugin_latency,
    ext_gui: *const clap_plugin_gui,
    /// Optional. Lets the host say whether this is a live take or an offline
    /// bounce; see `set_render_mode`.
    ext_render: *const clap_plugin_render,
    /// Optional. Lets the host tell a plugin that a port is not worth
    /// computing; see `set_port_activation`.
    ext_ports_activation: *const clap_plugin_audio_ports_activation,

    /// The editor lives here rather than in the caller, so `destroy` cannot run
    /// before the GUI is torn down no matter what the caller forgets (§5.3).
    editor: Option<ClapEditor>,

    active: Cell<bool>,
    latency: Cell<u32>,
    /// Main-thread edits waiting for the processor, exactly as the VST3 backend
    /// keeps them: CLAP delivers values only through events, so an edit made
    /// while audio is running has to ride the next block.
    pending_edits: Arc<Mutex<Vec<(ParamId, f64)>>>,
    /// Event buffers for a main-thread `params.flush`, which is the inactive
    /// path for the same edits.
    flush_buffers: RefCell<(InputEvents, OutputEvents)>,
    /// Values already handed over by `flush`, kept so `activate` can hand them
    /// over a second time as ordinary events.
    ///
    /// CLAP says a flush is enough and most plugins agree, but Surge XT Effects
    /// takes the value into what `get_value` reports and not into its DSP, so a
    /// preset set while the plugin was inactive is silently lost. Re-sending on
    /// the first block costs one event per edited parameter and makes the
    /// difference invisible from outside.
    ///
    /// Dropped by `load_state`, so whichever of the two happened last wins, and
    /// emptied by the activate that replays it, so a value the audio thread has
    /// since changed is not undone by a later activate.
    flushed: RefCell<Vec<(ParamId, f64)>>,
    context: Arc<dyn HostContext>,
}

impl ClapPlugin {
    /// Create and fully initialise the plugin `id` from `module`.
    pub fn create(module: &Module, id: &str, context: Arc<dyn HostContext>) -> Result<ClapPlugin> {
        let class = module
            .classes()?
            .into_iter()
            .find(|c| c.id == id)
            .ok_or_else(|| HostError::ClassNotFound(format!("{id} is not in this module")))?;

        let host = HostShim::new(Arc::clone(&context));
        let c_id = CString::new(id)
            .map_err(|_| HostError::ClassNotFound("plugin id has an interior nul".into()))?;

        let factory = module.factory();
        let create = unsafe { (*factory).create_plugin }
            .ok_or_else(|| HostError::NoFactory("factory cannot create plugins".into()))?;
        let plugin = unsafe { create(factory, host.raw(), c_id.as_ptr()) };
        if plugin.is_null() {
            return Err(HostError::ClassNotFound(format!(
                "create_plugin returned null for {id}"
            )));
        }
        host.set_plugin(plugin);

        // From here on the instance has to be destroyed on every failure path.
        let init_ok = unsafe { (*plugin).init }.is_none_or(|f| unsafe { f(plugin) });
        if !init_ok {
            unsafe {
                if let Some(destroy) = (*plugin).destroy {
                    destroy(plugin);
                }
            }
            return Err(HostError::ModuleLoad(format!(
                "clap_plugin::init returned false for {id}"
            )));
        }

        // Extensions are only valid to ask for after `init`, and the pointers a
        // plugin returns are stable for its lifetime — so they are read once
        // here rather than on every call.
        let ext_params = unsafe { extension::<clap_plugin_params>(plugin, CLAP_EXT_PARAMS) };
        let ext_state = unsafe { extension::<clap_plugin_state>(plugin, CLAP_EXT_STATE) };
        let ext_latency = unsafe { extension::<clap_plugin_latency>(plugin, CLAP_EXT_LATENCY) };
        let ext_gui = unsafe { extension::<clap_plugin_gui>(plugin, CLAP_EXT_GUI) };
        let ext_render = unsafe { extension::<clap_plugin_render>(plugin, CLAP_EXT_RENDER) };
        let ext_audio_ports =
            unsafe { extension::<clap_plugin_audio_ports>(plugin, CLAP_EXT_AUDIO_PORTS) };
        let ext_note_ports =
            unsafe { extension::<clap_plugin_note_ports>(plugin, CLAP_EXT_NOTE_PORTS) };
        // Both spellings: the id gained a version suffix when the extension
        // left draft, and plugins in the wild answer to either.
        let mut ext_ports_activation = unsafe {
            extension::<clap_plugin_audio_ports_activation>(plugin, CLAP_EXT_AUDIO_PORTS_ACTIVATION)
        };
        if ext_ports_activation.is_null() {
            ext_ports_activation = unsafe {
                extension::<clap_plugin_audio_ports_activation>(
                    plugin,
                    CLAP_EXT_AUDIO_PORTS_ACTIVATION_COMPAT,
                )
            };
        }

        let params = unsafe { read_params(plugin, ext_params) };
        let ports = unsafe { read_ports(plugin, ext_audio_ports) };
        let (note_inputs, note_outputs, clap_notes) =
            unsafe { read_note_ports(plugin, ext_note_ports) };

        Ok(ClapPlugin {
            _module: module.handle(),
            host,
            plugin,
            class,
            params,
            ports,
            note_inputs,
            note_outputs,
            clap_notes,
            ext_params,
            ext_state,
            ext_latency,
            ext_gui,
            ext_render,
            ext_ports_activation,
            editor: None,
            active: Cell::new(false),
            latency: Cell::new(0),
            pending_edits: Arc::new(Mutex::new(Vec::with_capacity(MAX_EVENTS_PER_BLOCK))),
            flush_buffers: RefCell::new((
                InputEvents::new(MAX_EVENTS_PER_BLOCK),
                OutputEvents::new(MAX_EVENTS_PER_BLOCK),
            )),
            flushed: RefCell::new(Vec::new()),
            context,
        })
    }

    pub fn class(&self) -> &ClassInfo {
        &self.class
    }

    /// Which of the extensions we know the names of this instance answers to.
    ///
    /// Purely diagnostic — nothing in the host branches on this. It exists so
    /// `host-cli info` can say what a module actually implements, which is the
    /// cheapest way to find out whether a real plugin exercises a code path we
    /// have only ever run against `clap-test-plugin`.
    ///
    /// The list is deliberately wider than what this backend supports: an
    /// extension we do *not* implement is the interesting result, because it
    /// names work a plugin is asking for and not getting.
    pub fn extensions(&self) -> Vec<&'static str> {
        KNOWN_EXTENSIONS
            .iter()
            .filter(|id| {
                // Safety: the instance is live for as long as `self` is, and
                // `get_extension` is a main-thread call, which this is.
                !unsafe { extension::<()>(self.plugin, id) }.is_null()
            })
            .map(|id| id.to_str().expect("the ids are ASCII literals"))
            .collect()
    }

    pub fn params(&self) -> &[ParamInfo] {
        &self.params
    }

    /// The plugin's declared buses and note capability (§14.2).
    pub fn io_layout(&self) -> IoLayout {
        let bus = |p: &Port| BusInfo {
            name: p.name.clone(),
            channels: p.channels,
            // CLAP marks the bus a plugin actually processes; everything else
            // is auxiliary, which is exactly the core's distinction.
            is_aux: !p.is_main,
        };
        IoLayout {
            inputs: self.ports.inputs.iter().map(bus).collect(),
            outputs: self.ports.outputs.iter().map(bus).collect(),
            accepts_notes: self.note_inputs > 0,
            emits_notes: self.note_outputs > 0,
        }
    }

    /// Whether the plugin offers an editor at all.
    pub fn has_editor(&self) -> bool {
        !self.ext_gui.is_null()
    }

    /// Whether the open editor can be resized by the user.
    pub fn editor_can_resize(&self) -> bool {
        !self.ext_gui.is_null() && unsafe { crate::gui::can_resize(self.plugin, self.ext_gui) }
    }

    /// Open the plugin's own editor in a top-level window (ADR-3).
    ///
    /// `owner` is the window it should float above: the DAW's root window when
    /// running as a plugin, null when standalone.
    // `owner` is a window handle, not memory: it is passed straight to the
    // platform's window creation call and never read by this crate. Clippy
    // cannot see the difference, and making the function `unsafe` would put the
    // obligation on every caller for a pointer none of them dereference either.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn open_editor(&mut self, owner: *mut std::ffi::c_void) -> std::result::Result<(), String> {
        if self.editor.is_some() {
            return Ok(());
        }
        if self.ext_gui.is_null() {
            return Err("this plugin has no editor".into());
        }
        let title = self.class.name.clone();
        // SAFETY: both pointers belong to this instance, and `self.editor` is
        // dropped in `Drop` before `destroy` runs.
        let editor = unsafe { ClapEditor::open(self.plugin, self.ext_gui, &title, owner) }?;
        self.editor = Some(editor);
        Ok(())
    }

    pub fn close_editor(&mut self) {
        // Dropping runs the §5.3 sequence; there is no way to close one without
        // it.
        self.editor = None;
    }

    pub fn editor_is_open(&self) -> bool {
        self.editor.is_some()
    }

    /// The container window of the open editor, for a harness that needs to
    /// pump messages itself.
    pub fn editor_window(&self) -> Option<&host_window::ContainerWindow> {
        self.editor.as_ref().map(ClapEditor::window)
    }

    /// Drive one UI tick: run the plugin's main-thread callbacks and timers,
    /// apply anything it asked for, and close the editor if the user did.
    ///
    /// Call from the host's UI thread. This is the CLAP half of what
    /// `subhost-adapter` calls on every frame, and skipping it makes an editor
    /// look frozen — plugins repaint from a timer, not from a paint message.
    pub fn tick(&mut self) {
        let requests = self.host.take_requests();
        self.apply(requests);
        self.host.tick_timers();

        if let Some(editor) = self.editor.as_mut() {
            editor.sync_size();
            if editor.close_requested() {
                self.editor = None;
            }
        }
    }

    /// Act on everything the plugin asked for since the last tick.
    fn apply(&mut self, requests: PendingRequests) {
        if requests.callback
            && let Some(on_main_thread) = unsafe { (*self.plugin).on_main_thread }
        {
            unsafe { on_main_thread(self.plugin) };
        }

        if requests.param_rescan != 0 {
            // Re-read wholesale rather than by flag: the list is small, and
            // reading half of it because only `TEXT` was set is how a stale
            // range survives a plugin update.
            self.params = unsafe { read_params(self.plugin, self.ext_params) };
        }

        if requests.latency {
            let latency = unsafe { read_latency(self.plugin, self.ext_latency) };
            if latency != self.latency.get() {
                self.latency.set(latency);
                self.context.latency_changed(latency);
            }
        }

        if let Some((width, height)) = requests.gui_resize
            && let Some(editor) = self.editor.as_mut()
        {
            editor.apply_requested_resize(width, height);
        }

        if requests.gui_closed {
            self.editor = None;
        }

        // `restart`, `process` and `audio_ports` are deliberately not acted on
        // here: they mean deactivate/activate, and only the owner of the audio
        // graph knows when that is safe. The context was told when the request
        // arrived (see `host.rs`), which is the path §7.4 describes.
    }

    /// Tell the plugin which of its ports the graph actually wired.
    ///
    /// `bind_ports` already gives every unwired port real memory, because CLAP
    /// has no null buffer — but memory is not the cost that matters. A plugin
    /// with an unused sidechain still filters it, and one with unused aux
    /// outputs still renders them; Surge XT computes both of its scene outputs
    /// whether or not anything reads them. This extension is the only way to
    /// say "do not bother", and it is the only thing in this backend that saves
    /// the plugin work rather than teaching us about it.
    ///
    /// Every port is set explicitly rather than only the unwired ones: ports
    /// start active, but an instance outlives a deactivate/activate pair, so a
    /// port turned off for one configuration would stay off for the next.
    ///
    /// Called with the plugin deactivated, which is when CLAP allows this
    /// unconditionally — so `can_activate_while_processing` never has to be
    /// asked.
    fn set_port_activation(&self, plan: &BindingPlan) {
        if self.ext_ports_activation.is_null() {
            return;
        }
        let Some(set_active) = (unsafe { (*self.ext_ports_activation).set_active }) else {
            return;
        };

        let apply = |is_input: bool, bindings: &[(u16, Binding)]| {
            for (index, (_, binding)) in bindings.iter().enumerate() {
                let active = matches!(binding, Binding::Caller(_));
                // The sample size is the buffer's, in bits, and zero when the
                // port is being turned off. This backend is `f32` throughout.
                let sample_size = if active { 32 } else { 0 };
                let ok =
                    unsafe { set_active(self.plugin, is_input, index as u32, active, sample_size) };
                if !ok {
                    // Refusing is allowed, and means the port stays as it was.
                    // Nothing downstream depends on the answer: the buffers are
                    // bound either way.
                    log::debug!(
                        "{}: refused to set {} port {index} {}",
                        self.class.name,
                        if is_input { "input" } else { "output" },
                        if active { "active" } else { "inactive" },
                    );
                }
            }
        };

        apply(true, &plan.inputs);
        apply(false, &plan.outputs);
    }

    /// Tell the plugin whether this is a live take or an offline bounce.
    ///
    /// The VST3 side has always said so — `processMode` is part of the setup
    /// struct — and a plugin that is told will often spend the extra cycles it
    /// would never risk in a live take: longer FFTs, more oversampling, a
    /// smoother interpolator.
    ///
    /// Set at every activate rather than once, and set explicitly in both
    /// directions: the mode is a property of the instance, not of the run, so
    /// an instance bounced offline and then played live would otherwise stay
    /// in offline mode.
    ///
    /// A plugin with a hard realtime requirement — an analog modelling plugin
    /// tied to wall-clock, a hardware bridge — is left alone. CLAP says such a
    /// plugin cannot be used offline at all; that is the caller's problem to
    /// have, and refusing to switch it is the least surprising thing to do.
    fn set_render_mode(&self, offline: bool) {
        if self.ext_render.is_null() {
            return;
        }
        let Some(set) = (unsafe { (*self.ext_render).set }) else {
            return;
        };
        if offline
            && let Some(hard) = unsafe { (*self.ext_render).has_hard_realtime_requirement }
            && unsafe { hard(self.plugin) }
        {
            log::debug!(
                "{}: has a hard realtime requirement; left in realtime mode",
                self.class.name
            );
            return;
        }

        let mode = if offline {
            CLAP_RENDER_OFFLINE
        } else {
            CLAP_RENDER_REALTIME
        };
        if !unsafe { set(self.plugin, mode) } {
            // Refusing is allowed and costs nothing: the plugin simply renders
            // the way it always does.
            log::debug!("{}: refused render mode {mode}", self.class.name);
        }
    }

    /// Deliver queued main-thread edits without a process call.
    ///
    /// The inactive half of `set_param`. While audio is running the processor
    /// drains the same queue, so this must not be called then — CLAP says
    /// `flush` and `process` may never overlap.
    fn flush_params(&self) {
        if self.ext_params.is_null() || self.active.get() {
            return;
        }
        let Some(flush) = (unsafe { (*self.ext_params).flush }) else {
            return;
        };
        let Ok(mut pending) = self.pending_edits.lock() else {
            return;
        };
        if pending.is_empty() {
            return;
        }
        let mut buffers = self.flush_buffers.borrow_mut();
        let (input, output) = &mut *buffers;
        input.clear();
        output.clear();
        let mut flushed = self.flushed.borrow_mut();
        for (id, plain) in pending.drain(..) {
            input.push_param(id, plain, 0);
            flushed.retain(|(existing, _)| *existing != id);
            flushed.push((id, plain));
        }
        drop(flushed);
        drop(pending);
        let in_raw = input.as_raw();
        let out_raw = output.as_raw();
        unsafe { flush(self.plugin, in_raw, out_raw) };
        output.clear();
    }
}

impl SubPluginMain for ClapPlugin {
    fn params(&self) -> &[ParamInfo] {
        &self.params
    }

    fn io_layout(&self) -> IoLayout {
        ClapPlugin::io_layout(self)
    }

    fn capabilities(&self) -> Capabilities {
        // Probed, not fixed: this is the format that actually has these, and
        // which of them a given plugin offers is per-parameter (§3.3).
        let any = |flag: ParamFlags| self.params.iter().any(|p| p.flags.contains(flag));
        Capabilities {
            modulation: any(ParamFlags::MODULATABLE),
            poly_modulation: any(ParamFlags::POLY_MODULATABLE),
            // CLAP note expressions ride the plugin's note input, and only the
            // CLAP dialect carries them; a MIDI-only port cannot.
            note_expression: self.note_inputs > 0 && self.clap_notes,
            // CLAP plugins may add and remove parameters and tell the host
            // through `clap.params`, which `tick` acts on.
            dynamic_params: true,
        }
    }

    fn snapshot(&self) -> ParamSnapshot {
        if self.ext_params.is_null() {
            return ParamSnapshot::default();
        }
        let Some(get_value) = (unsafe { (*self.ext_params).get_value }) else {
            return ParamSnapshot::default();
        };
        ParamSnapshot {
            values: self
                .params
                .iter()
                .filter_map(|p| {
                    let mut value = 0.0;
                    unsafe { get_value(self.plugin, p.id.0, &mut value) }.then_some(ParamValue {
                        id: p.id,
                        plain: value,
                    })
                })
                .collect(),
        }
    }

    fn param_to_text(&self, id: ParamId, plain: f64) -> Option<String> {
        if self.ext_params.is_null() {
            return None;
        }
        let to_text = unsafe { (*self.ext_params).value_to_text }?;
        let mut buf = vec![0 as c_char; PARAM_TEXT_CAPACITY];
        unsafe { to_text(self.plugin, id.0, plain, buf.as_mut_ptr(), buf.len() as u32) }
            .then(|| from_char_array(&buf))
    }

    fn param_from_text(&self, id: ParamId, text: &str) -> Option<f64> {
        if self.ext_params.is_null() {
            return None;
        }
        let from_text = unsafe { (*self.ext_params).text_to_value }?;
        let c_text = CString::new(text).ok()?;
        let mut value = 0.0;
        unsafe { from_text(self.plugin, id.0, c_text.as_ptr(), &mut value) }.then_some(value)
    }

    fn set_param(&mut self, id: ParamId, plain: f64) -> Result<()> {
        if !self.params.iter().any(|p| p.id == id) {
            return Err(HostError::InvalidState("no such parameter"));
        }
        // CLAP has no setter: a value reaches the plugin only as an event, and
        // the only question is whether it rides a `flush` or the next block.
        if let Ok(mut pending) = self.pending_edits.lock() {
            pending.retain(|(existing, _)| *existing != id);
            if pending.len() < pending.capacity() {
                pending.push((id, plain));
            }
        }
        self.flush_params();
        Ok(())
    }

    fn save_state(&self) -> Result<Vec<u8>> {
        if self.ext_state.is_null() {
            // A plugin with nothing to save is not an error; the wrapper stores
            // an empty blob and hands it back unchanged.
            return Ok(Vec::new());
        }
        let save = unsafe { (*self.ext_state).save }
            .ok_or_else(|| HostError::State("the plugin's state extension has no save".into()))?;
        let mut stream = OutStream::new();
        let raw = stream.as_raw();
        if !unsafe { save(self.plugin, raw) } {
            return Err(HostError::State("clap_plugin_state::save failed".into()));
        }
        Ok(stream.into_bytes())
    }

    fn load_state(&mut self, data: &[u8]) -> Result<()> {
        if self.ext_state.is_null() {
            return if data.is_empty() {
                Ok(())
            } else {
                Err(HostError::State(
                    "the plugin has no state extension to load into".into(),
                ))
            };
        }
        let load = unsafe { (*self.ext_state).load }
            .ok_or_else(|| HostError::State("the plugin's state extension has no load".into()))?;
        let mut stream = InStream::new(data);
        let raw = stream.as_raw();
        if !unsafe { load(self.plugin, raw) } {
            return Err(HostError::State("clap_plugin_state::load failed".into()));
        }
        // The blob is newer than any edit made before it, so nothing may be
        // replayed over the top of it.
        self.flushed.borrow_mut().clear();
        if let Ok(mut pending) = self.pending_edits.lock() {
            pending.clear();
        }
        Ok(())
    }

    fn latency_samples(&self) -> u32 {
        self.latency.get()
    }

    fn activate(&mut self, config: AudioConfig) -> Result<Box<dyn SubPluginProcessor>> {
        if self.active.get() {
            return Err(HostError::InvalidState("plugin is already active"));
        }
        // Anything queued while inactive has to reach the plugin before it
        // starts, or the first block renders with the old values.
        self.flush_params();
        // And again on the first block, for a plugin that took the flush into
        // its parameter cache but not into its DSP. See `flushed`.
        {
            // Drained, not copied: this is a one-shot. Once the plugin has been
            // run, a value that arrived as an ordinary event during `process`
            // is newer than anything flushed before it, and replaying the old
            // one on the next activate would undo the user's last edit.
            let mut flushed = self.flushed.borrow_mut();
            if !flushed.is_empty()
                && let Ok(mut pending) = self.pending_edits.lock()
            {
                for (id, plain) in flushed.drain(..) {
                    pending.retain(|(existing, _)| *existing != id);
                    if pending.len() < pending.capacity() {
                        pending.push((id, plain));
                    }
                }
            }
        }

        let plan = bind_ports(&self.ports, &config)?;
        // Before `activate`, which is when CLAP allows it unconditionally.
        self.set_port_activation(&plan);
        self.set_render_mode(config.offline);

        let activate = unsafe { (*self.plugin).activate }.ok_or_else(|| HostError::Backend {
            context: "clap_plugin::activate is null".into(),
            code: 0,
        })?;
        // A minimum of one frame, not of `max_block_size`: the sub-block
        // quantiser hands out short blocks (§14.9), and a plugin told its
        // minimum is 512 is entitled to refuse them.
        if !unsafe { activate(self.plugin, config.sample_rate, 1, config.max_block_size) } {
            return Err(HostError::UnsupportedBusConfig(format!(
                "the plugin refused {} Hz with blocks up to {} frames",
                config.sample_rate, config.max_block_size
            )));
        }
        self.active.set(true);

        // Latency is only meaningful once activated, which is why it is read
        // here rather than at construction.
        let latency = unsafe { read_latency(self.plugin, self.ext_latency) };
        self.latency.set(latency);

        if let Some(start) = unsafe { (*self.plugin).start_processing }
            && !unsafe { start(self.plugin) }
        {
            unsafe {
                if let Some(deactivate) = (*self.plugin).deactivate {
                    deactivate(self.plugin);
                }
            }
            self.active.set(false);
            return Err(HostError::Backend {
                context: "clap_plugin::start_processing".into(),
                code: 0,
            });
        }

        self.context.latency_changed(latency);

        Ok(Box::new(ClapProcessor::new(
            self.plugin,
            config,
            plan,
            Arc::clone(&self.pending_edits),
        )))
    }

    fn deactivate(&mut self, processor: Box<dyn SubPluginProcessor>) {
        // Dropped first, so nothing is still holding buffers the plugin is
        // about to be told it no longer has.
        drop(processor);
        unsafe {
            if let Some(stop) = (*self.plugin).stop_processing {
                stop(self.plugin);
            }
            if let Some(deactivate) = (*self.plugin).deactivate {
                deactivate(self.plugin);
            }
        }
        self.active.set(false);
    }
}

impl Drop for ClapPlugin {
    fn drop(&mut self) {
        // The editor goes first — see the field comment. Explicit rather than
        // left to field order, because the order *is* the contract (§5.3).
        self.editor = None;

        if self.active.get() {
            unsafe {
                if let Some(stop) = (*self.plugin).stop_processing {
                    stop(self.plugin);
                }
                if let Some(deactivate) = (*self.plugin).deactivate {
                    deactivate(self.plugin);
                }
            }
            self.active.set(false);
        }
        unsafe {
            if let Some(destroy) = (*self.plugin).destroy {
                destroy(self.plugin);
            }
        }
        // `host` drops after this, which is why it is declared before `plugin`:
        // the instance may call back into it while destroying.
    }
}

/// How every declared port is wired for one activation.
#[derive(Debug)]
struct BindingPlan {
    inputs: Vec<(u16, Binding)>,
    outputs: Vec<(u16, Binding)>,
    /// Channels of silence needed for unwired input ports.
    silence_channels: usize,
    /// Channels of discard buffer needed for unused output ports.
    scratch_channels: usize,
}

/// Match the caller's requested configuration against what the plugin declares.
///
/// CLAP cannot be argued with — a port is the width it says it is — so a
/// mismatch on a bus the graph actually uses is refused here rather than
/// papered over. Silently adapting a width is how a compressor ends up ducking
/// against half a sidechain.
fn bind_ports(ports: &PortLayout, config: &AudioConfig) -> Result<BindingPlan> {
    let mut inputs = Vec::with_capacity(ports.inputs.len());
    let mut silence_channels = 0usize;

    // The caller's input region is the main bus followed by each aux bus,
    // packed (§4.3), so the offsets are cumulative in that order.
    let mut caller_offset = 0usize;
    let mut aux = config.aux_inputs.iter();
    for (index, port) in ports.inputs.iter().enumerate() {
        let wanted = if index == 0 {
            config.input_channels
        } else {
            aux.next().map_or(0, u32::from)
        };
        if wanted == 0 {
            // Nothing wired here. The port still exists and still gets memory.
            inputs.push((port.channels, Binding::Scratch(silence_channels)));
            silence_channels += port.channels as usize;
            continue;
        }
        if wanted != u32::from(port.channels) {
            return Err(HostError::UnsupportedBusConfig(format!(
                "input port {index} ({}) is {} channels, not {wanted}",
                port.name, port.channels
            )));
        }
        inputs.push((port.channels, Binding::Caller(caller_offset)));
        caller_offset += port.channels as usize;
    }
    // An aux bus the graph wired but the plugin does not have is a graph the
    // compiler should never have produced.
    if aux.next().is_some() {
        return Err(HostError::UnsupportedBusConfig(format!(
            "the graph wired more aux inputs than the plugin's {} input port(s)",
            ports.inputs.len()
        )));
    }

    let mut outputs = Vec::with_capacity(ports.outputs.len());
    let mut scratch_channels = 0usize;
    for (index, port) in ports.outputs.iter().enumerate() {
        // Only the main output reaches the graph today (§14.2 lists the rest as
        // untested); the others are written somewhere harmless.
        if index == 0 && config.output_channels > 0 {
            if config.output_channels != u32::from(port.channels) {
                return Err(HostError::UnsupportedBusConfig(format!(
                    "output port 0 ({}) is {} channels, not {}",
                    port.name, port.channels, config.output_channels
                )));
            }
            outputs.push((port.channels, Binding::Caller(0)));
            continue;
        }
        outputs.push((port.channels, Binding::Scratch(scratch_channels)));
        scratch_channels += port.channels as usize;
    }

    // An effect the graph routed audio into that declares no output at all
    // cannot contribute anything, and every buffer below would be empty.
    if config.output_channels > 0 && ports.outputs.is_empty() {
        return Err(HostError::UnsupportedBusConfig(
            "the plugin declares no audio output port".into(),
        ));
    }

    Ok(BindingPlan {
        inputs,
        outputs,
        silence_channels,
        scratch_channels,
    })
}

/// Audio-thread half. Holds the instance pointer and nothing the main thread
/// touches except the edit queue, which it only ever *tries* to lock.
pub struct ClapProcessor {
    plugin: *const clap_plugin,
    config: AudioConfig,

    /// One entry per declared port, in declaration order — which is what
    /// `audio_inputs_count` means and what a plugin will read to the end of.
    in_buffers: Vec<clap_audio_buffer>,
    out_buffers: Vec<clap_audio_buffer>,
    in_bindings: Vec<(u16, Binding)>,
    out_bindings: Vec<(u16, Binding)>,

    /// Channel pointer arrays, one contiguous run per direction. Sized at
    /// activate; a block only refreshes the pointers.
    in_ptrs: Vec<*mut f32>,
    out_ptrs: Vec<*mut f32>,
    /// Zeroes for unwired input ports. Never written by anyone.
    silence: Vec<f32>,
    /// Somewhere for unused output ports to be written and forgotten.
    scratch: Vec<f32>,

    in_events: InputEvents,
    out_events: OutputEvents,
    pending_edits: Arc<Mutex<Vec<(ParamId, f64)>>>,

    /// Samples processed since activation, which is what CLAP's `steady_time`
    /// means. `-1` would mean "the host does not know", and we do.
    steady_time: i64,
}

// SAFETY: CLAP designates `process` as the audio-thread call, and the two-trait
// split of §4.2 is what guarantees only this half ever crosses to that thread.
unsafe impl Send for ClapProcessor {}

impl ClapProcessor {
    fn new(
        plugin: *const clap_plugin,
        config: AudioConfig,
        plan: BindingPlan,
        pending_edits: Arc<Mutex<Vec<(ParamId, f64)>>>,
    ) -> ClapProcessor {
        let frames = config.max_block_size as usize;
        let in_channels: usize = plan.inputs.iter().map(|(c, _)| *c as usize).sum();
        let out_channels: usize = plan.outputs.iter().map(|(c, _)| *c as usize).sum();

        ClapProcessor {
            plugin,
            config,
            in_buffers: plan.inputs.iter().map(|(c, _)| empty_buffer(*c)).collect(),
            out_buffers: plan.outputs.iter().map(|(c, _)| empty_buffer(*c)).collect(),
            in_bindings: plan.inputs,
            out_bindings: plan.outputs,
            in_ptrs: vec![std::ptr::null_mut(); in_channels],
            out_ptrs: vec![std::ptr::null_mut(); out_channels],
            silence: vec![0.0; plan.silence_channels * frames],
            scratch: vec![0.0; plan.scratch_channels * frames],
            in_events: InputEvents::new(MAX_EVENTS_PER_BLOCK),
            out_events: OutputEvents::new(MAX_EVENTS_PER_BLOCK),
            pending_edits,
            steady_time: 0,
        }
    }
}

impl SubPluginProcessor for ClapProcessor {
    fn process(
        &mut self,
        buffers: &mut AudioBuffers<'_>,
        events: &[Event],
        context: &TimeContext,
        out_events: &mut EventSink,
    ) -> ProcessStatus {
        let frames = buffers.frame_count();
        if frames == 0 {
            return ProcessStatus::Continue;
        }
        if frames > self.config.max_block_size {
            // Louder than a silent clamp: the caller broke the contract it
            // agreed to at activate, and clamping would drop audio quietly.
            return ProcessStatus::Error;
        }

        self.in_events.clear();
        self.out_events.clear();
        out_events.clear();

        // Main-thread edits go in first, at offset 0, so this block's own
        // event stream still overrides them.
        if let Ok(mut pending) = self.pending_edits.try_lock() {
            for (id, plain) in pending.drain(..) {
                self.in_events.push_param(id, plain, 0);
            }
        }
        for event in events {
            self.in_events.push(event);
        }
        // CLAP requires `in_events` sorted by time and does not check; the
        // caller's stream is sorted, but the edits just prepended are not
        // necessarily before it.
        self.in_events.sort();

        let frame_len = frames as usize;
        let input_base = buffers.raw_input().as_ptr();
        let silence_base = self.silence.as_mut_ptr();
        let mut at = 0usize;
        for (index, (channels, binding)) in self.in_bindings.iter().enumerate() {
            let width = *channels as usize;
            let base = match *binding {
                // Cast away const: CLAP declares input buffers mutable, and the
                // contract forbids writing them.
                Binding::Caller(offset) => unsafe { input_base.add(offset * frame_len) }.cast_mut(),
                Binding::Scratch(offset) => unsafe { silence_base.add(offset * frame_len) },
            };
            for channel in 0..width {
                self.in_ptrs[at + channel] = unsafe { base.add(channel * frame_len) };
            }
            let buffer = &mut self.in_buffers[index];
            buffer.data32 = unsafe { self.in_ptrs.as_mut_ptr().add(at) };
            // An unwired port reads the silence buffer, which never changes.
            // Saying so lets a plugin skip it, and a port deactivated through
            // `set_port_activation` is required to be marked constant.
            buffer.constant_mask = match binding {
                Binding::Caller(_) => 0,
                Binding::Scratch(_) => (1u64 << width) - 1,
            };
            at += width;
        }

        let output_base = buffers.raw_output_mut().as_mut_ptr();
        let scratch_base = self.scratch.as_mut_ptr();
        let mut at = 0usize;
        for (index, (channels, binding)) in self.out_bindings.iter().enumerate() {
            let width = *channels as usize;
            let base = match *binding {
                Binding::Caller(offset) => unsafe { output_base.add(offset * frame_len) },
                Binding::Scratch(offset) => unsafe { scratch_base.add(offset * frame_len) },
            };
            for channel in 0..width {
                self.out_ptrs[at + channel] = unsafe { base.add(channel * frame_len) };
            }
            let buffer = &mut self.out_buffers[index];
            buffer.data32 = unsafe { self.out_ptrs.as_mut_ptr().add(at) };
            buffer.constant_mask = 0;
            at += width;
        }

        let transport = to_transport(context, self.config.sample_rate);
        let in_raw = self.in_events.as_raw();
        let out_raw = self.out_events.as_raw();
        let data = clap_process {
            steady_time: self.steady_time,
            frames_count: frames,
            transport: &transport,
            audio_inputs: self.in_buffers.as_ptr(),
            audio_outputs: self.out_buffers.as_mut_ptr(),
            audio_inputs_count: self.in_buffers.len() as u32,
            audio_outputs_count: self.out_buffers.len() as u32,
            in_events: in_raw,
            out_events: out_raw,
        };

        let Some(process) = (unsafe { (*self.plugin).process }) else {
            buffers.clear_output();
            return ProcessStatus::Error;
        };
        let status: clap_process_status = {
            // Marks the thread for `clap.thread-check` for exactly the duration
            // of the call, which is what the format's annotation means.
            let _guard = AudioThreadGuard::enter();
            unsafe { process(self.plugin, &data) }
        };

        self.steady_time += i64::from(frames);
        self.out_events.drain_into(out_events);

        match status {
            CLAP_PROCESS_ERROR => {
                buffers.clear_output();
                ProcessStatus::Error
            }
            // Sleep is the format's "silent, and it will stay that way until
            // something arrives" — exactly what `Silent` means (§4.2). The
            // other statuses all mean the tail is still running.
            CLAP_PROCESS_SLEEP => ProcessStatus::Silent,
            _ => ProcessStatus::Continue,
        }
    }

    fn reset(&mut self) {
        // Unlike VST3, the format has a call for exactly this.
        if let Some(reset) = unsafe { (*self.plugin).reset } {
            let _guard = AudioThreadGuard::enter();
            unsafe { reset(self.plugin) };
        }
        self.steady_time = 0;
    }
}

fn empty_buffer(channels: u16) -> clap_audio_buffer {
    clap_audio_buffer {
        data32: std::ptr::null_mut(),
        // 32-bit only. Every plugin must support it, and the graph's own
        // buffers are `f32` (§4.3), so offering 64 would only add a conversion.
        data64: std::ptr::null_mut(),
        channel_count: u32::from(channels),
        latency: 0,
        constant_mask: 0,
    }
}

/// Every extension id `clap-sys` knows a name for, draft aliases included.
///
/// Taken from the bindings rather than written out here, so the spelling —
/// including the version suffixes CLAP puts in some ids, like
/// `clap.surround/4` — cannot drift from what plugins actually answer to.
///
/// Host-side ids are in the list too. A plugin has no reason to answer
/// `clap.log`, so if one ever does, that is worth seeing rather than hiding.
const KNOWN_EXTENSIONS: [&std::ffi::CStr; 46] = [
    clap_sys::ext::ambisonic::CLAP_EXT_AMBISONIC,
    clap_sys::ext::ambisonic::CLAP_EXT_AMBISONIC_COMPAT,
    clap_sys::ext::audio_ports::CLAP_EXT_AUDIO_PORTS,
    clap_sys::ext::audio_ports_activation::CLAP_EXT_AUDIO_PORTS_ACTIVATION,
    clap_sys::ext::audio_ports_activation::CLAP_EXT_AUDIO_PORTS_ACTIVATION_COMPAT,
    clap_sys::ext::audio_ports_config::CLAP_EXT_AUDIO_PORTS_CONFIG,
    clap_sys::ext::audio_ports_config::CLAP_EXT_AUDIO_PORTS_CONFIG_INFO,
    clap_sys::ext::audio_ports_config::CLAP_EXT_AUDIO_PORTS_CONFIG_INFO_COMPAT,
    clap_sys::ext::configurable_audio_ports::CLAP_EXT_CONFIGURABLE_AUDIO_PORTS,
    clap_sys::ext::configurable_audio_ports::CLAP_EXT_CONFIGURABLE_AUDIO_PORTS_COMPAT,
    clap_sys::ext::context_menu::CLAP_EXT_CONTEXT_MENU,
    clap_sys::ext::context_menu::CLAP_EXT_CONTEXT_MENU_COMPAT,
    clap_sys::ext::draft::extensible_audio_ports::CLAP_EXT_EXTENSIBLE_AUDIO_PORTS,
    clap_sys::ext::draft::resource_directory::CLAP_EXT_RESOURCE_DIRECTORY,
    clap_sys::ext::draft::transport_control::CLAP_EXT_TRANSPORT_CONTROL,
    clap_sys::ext::draft::triggers::CLAP_EXT_TRIGGERS,
    clap_sys::ext::draft::tuning::CLAP_EXT_TUNING,
    clap_sys::ext::draft::undo::CLAP_EXT_UNDO,
    clap_sys::ext::draft::undo::CLAP_EXT_UNDO_CONTEXT,
    clap_sys::ext::draft::undo::CLAP_EXT_UNDO_DELTA,
    clap_sys::ext::event_registry::CLAP_EXT_EVENT_REGISTRY,
    clap_sys::ext::gui::CLAP_EXT_GUI,
    clap_sys::ext::latency::CLAP_EXT_LATENCY,
    clap_sys::ext::log::CLAP_EXT_LOG,
    clap_sys::ext::note_name::CLAP_EXT_NOTE_NAME,
    clap_sys::ext::note_ports::CLAP_EXT_NOTE_PORTS,
    clap_sys::ext::param_indication::CLAP_EXT_PARAM_INDICATION,
    clap_sys::ext::param_indication::CLAP_EXT_PARAM_INDICATION_COMPAT,
    clap_sys::ext::params::CLAP_EXT_PARAMS,
    clap_sys::ext::posix_fd_support::CLAP_EXT_POSIX_FD_SUPPORT,
    clap_sys::ext::preset_load::CLAP_EXT_PRESET_LOAD,
    clap_sys::ext::preset_load::CLAP_EXT_PRESET_LOAD_COMPAT,
    clap_sys::ext::remote_controls::CLAP_EXT_REMOTE_CONTROLS,
    clap_sys::ext::remote_controls::CLAP_EXT_REMOTE_CONTROLS_COMPAT,
    clap_sys::ext::render::CLAP_EXT_RENDER,
    clap_sys::ext::state::CLAP_EXT_STATE,
    clap_sys::ext::state_context::CLAP_EXT_STATE_CONTEXT,
    clap_sys::ext::surround::CLAP_EXT_SURROUND,
    clap_sys::ext::surround::CLAP_EXT_SURROUND_COMPAT,
    clap_sys::ext::tail::CLAP_EXT_TAIL,
    clap_sys::ext::thread_check::CLAP_EXT_THREAD_CHECK,
    clap_sys::ext::thread_pool::CLAP_EXT_THREAD_POOL,
    clap_sys::ext::timer_support::CLAP_EXT_TIMER_SUPPORT,
    clap_sys::ext::track_info::CLAP_EXT_TRACK_INFO,
    clap_sys::ext::track_info::CLAP_EXT_TRACK_INFO_COMPAT,
    clap_sys::ext::voice_info::CLAP_EXT_VOICE_INFO,
];

/// Fetch one extension, or null.
///
/// # Safety
/// `plugin` must be a live, initialised instance.
unsafe fn extension<T>(plugin: *const clap_plugin, id: &std::ffi::CStr) -> *const T {
    match unsafe { (*plugin).get_extension } {
        Some(get) => unsafe { get(plugin, id.as_ptr()) }.cast::<T>(),
        None => std::ptr::null(),
    }
}

/// # Safety
/// `plugin` must be live; `ext` may be null.
unsafe fn read_latency(plugin: *const clap_plugin, ext: *const clap_plugin_latency) -> u32 {
    if ext.is_null() {
        return 0;
    }
    match unsafe { (*ext).get } {
        Some(get) => unsafe { get(plugin) },
        None => 0,
    }
}

/// Read the plugin's parameter list into the core's model.
///
/// No conversion: CLAP is already plain values with an explicit range, which is
/// the shape ADR-4 chose for exactly this reason.
///
/// # Safety
/// `plugin` must be live; `ext` may be null.
unsafe fn read_params(
    plugin: *const clap_plugin,
    ext: *const clap_plugin_params,
) -> Vec<ParamInfo> {
    if ext.is_null() {
        return Vec::new();
    }
    let (Some(count), Some(get_info)) = (unsafe { ((*ext).count, (*ext).get_info) }) else {
        return Vec::new();
    };

    let total = unsafe { count(plugin) };
    let mut out = Vec::with_capacity(total as usize);
    for index in 0..total {
        let mut raw: clap_param_info = unsafe { std::mem::zeroed() };
        if !unsafe { get_info(plugin, index, &mut raw) } {
            continue;
        }

        let mut flags = ParamFlags::NONE;
        flags.set(ParamFlags::STEPPED, raw.flags & CLAP_PARAM_IS_STEPPED != 0);
        flags.set(
            ParamFlags::PERIODIC,
            raw.flags & CLAP_PARAM_IS_PERIODIC != 0,
        );
        flags.set(ParamFlags::HIDDEN, raw.flags & CLAP_PARAM_IS_HIDDEN != 0);
        flags.set(
            ParamFlags::READONLY,
            raw.flags & CLAP_PARAM_IS_READONLY != 0,
        );
        flags.set(ParamFlags::BYPASS, raw.flags & CLAP_PARAM_IS_BYPASS != 0);
        // Deliberately not CLAP's own `IS_AUTOMATABLE`: the wrapper writes a
        // parameter through events either way, and a plugin that marks a
        // control non-automatable but modulatable is still drivable (ADR-5).
        // What the flag gates in the UI is whether a socket is offered.
        flags.set(
            ParamFlags::AUTOMATABLE,
            raw.flags & CLAP_PARAM_IS_AUTOMATABLE != 0,
        );
        flags.set(
            ParamFlags::MODULATABLE,
            raw.flags & CLAP_PARAM_IS_MODULATABLE != 0,
        );
        flags.set(
            ParamFlags::POLY_MODULATABLE,
            raw.flags
                & (CLAP_PARAM_IS_MODULATABLE_PER_NOTE_ID
                    | CLAP_PARAM_IS_MODULATABLE_PER_KEY
                    | CLAP_PARAM_IS_MODULATABLE_PER_CHANNEL)
                != 0,
        );

        out.push(ParamInfo {
            id: ParamId(raw.id),
            name: from_char_array(&raw.name),
            module: from_char_array(&raw.module),
            min: raw.min_value,
            max: raw.max_value,
            default: raw.default_value,
            flags,
        });
    }
    out
}

/// # Safety
/// `plugin` must be live; `ext` may be null.
unsafe fn read_ports(
    plugin: *const clap_plugin,
    ext: *const clap_plugin_audio_ports,
) -> PortLayout {
    if ext.is_null() {
        return PortLayout::default();
    }
    let (Some(count), Some(get)) = (unsafe { ((*ext).count, (*ext).get) }) else {
        return PortLayout::default();
    };

    let side = |is_input: bool| -> Vec<Port> {
        let total = unsafe { count(plugin, is_input) };
        (0..total)
            .filter_map(|index| {
                let mut raw: clap_audio_port_info = unsafe { std::mem::zeroed() };
                if !unsafe { get(plugin, index, is_input, &mut raw) } {
                    return None;
                }
                Some(Port {
                    name: from_char_array(&raw.name[..CLAP_NAME_SIZE]),
                    channels: raw.channel_count.min(u32::from(u16::MAX)) as u16,
                    // Port 0 is the main bus by convention even when the flag is
                    // absent; plugins that only have one often do not set it.
                    is_main: raw.flags & CLAP_AUDIO_PORT_IS_MAIN != 0 || index == 0,
                })
            })
            .collect()
    };

    PortLayout {
        inputs: side(true),
        outputs: side(false),
    }
}

/// Note port counts, and whether the input speaks CLAP's own note dialect.
///
/// # Safety
/// `plugin` must be live; `ext` may be null.
unsafe fn read_note_ports(
    plugin: *const clap_plugin,
    ext: *const clap_plugin_note_ports,
) -> (usize, usize, bool) {
    if ext.is_null() {
        return (0, 0, false);
    }
    let (Some(count), Some(get)) = (unsafe { ((*ext).count, (*ext).get) }) else {
        return (0, 0, false);
    };

    let inputs = unsafe { count(plugin, true) } as usize;
    let outputs = unsafe { count(plugin, false) } as usize;

    let mut clap_notes = false;
    for index in 0..inputs as u32 {
        let mut raw: clap_note_port_info = unsafe { std::mem::zeroed() };
        if unsafe { get(plugin, index, true, &mut raw) }
            && raw.supported_dialects & CLAP_NOTE_DIALECT_CLAP != 0
        {
            clap_notes = true;
            break;
        }
    }
    (inputs, outputs, clap_notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_host_api::AuxBuses;

    fn port(name: &str, channels: u16, is_main: bool) -> Port {
        Port {
            name: name.into(),
            channels,
            is_main,
        }
    }

    fn stereo_effect() -> PortLayout {
        PortLayout {
            inputs: vec![port("Main", 2, true), port("Sidechain", 2, false)],
            outputs: vec![port("Main", 2, true)],
        }
    }

    #[test]
    fn an_unwired_sidechain_still_gets_memory() {
        // CLAP has no inactive port: the plugin reads every entry of
        // `audio_inputs`, so the one nothing is wired to needs real silence.
        let plan = bind_ports(&stereo_effect(), &AudioConfig::default()).expect("bind");
        assert_eq!(plan.inputs[0].1, Binding::Caller(0));
        assert_eq!(plan.inputs[1].1, Binding::Scratch(0));
        assert_eq!(plan.silence_channels, 2);
        assert_eq!(plan.outputs[0].1, Binding::Caller(0));
    }

    #[test]
    fn a_wired_sidechain_reads_the_callers_region() {
        let config = AudioConfig {
            aux_inputs: AuxBuses::new(&[2]),
            ..AudioConfig::default()
        };
        let plan = bind_ports(&stereo_effect(), &config).expect("bind");
        // The caller's input region is main-then-aux, packed (§4.3).
        assert_eq!(plan.inputs[0].1, Binding::Caller(0));
        assert_eq!(plan.inputs[1].1, Binding::Caller(2));
        assert_eq!(plan.silence_channels, 0);
    }

    #[test]
    fn a_width_the_plugin_does_not_have_is_refused() {
        let mono = PortLayout {
            inputs: vec![port("Main", 1, true)],
            outputs: vec![port("Main", 1, true)],
        };
        // Adapting silently is how half a buffer ends up uninitialised; the
        // VST3 backend refuses the same case.
        let err = bind_ports(&mono, &AudioConfig::default()).unwrap_err();
        assert!(matches!(err, HostError::UnsupportedBusConfig(_)), "{err:?}");
    }

    #[test]
    fn an_instrument_with_no_input_port_is_fine() {
        let instrument = PortLayout {
            inputs: Vec::new(),
            outputs: vec![port("Main", 2, true)],
        };
        let config = AudioConfig {
            input_channels: 0,
            ..AudioConfig::default()
        };
        let plan = bind_ports(&instrument, &config).expect("bind");
        assert!(plan.inputs.is_empty());
        assert_eq!(plan.outputs[0].1, Binding::Caller(0));
    }

    #[test]
    fn extra_output_ports_are_written_somewhere_harmless() {
        let two_out = PortLayout {
            inputs: vec![port("Main", 2, true)],
            outputs: vec![port("Main", 2, true), port("Aux", 2, false)],
        };
        let plan = bind_ports(&two_out, &AudioConfig::default()).expect("bind");
        assert_eq!(plan.outputs[1].1, Binding::Scratch(0));
        assert_eq!(plan.scratch_channels, 2);
    }

    #[test]
    fn wiring_an_aux_the_plugin_lacks_is_refused() {
        let plain = PortLayout {
            inputs: vec![port("Main", 2, true)],
            outputs: vec![port("Main", 2, true)],
        };
        let config = AudioConfig {
            aux_inputs: AuxBuses::new(&[2]),
            ..AudioConfig::default()
        };
        assert!(bind_ports(&plain, &config).is_err());
    }
}
