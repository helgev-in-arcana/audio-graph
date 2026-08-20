//! Instantiating a VST3 class and driving it — M1 and M2.
//!
//! The lifecycle VST3 demands is long and order-sensitive:
//!
//! ```text
//! createInstance(IComponent) -> initialize(host)
//!   -> getControllerClassId -> createInstance(IEditController) -> initialize(host)
//!   -> setComponentHandler -> connect(IConnectionPoint pair)
//!   -> component.getState -> controller.setComponentState
//!   -> setBusArrangements -> activateBus -> setupProcessing
//!   -> setActive(true) -> setProcessing(true) -> process...
//! ```
//!
//! Getting any of it out of order produces failures that surface much later,
//! so the whole sequence lives in one place and is expressed through the
//! two-trait split of §4.2: [`Vst3Plugin`] is the main-thread half, and
//! `activate` yields the audio-thread half by value.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use plugin_host_api::{
    AudioBuffers, AudioConfig, Capabilities, Event, EventSink, HostContext, HostError, ParamFlags,
    ParamId, ParamInfo, ParamSnapshot, ParamValue as ApiParamValue, ProcessStatus, Result,
    SubPluginMain, SubPluginProcessor, TimeContext,
};
use vst3::Steinberg::Vst::{
    IAudioProcessor, IAudioProcessorTrait, IComponent, IComponentHandler, IComponentTrait,
    IConnectionPoint, IConnectionPointTrait, IEditController, IEditControllerTrait, IEventList,
    IParameterChanges, ParameterInfo, ProcessData, ProcessSetup, SpeakerArrangement, String128,
    TChar,
};
use vst3::Steinberg::{
    FUnknown, IPluginBaseTrait, IPluginFactoryTrait, TUID, kNotImplemented, kResultFalse,
    kResultOk, kResultTrue,
};
use vst3::{ComPtr, ComWrapper, Interface};

use crate::cid::Cid;
use crate::host_app::{ComponentHandler, HostApplication};

use crate::module::{Module, ModuleInner};
use crate::param_map::ParamMap;
use crate::process_io::{EventList, ParameterChanges};
use crate::stream::MemoryStream;
use crate::util::{from_char16, to_char16};
use crate::vst_events;

/// Per-block limits for the pre-allocated processing containers.
///
/// Fixed rather than derived because they must be decided before any audio
/// runs, and the sub-block quantiser (§9.2) bounds how many points a block can
/// ever carry: at 16-sample sub-blocks a 4096-sample block yields 256 updates
/// per parameter at most.
const MAX_PARAM_QUEUES: usize = 512;
const MAX_POINTS_PER_PARAM: usize = 512;
const MAX_EVENTS_PER_BLOCK: usize = 2048;

/// A loaded, initialised VST3 plugin instance. Main thread only.
pub struct Vst3Plugin {
    /// Keeps the module (and therefore the code these vtables point into)
    /// alive for as long as any instance exists.
    _module: Rc<ModuleInner>,
    /// Same reason: the plugin holds raw pointers to these host objects.
    _host_app: ComWrapper<HostApplication>,
    _handler: ComWrapper<ComponentHandler>,

    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    controller: Option<ComPtr<IEditController>>,
    /// Whether the controller is a distinct object from the component.
    ///
    /// A plugin may implement both interfaces on one object. When it does,
    /// `initialize` and `terminate` must each be called exactly once for that
    /// object — calling them a second time through the controller interface
    /// leaves the plugin half torn down, and the next instantiation from the
    /// same module faults.
    controller_is_separate: bool,
    /// The processor/controller connection, kept so it can be torn down in the
    /// right order.
    connection: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,

    params: Vec<ParamInfo>,
    /// Main-thread parameter edits waiting to be delivered to the processor.
    ///
    /// VST3 splits a plugin in two, and `IEditController::setParamNormalized`
    /// only reaches one half. The processor learns values solely through the
    /// change list in `process`, so an edit made from the main thread has to be
    /// queued for the next block — otherwise `IComponent::getState` saves a
    /// value the processor never had, and the preset is silently wrong.
    ///
    /// `Mutex` is safe on the audio side because the processor only ever
    /// *tries* to lock: an edit that loses the race is delivered one block
    /// later rather than blocking the callback.
    pending_edits: Arc<Mutex<Vec<(ParamId, f64)>>>,
    /// True once `activate` has handed out a processor.
    active: RefCell<bool>,
    latency: RefCell<u32>,
    context: Arc<dyn HostContext>,
}

impl Vst3Plugin {
    /// Create and fully initialise the class `cid` from `module`.
    pub fn create(module: &Module, cid: Cid, context: Arc<dyn HostContext>) -> Result<Vst3Plugin> {
        // Module-scoped, not instance-scoped: the factory retains the pointer
        // it is given via setHostContext for the module's whole lifetime. On
        // Linux this is also where a plugin picks up the run loop, and §5.4
        // wants that source to outlive any editor.
        let host_app = module.host_application(Arc::clone(&context));
        let host_unknown = com_ref_ptr::<_, FUnknown>(&host_app);

        let component = create_instance::<IComponent>(module, cid.to_tuid())?;
        check(
            unsafe { component.initialize(host_unknown) },
            "IComponent::initialize",
        )?;

        let processor = component
            .cast::<IAudioProcessor>()
            .ok_or_else(|| HostError::Backend {
                context: "IComponent does not implement IAudioProcessor".into(),
                code: 0,
            })?;

        let handler = ComponentHandler::new(Arc::clone(&context));
        let (controller, controller_is_separate) =
            Self::create_controller(module, &component, host_unknown, &handler)?;
        // Only meaningful between two distinct objects. A single object
        // implementing both interfaces would be connected to itself, which
        // plugins do not expect and OTT, for one, corrupts its heap over.
        let connection = if controller_is_separate {
            Self::connect(&component, controller.as_ref())
        } else {
            None
        };

        // The controller starts out knowing nothing about the processor's
        // state, so the initial state has to be handed across explicitly.
        if let Some(ctrl) = &controller {
            let stream = MemoryStream::empty();
            let stream_ptr = com_ref_ptr::<_, vst3::Steinberg::IBStream>(&stream);
            if unsafe { component.getState(stream_ptr) } == kResultOk {
                stream.rewind();
                unsafe { ctrl.setComponentState(stream_ptr) };
            }
        }

        let params = controller.as_ref().map(read_params).unwrap_or_default();

        Ok(Vst3Plugin {
            _module: module.handle(),
            _host_app: host_app,
            _handler: handler,
            component,
            processor,
            controller,
            controller_is_separate,
            connection,
            params,
            pending_edits: Arc::new(Mutex::new(Vec::with_capacity(MAX_PARAM_QUEUES))),
            active: RefCell::new(false),
            latency: RefCell::new(0),
            context,
        })
    }

    fn create_controller(
        module: &Module,
        component: &ComPtr<IComponent>,
        host_unknown: *mut FUnknown,
        handler: &ComWrapper<ComponentHandler>,
    ) -> Result<(Option<ComPtr<IEditController>>, bool)> {
        // Two legal shapes: a separate controller class, or one object
        // implementing both interfaces. A host that handles only the first
        // fails on a large fraction of real plugins.
        let mut controller_cid: TUID = [0; 16];
        let separate = unsafe { component.getControllerClassId(&mut controller_cid) } == kResultOk
            && controller_cid != [0; 16];

        let (controller, is_separate) = if separate {
            match create_instance::<IEditController>(module, controller_cid) {
                Ok(ctrl) => {
                    check(
                        unsafe { ctrl.initialize(host_unknown) },
                        "IEditController::initialize",
                    )?;
                    (Some(ctrl), true)
                }
                // A missing controller class is survivable: audio still works,
                // only parameters and the editor are lost. Refusing to load
                // would be worse for the user than a degraded load.
                Err(e) => {
                    log::warn!("controller class could not be created: {e}");
                    (None, false)
                }
            }
        } else {
            // Same object wearing both interfaces: already initialised as the
            // component, so it must not be initialised again.
            (component.cast::<IEditController>(), false)
        };

        if let Some(ctrl) = &controller {
            let handler_ptr = com_ref_ptr::<_, IComponentHandler>(handler);
            unsafe { ctrl.setComponentHandler(handler_ptr) };
        }

        Ok((controller, is_separate))
    }

    /// Wire the processor and controller together if both expose a connection
    /// point. Plugins use this channel for anything parameters cannot carry.
    ///
    /// Only called when the two are separate objects — see the call site.
    fn connect(
        component: &ComPtr<IComponent>,
        controller: Option<&ComPtr<IEditController>>,
    ) -> Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)> {
        let controller = controller?;
        let cp_component = component.cast::<IConnectionPoint>()?;
        let cp_controller = controller.cast::<IConnectionPoint>()?;

        // Connected directly rather than through a relay object. A relay only
        // earns its keep when the two halves live on different threads, which
        // is an IPC-era concern (ADR-6), not a v1 one.
        unsafe {
            cp_component.connect(cp_controller.as_ptr());
            cp_controller.connect(cp_component.as_ptr());
        }
        Some((cp_component, cp_controller))
    }

    /// Every bus the plugin declares, plus whether it takes notes (§14.2).
    ///
    /// Read before activation, so these are the plugin's *defaults* — what it
    /// says it is before anyone negotiates with it. That is the right thing for
    /// building sockets out of: the node has to offer a sidechain socket before
    /// the graph can ask for one to be connected.
    pub fn io_layout(&self) -> plugin_host_api::IoLayout {
        use vst3::Steinberg::Vst::{BusDirections_, BusTypes_, MediaTypes_};

        let buses = |media: i32, dir: i32| -> Vec<plugin_host_api::BusInfo> {
            let count = unsafe { self.component.getBusCount(media, dir) };
            (0..count.max(0))
                .filter_map(|index| {
                    let mut info: vst3::Steinberg::Vst::BusInfo = unsafe { std::mem::zeroed() };
                    if unsafe { self.component.getBusInfo(media, dir, index, &mut info) }
                        != kResultOk
                    {
                        return None;
                    }
                    Some(plugin_host_api::BusInfo {
                        name: crate::util::from_char16(&info.name),
                        channels: info.channelCount.max(0) as u16,
                        is_aux: info.busType == BusTypes_::kAux as i32,
                    })
                })
                .collect()
        };

        let audio = MediaTypes_::kAudio as i32;
        let event = MediaTypes_::kEvent as i32;
        let input = BusDirections_::kInput as i32;
        let output = BusDirections_::kOutput as i32;
        plugin_host_api::IoLayout {
            inputs: buses(audio, input),
            outputs: buses(audio, output),
            accepts_notes: !buses(event, input).is_empty(),
            emits_notes: !buses(event, output).is_empty(),
        }
    }

    /// The class's reported I/O, used to decide whether stereo is workable.
    pub fn bus_channel_counts(&self) -> (u32, u32) {
        use vst3::Steinberg::Vst::{BusDirections_, MediaTypes_};
        let count = |dir: i32| -> u32 {
            let n = unsafe { self.component.getBusCount(MediaTypes_::kAudio as i32, dir) };
            if n <= 0 {
                return 0;
            }
            let mut info: vst3::Steinberg::Vst::BusInfo = unsafe { std::mem::zeroed() };
            if unsafe {
                self.component
                    .getBusInfo(MediaTypes_::kAudio as i32, dir, 0, &mut info)
            } == kResultOk
            {
                info.channelCount.max(0) as u32
            } else {
                0
            }
        };
        (
            count(BusDirections_::kInput as i32),
            count(BusDirections_::kOutput as i32),
        )
    }

    pub fn params(&self) -> &[ParamInfo] {
        &self.params
    }

    /// Create the plugin's editor view, if it has one.
    ///
    /// Returns the raw `IPlugView`. That is deliberate: everything to do with
    /// windows lives in `vst3-host-view` (§2), and handing it the interface is
    /// the whole seam between the two crates. `plugin-host-api` never sees it,
    /// so the rule in §4.1 about backend types staying out of the API surface
    /// is untouched.
    ///
    /// The caller owns the returned view and must tear it down in the order
    /// §5.3 lays out; `vst3_host_view::EditorWindow` does exactly that.
    pub fn create_view(&self) -> Option<ComPtr<vst3::Steinberg::IPlugView>> {
        let controller = self.controller.as_ref()?;
        // "editor" is the only view name VST3 defines.
        let name = c"editor";
        let ptr = unsafe { controller.createView(name.as_ptr()) };
        // createView returns an owned reference.
        unsafe { ComPtr::from_raw(ptr) }
    }

    /// Whether the plugin offers an editor at all.
    pub fn has_editor(&self) -> bool {
        self.create_view().is_some()
    }

    fn controller(&self) -> Result<&ComPtr<IEditController>> {
        self.controller
            .as_ref()
            .ok_or(HostError::InvalidState("plugin has no edit controller"))
    }
}

impl SubPluginMain for Vst3Plugin {
    fn params(&self) -> &[ParamInfo] {
        &self.params
    }

    fn io_layout(&self) -> plugin_host_api::IoLayout {
        Vst3Plugin::io_layout(self)
    }

    fn capabilities(&self) -> Capabilities {
        // Fixed, not probed: VST3 has one value per parameter, so there is no
        // non-destructive modulation and no per-voice addressing to discover
        // (§3.4). Note expression is a different matter and is present.
        Capabilities {
            modulation: false,
            poly_modulation: false,
            note_expression: self.controller.as_ref().is_some_and(|c| {
                c.cast::<vst3::Steinberg::Vst::INoteExpressionController>()
                    .is_some()
            }),
            dynamic_params: false,
        }
    }

    fn snapshot(&self) -> ParamSnapshot {
        let Ok(ctrl) = self.controller() else {
            return ParamSnapshot::default();
        };
        ParamSnapshot {
            values: self
                .params
                .iter()
                .map(|p| {
                    let normalized = unsafe { ctrl.getParamNormalized(p.id.0) };
                    ApiParamValue {
                        id: p.id,
                        plain: unsafe { ctrl.normalizedParamToPlain(p.id.0, normalized) },
                    }
                })
                .collect(),
        }
    }

    fn param_to_text(&self, id: ParamId, plain: f64) -> Option<String> {
        let ctrl = self.controller().ok()?;
        let normalized = unsafe { ctrl.plainParamToNormalized(id.0, plain) };
        let mut buf: String128 = [0; 128];
        (unsafe { ctrl.getParamStringByValue(id.0, normalized, &mut buf) } == kResultOk)
            .then(|| from_char16(&buf))
    }

    fn param_from_text(&self, id: ParamId, text: &str) -> Option<f64> {
        let ctrl = self.controller().ok()?;
        let mut buf: [TChar; 128] = [0; 128];
        to_char16(text, &mut buf);
        let mut normalized = 0.0;
        (unsafe { ctrl.getParamValueByString(id.0, buf.as_mut_ptr(), &mut normalized) }
            == kResultOk)
            .then(|| unsafe { ctrl.normalizedParamToPlain(id.0, normalized) })
    }

    fn set_param(&mut self, id: ParamId, plain: f64) -> Result<()> {
        let ctrl = self.controller()?;
        let normalized = unsafe { ctrl.plainParamToNormalized(id.0, plain) };
        // The return value is advisory. Every iZotope plugin here answers
        // kResultFalse and applies the value anyway, and the SDK's own hosts
        // ignore it too. The caller can see what actually happened through
        // `snapshot`, which is a better source of truth than a status code.
        let res = unsafe { ctrl.setParamNormalized(id.0, normalized) };
        if res != kResultOk && res != kResultTrue && res != kResultFalse {
            return Err(HostError::Backend {
                context: "IEditController::setParamNormalized".into(),
                code: res,
            });
        }

        // The other half of the plugin still has to hear about it.
        if let Ok(mut pending) = self.pending_edits.lock() {
            pending.retain(|(existing, _)| *existing != id);
            if pending.len() < pending.capacity() {
                pending.push((id, plain));
            }
        }
        Ok(())
    }

    fn save_state(&self) -> Result<Vec<u8>> {
        // Two chunks: the processor's and the controller's. Both are needed —
        // a plugin may keep editor-only state (scroll position, current page)
        // in the second one — so they are length-prefixed into one blob.
        let component_state = {
            let stream = MemoryStream::empty();
            let ptr = com_ref_ptr::<_, vst3::Steinberg::IBStream>(&stream);
            check(
                unsafe { self.component.getState(ptr) },
                "IComponent::getState",
            )?;
            stream.contents()
        };

        let controller_state = match &self.controller {
            Some(ctrl) => {
                let stream = MemoryStream::empty();
                let ptr = com_ref_ptr::<_, vst3::Steinberg::IBStream>(&stream);
                if unsafe { ctrl.getState(ptr) } == kResultOk {
                    stream.contents()
                } else {
                    Vec::new()
                }
            }
            None => Vec::new(),
        };

        let mut out = Vec::with_capacity(component_state.len() + controller_state.len() + 8);
        out.extend_from_slice(&(component_state.len() as u32).to_le_bytes());
        out.extend_from_slice(&(controller_state.len() as u32).to_le_bytes());
        out.extend_from_slice(&component_state);
        out.extend_from_slice(&controller_state);
        Ok(out)
    }

    fn load_state(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 8 {
            return Err(HostError::State("state blob is truncated".into()));
        }
        let component_len = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
        let controller_len = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
        if data.len() < 8 + component_len + controller_len {
            return Err(HostError::State("state blob is truncated".into()));
        }
        let component_state = data[8..8 + component_len].to_vec();
        let controller_state = data[8 + component_len..8 + component_len + controller_len].to_vec();

        let stream = MemoryStream::from_bytes(component_state);
        let ptr = com_ref_ptr::<_, vst3::Steinberg::IBStream>(&stream);
        check(
            unsafe { self.component.setState(ptr) },
            "IComponent::setState",
        )?;

        if let Some(ctrl) = &self.controller {
            // The controller needs the *component* state as well as its own:
            // that is how it learns the parameter values the processor just
            // restored. Rewind first, since setState consumed the stream.
            stream.rewind();
            unsafe { ctrl.setComponentState(ptr) };

            if !controller_state.is_empty() {
                let ctrl_stream = MemoryStream::from_bytes(controller_state);
                let ctrl_ptr = com_ref_ptr::<_, vst3::Steinberg::IBStream>(&ctrl_stream);
                unsafe { ctrl.setState(ctrl_ptr) };
            }
        }
        Ok(())
    }

    fn latency_samples(&self) -> u32 {
        *self.latency.borrow()
    }

    fn activate(&mut self, config: AudioConfig) -> Result<Box<dyn SubPluginProcessor>> {
        if *self.active.borrow() {
            return Err(HostError::InvalidState("plugin is already active"));
        }

        let declared = setup_buses(&self.component, &self.processor, &config)?;

        let mut setup = ProcessSetup {
            processMode: if config.offline {
                vst3::Steinberg::Vst::ProcessModes_::kOffline as i32
            } else {
                vst3::Steinberg::Vst::ProcessModes_::kRealtime as i32
            },
            symbolicSampleSize: vst3::Steinberg::Vst::SymbolicSampleSizes_::kSample32 as i32,
            maxSamplesPerBlock: config.max_block_size as i32,
            sampleRate: config.sample_rate,
        };
        check(
            unsafe { self.processor.setupProcessing(&mut setup) },
            "IAudioProcessor::setupProcessing",
        )?;

        check(
            unsafe { self.component.setActive(1) },
            "IComponent::setActive(true)",
        )?;
        // Latency is only meaningful once the plugin is set up, which is why
        // it is read here rather than at construction.
        *self.latency.borrow_mut() = unsafe { self.processor.getLatencySamples() };
        // setProcessing is optional: a plugin with no realtime/offline
        // distinction returns kNotImplemented, and six of the iZotope plugins
        // here do. Treating that as a failure refuses to load them.
        let res = unsafe { self.processor.setProcessing(1) };
        if res != kResultOk && res != kResultTrue && res != kNotImplemented {
            unsafe { self.component.setActive(0) };
            *self.active.borrow_mut() = false;
            return Err(HostError::Backend {
                context: "IAudioProcessor::setProcessing(true)".into(),
                code: res,
            });
        }

        *self.active.borrow_mut() = true;
        self.context.latency_changed(*self.latency.borrow());

        // Built here, on the main thread, because IEditController may only be
        // called from it — see param_map's module comment.
        let map = match &self.controller {
            Some(ctrl) => ParamMap::build(&self.params, |id, normalized| unsafe {
                ctrl.normalizedParamToPlain(id.0, normalized)
            }),
            None => ParamMap::build(&[], |_, n| n),
        };

        Ok(Box::new(Vst3Processor::new(
            self.processor.clone(),
            config,
            &declared,
            map,
            Arc::clone(&self.pending_edits),
        )))
    }

    fn deactivate(&mut self, processor: Box<dyn SubPluginProcessor>) {
        // Dropping the processor first releases its clone of the interface
        // pointer, so setActive(false) is the last thing touching it.
        drop(processor);
        unsafe {
            self.processor.setProcessing(0);
            self.component.setActive(0);
        }
        *self.active.borrow_mut() = false;
    }
}

impl Drop for Vst3Plugin {
    fn drop(&mut self) {
        // Reverse of construction. Skipping the disconnect leaves each half
        // holding a pointer to the other, and plugins do dereference it during
        // their own teardown.
        if let Some((component_cp, controller_cp)) = self.connection.take() {
            unsafe {
                component_cp.disconnect(controller_cp.as_ptr());
                controller_cp.disconnect(component_cp.as_ptr());
            }
        }
        if *self.active.borrow() {
            unsafe {
                self.processor.setProcessing(0);
                self.component.setActive(0);
            }
        }
        if let Some(ctrl) = &self.controller {
            unsafe { ctrl.setComponentHandler(std::ptr::null_mut()) };
            if self.controller_is_separate {
                unsafe { ctrl.terminate() };
            }
        }
        unsafe { self.component.terminate() };
    }
}

/// Audio-thread half. Owns nothing the main thread also touches except the
/// interface pointer itself, whose thread-safety is the format's contract.
pub struct Vst3Processor {
    processor: ComPtr<IAudioProcessor>,
    config: AudioConfig,

    input_changes: ComWrapper<ParameterChanges>,
    output_changes: ComWrapper<ParameterChanges>,
    input_events: ComWrapper<EventList>,
    output_events: ComWrapper<EventList>,

    /// Channel pointer arrays rebuilt each block from the caller's flat
    /// buffers. Sized once, never grown.
    input_ptrs: Vec<*mut f32>,
    output_ptrs: Vec<*mut f32>,
    /// One descriptor per bus the plugin *declares*, active or not (§14.11).
    ///
    /// Not one per bus we use: `numInputs` counts declared buses, and a plugin
    /// that trusts it will read every entry. Ours declares a sidechain, and
    /// handing it one entry is how this was found. Built at activate with the
    /// widths fixed, so a block only refreshes the pointers.
    input_buses: Vec<vst3::Steinberg::Vst::AudioBusBuffers>,
    output_buses: Vec<vst3::Steinberg::Vst::AudioBusBuffers>,

    /// Plain→normalised conversion captured at activate.
    param_map: ParamMap,
    /// Shared with the main-thread half; see `Vst3Plugin::pending_edits`.
    pending_edits: Arc<Mutex<Vec<(ParamId, f64)>>>,
}

// SAFETY: VST3 designates IAudioProcessor as the audio-thread interface; the
// whole point of the two-trait split is that only this half crosses to that
// thread, and it is never shared with the main thread while it lives.
unsafe impl Send for Vst3Processor {}

impl Vst3Processor {
    fn new(
        processor: ComPtr<IAudioProcessor>,
        config: AudioConfig,
        declared: &DeclaredBuses,
        param_map: ParamMap,
        pending_edits: Arc<Mutex<Vec<(ParamId, f64)>>>,
    ) -> Vst3Processor {
        Vst3Processor {
            processor,
            config,
            input_changes: ParameterChanges::new(MAX_PARAM_QUEUES, MAX_POINTS_PER_PARAM),
            output_changes: ParameterChanges::new(MAX_PARAM_QUEUES, MAX_POINTS_PER_PARAM),
            input_events: EventList::new(MAX_EVENTS_PER_BLOCK),
            output_events: EventList::new(MAX_EVENTS_PER_BLOCK),
            input_ptrs: vec![std::ptr::null_mut(); config.total_input_channels() as usize],
            output_ptrs: vec![std::ptr::null_mut(); config.output_channels as usize],
            input_buses: declared.inputs.iter().map(empty_bus).collect(),
            output_buses: declared.outputs.iter().map(empty_bus).collect(),
            param_map,
            pending_edits,
        }
    }
}

impl SubPluginProcessor for Vst3Processor {
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
            // agreed to at activate, and clamping would silently drop audio.
            return ProcessStatus::Error;
        }

        self.input_changes.clear();
        self.output_changes.clear();
        self.input_events.clear();
        self.output_events.clear();
        out_events.clear();

        // Main-thread edits go in first, at offset 0, so an event stream for
        // this block still overrides them.
        if let Ok(mut pending) = self.pending_edits.try_lock() {
            for (id, plain) in pending.drain(..) {
                if let Some(normalized) = self.param_map.normalize(id, plain) {
                    self.input_changes.add_point(id.0, 0, normalized);
                }
            }
        }

        vst_events::fill_inputs(
            events,
            &self.param_map,
            &self.input_changes,
            &self.input_events,
        );

        // Channel pointers into the caller's flat planar storage.
        let frame_len = frames as usize;
        let input_raw = buffers.raw_input().as_ptr();
        for (channel, slot) in self.input_ptrs.iter_mut().enumerate() {
            // Cast away const: VST3 declares input buffers mutable but the
            // contract forbids writing them.
            *slot = unsafe { input_raw.add(channel * frame_len) as *mut f32 };
        }
        let output_raw = buffers.raw_output_mut().as_mut_ptr();
        for (channel, slot) in self.output_ptrs.iter_mut().enumerate() {
            *slot = unsafe { output_raw.add(channel * frame_len) };
        }

        // One `AudioBusBuffers` per *declared* bus, pointing into the one flat
        // run (§14.11). An inactive bus keeps zero channels and a null pointer,
        // which is what an unconnected bus looks like — but it still gets an
        // entry, because `numInputs` counts declared buses and a plugin that
        // trusts the count will read every one. Only the pointers are refreshed
        // here: the array and the widths were fixed at activate, because
        // `process` may not allocate.
        let mut at = 0usize;
        for bus in self.input_buses.iter_mut() {
            let width = bus.numChannels.max(0) as usize;
            bus.silenceFlags = 0;
            if width == 0 {
                continue;
            }
            bus.__field0.channelBuffers32 = unsafe { self.input_ptrs.as_mut_ptr().add(at) };
            at += width;
        }
        let mut at = 0usize;
        for bus in self.output_buses.iter_mut() {
            let width = bus.numChannels.max(0) as usize;
            bus.silenceFlags = 0;
            if width == 0 {
                continue;
            }
            bus.__field0.channelBuffers32 = unsafe { self.output_ptrs.as_mut_ptr().add(at) };
            at += width;
        }
        let mut process_context = vst_events::to_process_context(context, self.config.sample_rate);

        let mut data = ProcessData {
            processMode: if self.config.offline {
                vst3::Steinberg::Vst::ProcessModes_::kOffline as i32
            } else {
                vst3::Steinberg::Vst::ProcessModes_::kRealtime as i32
            },
            symbolicSampleSize: vst3::Steinberg::Vst::SymbolicSampleSizes_::kSample32 as i32,
            numSamples: frames as i32,
            numInputs: self.input_buses.len() as i32,
            numOutputs: self.output_buses.len() as i32,
            inputs: self.input_buses.as_mut_ptr(),
            outputs: self.output_buses.as_mut_ptr(),
            inputParameterChanges: com_ref_ptr::<_, IParameterChanges>(&self.input_changes),
            outputParameterChanges: com_ref_ptr::<_, IParameterChanges>(&self.output_changes),
            inputEvents: com_ref_ptr::<_, IEventList>(&self.input_events),
            outputEvents: com_ref_ptr::<_, IEventList>(&self.output_events),
            processContext: &mut process_context,
        };

        let result = unsafe { self.processor.process(&mut data) };
        if result != kResultOk && result != kResultTrue {
            buffers.clear_output();
            return ProcessStatus::Error;
        }

        vst_events::drain_outputs(&self.output_events, out_events);

        // The plugin sets silence flags on the output bus when it has nothing
        // to say; honouring that is what lets a chain skip downstream work.
        if let Some(main) = self.output_buses.first()
            && main.silenceFlags != 0
            && self.output_ptrs.len() as u32 <= 64
        {
            let all_silent = (0..self.output_ptrs.len()).all(|c| main.silenceFlags & (1 << c) != 0);
            if all_silent {
                return ProcessStatus::Silent;
            }
        }
        ProcessStatus::Continue
    }

    fn reset(&mut self) {
        // The format's way to drop tails is a processing off/on cycle; there is
        // no dedicated reset call.
        unsafe {
            self.processor.setProcessing(0);
            self.processor.setProcessing(1);
        }
    }
}

/// A descriptor for one declared bus, with no buffer attached yet.
fn empty_bus(bus: &DeclaredBus) -> vst3::Steinberg::Vst::AudioBusBuffers {
    vst3::Steinberg::Vst::AudioBusBuffers {
        numChannels: bus.channels as i32,
        silenceFlags: 0,
        __field0: vst3::Steinberg::Vst::AudioBusBuffers__type0 {
            channelBuffers32: std::ptr::null_mut(),
        },
    }
}

/// What one declared bus should be handed in `ProcessData` (§14.11).
///
/// Every bus a plugin *declares* needs an entry, active or not — that is what
/// `numInputs` counts, and a plugin reading past the array is what it costs to
/// be clever about it. Zero channels means the bus is declared but not switched
/// on, which is what an unconnected bus looks like to a plugin.
#[derive(Debug, Clone, Copy)]
struct DeclaredBus {
    channels: usize,
}

/// Every bus of a plugin, in declaration order, after negotiation.
struct DeclaredBuses {
    inputs: Vec<DeclaredBus>,
    outputs: Vec<DeclaredBus>,
}

/// Negotiate bus arrangements and activate the buses we intend to use.
fn setup_buses(
    component: &ComPtr<IComponent>,
    processor: &ComPtr<IAudioProcessor>,
    config: &AudioConfig,
) -> Result<DeclaredBuses> {
    use vst3::Steinberg::Vst::{BusDirections_, MediaTypes_, SpeakerArr};

    let arrangement = |channels: u32| -> SpeakerArrangement {
        match channels {
            0 => 0,
            1 => SpeakerArr::kMono,
            // v1 is stereo-only (§11); wider requests are asked for as stereo
            // and the result is verified below rather than assumed.
            _ => SpeakerArr::kStereo,
        }
    };

    // Every input bus is named in one call: VST3 negotiates the whole
    // arrangement at once, and asking about the main bus alone leaves a plugin
    // believing its sidechain is whatever it defaulted to (§14.11).
    let mut inputs: Vec<SpeakerArrangement> = Vec::with_capacity(1 + config.aux_inputs.len());
    if config.input_channels > 0 {
        inputs.push(arrangement(config.input_channels));
    }
    for width in config.aux_inputs.iter() {
        inputs.push(arrangement(u32::from(width)));
    }
    let mut outputs = [arrangement(config.output_channels)];
    let num_in = inputs.len() as i32;
    let num_out = if config.output_channels == 0 { 0 } else { 1 };

    let res = unsafe {
        processor.setBusArrangements(inputs.as_mut_ptr(), num_in, outputs.as_mut_ptr(), num_out)
    };
    // kResultFalse means "I chose something else", not failure. Verify rather
    // than trust: a plugin that quietly picked mono would otherwise corrupt
    // the second channel's memory.
    if res != kResultOk {
        let mut actual: SpeakerArrangement = 0;
        if num_out > 0
            && unsafe {
                processor.getBusArrangement(BusDirections_::kOutput as i32, 0, &mut actual)
            } == kResultOk
            && actual != outputs[0]
        {
            return Err(HostError::UnsupportedBusConfig(format!(
                "plugin refused a {}-channel output bus",
                config.output_channels
            )));
        }
        // An aux bus that came back different matters just as much: the buffer
        // handed to it is sized from what was asked for, and a plugin that
        // settled on mono would read past the end of its sidechain.
        for (index, wanted) in inputs.iter().enumerate() {
            let mut actual: SpeakerArrangement = 0;
            if unsafe {
                processor.getBusArrangement(
                    BusDirections_::kInput as i32,
                    index as i32,
                    &mut actual,
                )
            } == kResultOk
                && actual != *wanted
            {
                return Err(HostError::UnsupportedBusConfig(format!(
                    "plugin refused the arrangement asked for on input bus {index}"
                )));
            }
        }
    }

    // Buses default to inactive; a plugin with an inactive output bus writes
    // nothing at all. Only as many input buses as were negotiated are switched
    // on: an active sidechain that never receives audio is worse than an
    // inactive one, because a compressor will duck to silence against it.
    let mut declared = DeclaredBuses {
        inputs: Vec::new(),
        outputs: Vec::new(),
    };
    for (media, dir, active) in [
        (MediaTypes_::kAudio, BusDirections_::kInput, num_in as usize),
        (
            MediaTypes_::kAudio,
            BusDirections_::kOutput,
            num_out as usize,
        ),
        (MediaTypes_::kEvent, BusDirections_::kInput, 1),
        (MediaTypes_::kEvent, BusDirections_::kOutput, 0),
    ] {
        let count = unsafe { component.getBusCount(media as i32, dir as i32) };
        for index in 0..count {
            let on = (index as usize) < active;
            unsafe { component.activateBus(media as i32, dir as i32, index, u8::from(on)) };
            if media as i32 != MediaTypes_::kAudio as i32 {
                continue;
            }
            let width = if dir as i32 == BusDirections_::kInput as i32 {
                inputs.get(index as usize).copied()
            } else {
                outputs.get(index as usize).copied()
            };
            let bus = DeclaredBus {
                channels: if on {
                    width.map_or(0, channel_count)
                } else {
                    0
                },
            };
            if dir as i32 == BusDirections_::kInput as i32 {
                declared.inputs.push(bus);
            } else {
                declared.outputs.push(bus);
            }
        }
    }

    Ok(declared)
}

/// How many channels a speaker arrangement's bits stand for.
fn channel_count(arrangement: SpeakerArrangement) -> usize {
    arrangement.count_ones() as usize
}

/// Read the controller's parameter list into the core's plain-valued model.
fn read_params(controller: &ComPtr<IEditController>) -> Vec<ParamInfo> {
    use vst3::Steinberg::Vst::ParameterInfo_::ParameterFlags_ as F;

    let count = unsafe { controller.getParameterCount() };
    let mut out = Vec::with_capacity(count.max(0) as usize);

    for index in 0..count {
        let mut raw: ParameterInfo = unsafe { std::mem::zeroed() };
        if unsafe { controller.getParameterInfo(index, &mut raw) } != kResultOk {
            continue;
        }

        let stepped = raw.stepCount > 0;
        // VST3 speaks normalised 0..1; the core speaks plain (§3.1). The
        // controller's own converter is the only correct source for the range,
        // since the mapping can be non-linear.
        let (min, max) = if stepped {
            (0.0, raw.stepCount as f64)
        } else {
            (
                unsafe { controller.normalizedParamToPlain(raw.id, 0.0) },
                unsafe { controller.normalizedParamToPlain(raw.id, 1.0) },
            )
        };

        let mut flags = ParamFlags::NONE;
        flags.set(ParamFlags::STEPPED, stepped);
        flags.set(ParamFlags::PERIODIC, raw.flags & F::kIsWrapAround != 0);
        flags.set(ParamFlags::HIDDEN, raw.flags & F::kIsHidden != 0);
        flags.set(ParamFlags::READONLY, raw.flags & F::kIsReadOnly != 0);
        flags.set(ParamFlags::BYPASS, raw.flags & F::kIsBypass != 0);
        flags.set(ParamFlags::AUTOMATABLE, raw.flags & F::kCanAutomate != 0);
        // No MODULATABLE / POLY_MODULATABLE: VST3 has one value per parameter,
        // so neither exists in this backend (§3.4).

        out.push(ParamInfo {
            id: ParamId(raw.id),
            name: from_char16(&raw.title),
            module: from_char16(&raw.units),
            min,
            max,
            default: unsafe {
                controller.normalizedParamToPlain(raw.id, raw.defaultNormalizedValue)
            },
            flags,
        });
    }

    out
}

fn create_instance<I: Interface>(module: &Module, cid: TUID) -> Result<ComPtr<I>> {
    let mut obj: *mut std::ffi::c_void = std::ptr::null_mut();
    let iid = I::IID;
    let res = unsafe {
        module.factory().createInstance(
            cid.as_ptr() as *const std::ffi::c_char,
            iid.as_ptr() as *const std::ffi::c_char,
            &mut obj,
        )
    };
    if res != kResultOk || obj.is_null() {
        return Err(HostError::ClassNotFound(format!(
            "createInstance failed with {res:#010x}"
        )));
    }
    // createInstance returns an owned reference.
    unsafe { ComPtr::from_raw(obj as *mut I) }
        .ok_or_else(|| HostError::ClassNotFound("createInstance returned null".into()))
}

/// Borrowed interface pointer from a host-owned COM object.
fn com_ref_ptr<C: vst3::Class, I: Interface>(wrapper: &ComWrapper<C>) -> *mut I {
    wrapper
        .as_com_ref::<I>()
        .map_or(std::ptr::null_mut(), |r| r.as_ptr())
}

fn check(result: i32, context: &str) -> Result<()> {
    if result == kResultOk || result == kResultTrue {
        Ok(())
    } else {
        Err(HostError::Backend {
            context: context.to_string(),
            code: result,
        })
    }
}
