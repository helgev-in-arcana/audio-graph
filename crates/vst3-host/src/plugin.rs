//! Instantiating and driving a VST3 plugin instance.
//!
//! The VST3 lifecycle is sequence-sensitive:
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
//! Getting any of it out of order produces failures that surface much later, so
//! the whole sequence lives in one place and is expressed through the two-trait
//! split: [`Vst3Plugin`] is the main-thread half, and `activate` yields the
//! audio-thread half by value, so a processor cannot exist before the sequence
//! has run.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use plugin_host_api::{
    AudioBuffers, AudioConfig, Capabilities, Event, EventSink, HostContext, HostError, MainThread,
    ParamFlags, ParamId, ParamInfo, ParamSnapshot, ParamValue as ApiParamValue, ProcessStatus,
    Processor, Result, SubPluginMain, SubPluginProcessor, TimeContext, reclaim_main_thread,
};
use vst3::Steinberg::Vst::{
    IAudioProcessor, IAudioProcessorTrait, IComponent, IComponentHandler, IComponentTrait,
    IConnectionPoint, IConnectionPointTrait, IEditController, IEditControllerTrait, IEventList,
    IParameterChanges, ParameterInfo, ProcessData, ProcessSetup, SpeakerArrangement, String128,
    TChar,
};
// Named only so `Vst3Plugin::interfaces` can ask for them by type; none of
// these are called.
use vst3::Steinberg::Vst::{
    IAudioPresentationLatency, IAutomationState, IEditController2, IEditControllerHostEditing,
    IKeyswitchController, IMidiMapping, INoteExpressionController,
    INoteExpressionPhysicalUIMapping, IParameterFunctionName, IPrefetchableSupport,
    IProcessContextRequirements, IProgramListData, IUnitData, IUnitInfo,
    IXmlRepresentationController,
};
// Nested one level deeper than the rest, hence its own import.
use vst3::Steinberg::Vst::ChannelContext::IInfoListener;
use vst3::Steinberg::{
    FUnknown, IPluginBaseTrait, IPluginFactoryTrait, TUID, kNotImplemented, kResultFalse,
    kResultOk, kResultTrue,
};
use vst3::{ComPtr, ComWrapper, Interface};

use crate::cid::Cid;
use crate::host_app::{ComponentHandler, HostApplication};

use crate::midi_map::MidiMap;
use crate::module::{Module, ModuleInner};
use crate::param_map::ParamMap;
use crate::process_io::{EventList, ParameterChanges};
use crate::stream::MemoryStream;
use crate::util::{from_char16, to_char16};
use crate::vst_events;

/// Per-block capacity limits for pre-allocated processing containers.
///
/// Fixed rather than derived because they must be decided before any audio runs,
/// and the sub-block quantiser bounds how many points a block can ever carry: at
/// 16-sample sub-blocks a 4096-sample block yields 256 updates per parameter at
/// most.
const MAX_PARAM_QUEUES: usize = 512;
const MAX_POINTS_PER_PARAM: usize = 512;
const MAX_EVENTS_PER_BLOCK: usize = 2048;

/// A loaded, initialised VST3 plugin instance. Main thread only.
pub struct Vst3Plugin {
    instance: Arc<MainThread<Vst3Instance>>,
    _main_thread: std::marker::PhantomData<Rc<()>>,

    params: Vec<ParamInfo>,
    io: plugin_host_api::IoLayout,
    metadata_dirty: bool,
    metadata_structural: bool,
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
    latency: RefCell<u32>,
    context: Arc<dyn HostContext>,
}

impl Vst3Plugin {
    /// Deliver recorded requests outside native callbacks, on the owning thread.
    pub fn tick(&mut self) {
        use plugin_host_api::RestartReason;
        use vst3::Steinberg::Vst::RestartFlags_::{
            kIoChanged, kIoTitlesChanged, kLatencyChanged, kParamTitlesChanged, kParamValuesChanged,
        };
        let flags = self.instance.get()._handler.take_restart_requests();
        self.metadata_dirty |=
            flags & (kIoChanged | kIoTitlesChanged | kLatencyChanged | kParamTitlesChanged) != 0;
        self.metadata_structural |= flags & (kIoChanged | kLatencyChanged) != 0;
        for (flag, reason) in [
            (kParamValuesChanged, RestartReason::ParamValues),
            (kParamTitlesChanged, RestartReason::ParamTitles),
            (kLatencyChanged, RestartReason::Latency),
            (kIoChanged | kIoTitlesChanged, RestartReason::IoConfig),
        ] {
            if flags & flag != 0 {
                self.context.request_restart(reason);
            }
        }
    }

    /// Create and fully initialise the class `cid` from `module`.
    pub fn create(module: &Module, cid: Cid, context: Arc<dyn HostContext>) -> Result<Vst3Plugin> {
        // Module-scoped, not instance-scoped: the factory retains the pointer
        // it is given via setHostContext for the module's whole lifetime. On
        // Linux this is also where a plugin picks up the run loop, which has to
        // outlive any editor.
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
        // plugins do not expect and at least one corrupts its heap over.
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

        let mut loaded = Vst3Plugin {
            _main_thread: std::marker::PhantomData,
            instance: Arc::new(MainThread::new(Vst3Instance {
                connection,
                controller,
                processor,
                component,
                _handler: handler,
                _host_app: host_app,
                _module: module.handle(),
                controller_is_separate,
                active: RefCell::new(false),
            })),
            params: Vec::new(),
            io: plugin_host_api::IoLayout::default(),
            metadata_dirty: false,
            metadata_structural: false,
            pending_edits: Arc::new(Mutex::new(Vec::with_capacity(MAX_PARAM_QUEUES))),
            latency: RefCell::new(0),
            context,
        };
        loaded.params = loaded
            .instance
            .get()
            .controller
            .as_ref()
            .map(read_params)
            .transpose()?
            .unwrap_or_default();
        loaded.io = loaded.read_io_layout()?;
        Ok(loaded)
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
        // earns its keep when the two halves live on different threads, which is
        // an out-of-process concern rather than a present one.
        unsafe {
            cp_component.connect(cp_controller.as_ptr());
            cp_controller.connect(cp_component.as_ptr());
        }
        Some((cp_component, cp_controller))
    }

    fn request_main_channels(&mut self, input: u16, output: u16) -> Result<()> {
        use vst3::Steinberg::Vst::{BusDirections_, MediaTypes_, SpeakerArr};

        // Arrangements may only be set while deactivated.
        if *self.instance.get().active.borrow() {
            return Err(HostError::InvalidState(
                "bus negotiation requires an inactive plugin",
            ));
        }
        if input > 2 || output > 2 {
            return Err(HostError::UnsupportedBusConfig(
                "only mono and stereo main arrangements are supported".into(),
            ));
        }
        let audio = MediaTypes_::kAudio as i32;
        let current = |dir: i32| -> Vec<SpeakerArrangement> {
            let count = unsafe { self.instance.get().component.getBusCount(audio, dir) };
            (0..count.max(0))
                .map(|index| {
                    let mut arrangement: SpeakerArrangement = 0;
                    unsafe {
                        self.instance.get().processor.getBusArrangement(
                            dir,
                            index,
                            &mut arrangement,
                        )
                    };
                    arrangement
                })
                .collect()
        };
        let mut inputs = current(BusDirections_::kInput as i32);
        let mut outputs = current(BusDirections_::kOutput as i32);
        for (buses, channels) in [(&mut inputs, input), (&mut outputs, output)] {
            if let Some(main) = buses.first_mut() {
                *main = match channels {
                    0 => 0,
                    1 => SpeakerArr::kMono,
                    _ => SpeakerArr::kStereo,
                };
            }
        }
        unsafe {
            self.instance.get().processor.setBusArrangements(
                inputs.as_mut_ptr(),
                inputs.len() as i32,
                outputs.as_mut_ptr(),
                outputs.len() as i32,
            )
        };
        self.metadata_dirty = true;
        self.metadata_structural = true;
        self.refresh_metadata()?;
        if self.io.main_input_channels() == input
            && self.io.outputs.first().map_or(0, |bus| bus.channels) == output
        {
            Ok(())
        } else {
            Err(HostError::UnsupportedBusConfig(
                "plugin selected different main widths".into(),
            ))
        }
    }

    /// Returns every bus declared by the plugin and note input/output capabilities.
    ///
    pub fn io_layout(&self) -> plugin_host_api::IoLayout {
        self.io.clone()
    }

    fn read_io_layout(&self) -> Result<plugin_host_api::IoLayout> {
        use vst3::Steinberg::Vst::{BusDirections_, BusTypes_, MediaTypes_};

        let buses = |media: i32, dir: i32| -> Result<Vec<plugin_host_api::BusInfo>> {
            let count = unsafe { self.instance.get().component.getBusCount(media, dir) };
            (0..count.max(0))
                .map(|index| {
                    let mut info: vst3::Steinberg::Vst::BusInfo = unsafe { std::mem::zeroed() };
                    if unsafe {
                        self.instance
                            .get()
                            .component
                            .getBusInfo(media, dir, index, &mut info)
                    } != kResultOk
                    {
                        return Err(HostError::InvalidState("bus enumeration failed"));
                    }
                    Ok(plugin_host_api::BusInfo {
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
        Ok(plugin_host_api::IoLayout {
            inputs: buses(audio, input)?,
            outputs: buses(audio, output)?,
            accepts_notes: !buses(event, input)?.is_empty(),
            emits_notes: !buses(event, output)?.is_empty(),
        })
    }

    /// The class's reported I/O, used to decide whether stereo is workable.
    pub fn bus_channel_counts(&self) -> (u32, u32) {
        use vst3::Steinberg::Vst::{BusDirections_, MediaTypes_};
        let count = |dir: i32| -> u32 {
            let n = unsafe {
                self.instance
                    .get()
                    .component
                    .getBusCount(MediaTypes_::kAudio as i32, dir)
            };
            if n <= 0 {
                return 0;
            }
            let mut info: vst3::Steinberg::Vst::BusInfo = unsafe { std::mem::zeroed() };
            if unsafe {
                self.instance.get().component.getBusInfo(
                    MediaTypes_::kAudio as i32,
                    dir,
                    0,
                    &mut info,
                )
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

    /// Which of the interfaces we know the names of this instance answers to.
    ///
    /// The VST3 counterpart of `ClapPlugin::extensions`, and purely diagnostic
    /// in the same way: nothing branches on it. `queryInterface` is what VST3
    /// has instead of `get_extension`, so the question "what does this plugin
    /// actually implement" is asked here by casting rather than by name.
    ///
    /// Both halves are asked, because a plugin splits its optional interfaces
    /// between them — `IMidiMapping` lives on the controller while
    /// `IProcessContextRequirements` lives on the component — and the caller
    /// has no reason to care which half answered.
    pub fn interfaces(&self) -> Vec<&'static str> {
        let mut found = Vec::new();

        macro_rules! probe {
            ($($i:ident),* $(,)?) => {$(
                if self.instance.get().component.cast::<$i>().is_some()
                    || self.instance.get().controller.as_ref().is_some_and(|c| c.cast::<$i>().is_some())
                {
                    found.push(stringify!($i));
                }
            )*};
        }

        probe!(
            IComponent,
            IAudioProcessor,
            IEditController,
            IEditController2,
            IEditControllerHostEditing,
            IConnectionPoint,
            IUnitInfo,
            IUnitData,
            IProgramListData,
            IMidiMapping,
            INoteExpressionController,
            INoteExpressionPhysicalUIMapping,
            IKeyswitchController,
            IProcessContextRequirements,
            IAudioPresentationLatency,
            IPrefetchableSupport,
            IAutomationState,
            IParameterFunctionName,
            IXmlRepresentationController,
            IInfoListener,
        );

        found
    }

    pub fn params(&self) -> &[ParamInfo] {
        &self.params
    }

    /// Creates a view retaining its native instance and module until the view is released.
    pub fn create_view(&self) -> Option<Vst3View> {
        let controller = self.instance.get().controller.as_ref()?;
        // "editor" is the only view name VST3 defines.
        let name = c"editor";
        let ptr = unsafe { controller.createView(name.as_ptr()) };
        // createView returns an owned reference.
        Some(Vst3View {
            view: unsafe { ComPtr::from_raw(ptr) }?,
            _instance: Arc::clone(&self.instance),
            _main_thread: std::marker::PhantomData,
        })
    }

    /// Whether the plugin offers an editor at all.
    pub fn has_editor(&self) -> bool {
        self.create_view().is_some()
    }

    fn controller(&self) -> Result<&ComPtr<IEditController>> {
        self.instance
            .get()
            .controller
            .as_ref()
            .ok_or(HostError::InvalidState("plugin has no edit controller"))
    }
}

impl SubPluginMain for Vst3Plugin {
    fn tick(&mut self) {
        Vst3Plugin::tick(self);
    }

    fn request_main_bus_channels(&mut self, input: u16, output: u16) -> Result<()> {
        self.request_main_channels(input, output)
    }

    fn refresh_metadata(&mut self) -> Result<plugin_host_api::MetadataUpdate> {
        use plugin_host_api::MetadataUpdate;
        reclaim_main_thread();
        self.tick();
        if !self.metadata_dirty {
            return Ok(MetadataUpdate::Unchanged);
        }
        if self.metadata_structural && *self.instance.get().active.borrow() {
            return Ok(MetadataUpdate::NeedsDeactivation);
        }
        let params = self
            .instance
            .get()
            .controller
            .as_ref()
            .map(read_params)
            .transpose()?
            .unwrap_or_default();
        let io = self.read_io_layout()?;
        let mapping_changed = params.len() != self.params.len()
            || params.iter().zip(&self.params).any(|(a, b)| {
                (a.id, a.min, a.max, a.default, a.flags) != (b.id, b.min, b.max, b.default, b.flags)
            });
        if mapping_changed && *self.instance.get().active.borrow() {
            self.metadata_structural = true;
            return Ok(MetadataUpdate::NeedsDeactivation);
        }
        self.metadata_dirty = false;
        self.metadata_structural = false;
        self.tick();
        if self.metadata_dirty {
            return Err(HostError::InvalidState(
                "metadata changed during refresh; retry",
            ));
        }
        self.params = params;
        self.io = io;
        Ok(MetadataUpdate::Refreshed)
    }

    fn params(&self) -> &[ParamInfo] {
        &self.params
    }

    fn io_layout(&self) -> plugin_host_api::IoLayout {
        Vst3Plugin::io_layout(self)
    }

    fn capabilities(&self) -> Capabilities {
        // VST3 parameters hold a single normalized value without non-destructive modulation.
        // Note expression support is queried from the controller interface.
        Capabilities {
            modulation: false,
            poly_modulation: false,
            note_expression: self.instance.get().controller.as_ref().is_some_and(|c| {
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
        let mut pending = self
            .pending_edits
            .lock()
            .map_err(|_| HostError::InvalidState("parameter queue poisoned"))?;
        if pending.len() == pending.capacity()
            && !pending.iter().any(|(existing, _)| *existing == id)
        {
            return Err(HostError::InvalidState("parameter queue full"));
        }
        pending.retain(|(existing, _)| *existing != id);
        pending.push((id, plain));
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
                unsafe { self.instance.get().component.getState(ptr) },
                "IComponent::getState",
            )?;
            stream.contents()
        };

        let controller_state = match &self.instance.get().controller {
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
        reclaim_main_thread();
        if *self.instance.get().active.borrow() {
            return Err(HostError::InvalidState(
                "state restoration requires an inactive plugin",
            ));
        }
        self.tick();
        if data.len() < 8 {
            return Err(HostError::State("state blob is truncated".into()));
        }
        let component_len = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
        let controller_len = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
        if data.len() < 8 + component_len + controller_len {
            return Err(HostError::State("state blob is truncated".into()));
        }
        let component_state = data[8..8 + component_len].to_vec();
        self.metadata_dirty = true;
        self.metadata_structural = true;
        let controller_state = data[8 + component_len..8 + component_len + controller_len].to_vec();

        let stream = MemoryStream::from_bytes(component_state);
        let ptr = com_ref_ptr::<_, vst3::Steinberg::IBStream>(&stream);
        check(
            unsafe { self.instance.get().component.setState(ptr) },
            "IComponent::setState",
        )?;

        if let Some(ctrl) = &self.instance.get().controller {
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
        self.refresh_metadata()?;
        Ok(())
    }

    fn latency_samples(&self) -> u32 {
        *self.latency.borrow()
    }

    fn activate(&mut self, config: AudioConfig) -> Result<Processor> {
        config.validate()?;
        reclaim_main_thread();
        self.tick();
        if self.metadata_dirty {
            return Err(HostError::InvalidState(
                "refresh metadata before activation",
            ));
        }
        if *self.instance.get().active.borrow() {
            return Err(HostError::InvalidState("plugin is already active"));
        }

        let declared = setup_buses(
            &self.instance.get().component,
            &self.instance.get().processor,
            &config,
        )?;
        self.io = self.read_io_layout()?;

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
            unsafe { self.instance.get().processor.setupProcessing(&mut setup) },
            "IAudioProcessor::setupProcessing",
        )?;

        check(
            unsafe { self.instance.get().component.setActive(1) },
            "IComponent::setActive(true)",
        )?;
        // Latency is only meaningful once the plugin is set up, which is why
        // it is read here rather than at construction.
        *self.latency.borrow_mut() = unsafe { self.instance.get().processor.getLatencySamples() };
        // setProcessing is optional: a plugin with no realtime/offline
        // distinction returns kNotImplemented, and six of the iZotope plugins
        // here do. Treating that as a failure refuses to load them.
        let res = unsafe { self.instance.get().processor.setProcessing(1) };
        if res != kResultOk && res != kResultTrue && res != kNotImplemented {
            unsafe { self.instance.get().component.setActive(0) };
            *self.instance.get().active.borrow_mut() = false;
            return Err(HostError::Backend {
                context: "IAudioProcessor::setProcessing(true)".into(),
                code: res,
            });
        }

        *self.instance.get().active.borrow_mut() = true;
        self.context.latency_changed(*self.latency.borrow());

        // Built here, on the main thread, because IEditController may only be
        // called from it — see param_map's module comment.
        let map = match &self.instance.get().controller {
            Some(ctrl) => ParamMap::build(&self.params, |id, normalized| unsafe {
                ctrl.normalizedParamToPlain(id.0, normalized)
            }),
            None => ParamMap::build(&[], |_, n| n),
        };

        // Same thread, same reason: IMidiMapping hangs off IEditController.
        let midi = MidiMap::build(
            self.instance
                .get()
                .controller
                .as_ref()
                .and_then(|c| c.cast::<IMidiMapping>())
                .as_ref(),
        );

        Ok(Processor::new(Vst3Processor::new(
            self.instance.get().processor.clone(),
            config,
            &declared,
            map,
            midi,
            Arc::clone(&self.pending_edits),
            Arc::clone(&self.instance),
        )))
    }
}

/// An editor view whose code and controller remain alive until its final release.
pub struct Vst3View {
    view: ComPtr<vst3::Steinberg::IPlugView>,
    _instance: Arc<MainThread<Vst3Instance>>,
    _main_thread: std::marker::PhantomData<Rc<()>>,
}

impl Vst3View {
    /// Borrows the native interface. Any derived interfaces must be released before this handle.
    pub fn as_ptr(&self) -> *mut vst3::Steinberg::IPlugView {
        self.view.as_ptr()
    }
}

struct Vst3Instance {
    connection: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    controller: Option<ComPtr<IEditController>>,
    processor: ComPtr<IAudioProcessor>,
    component: ComPtr<IComponent>,
    _handler: ComWrapper<ComponentHandler>,
    _host_app: ComWrapper<HostApplication>,
    // Every interface and host callback must be released before the library.
    _module: Rc<ModuleInner>,
    controller_is_separate: bool,
    active: RefCell<bool>,
}

impl Vst3Instance {
    fn deactivate(&self) {
        if *self.active.borrow() {
            unsafe {
                self.processor.setProcessing(0);
                self.component.setActive(0);
            }
            *self.active.borrow_mut() = false;
        }
    }
}

impl Drop for Vst3Instance {
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
        self.deactivate();
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
    /// Descriptors for each bus declared by the plugin (active or inactive).
    ///
    /// `numInputs` and `numOutputs` count all declared buses. Inactive buses have
    /// zero channels and null channel buffers.
    input_buses: Vec<vst3::Steinberg::Vst::AudioBusBuffers>,
    output_buses: Vec<vst3::Steinberg::Vst::AudioBusBuffers>,

    /// Plain→normalised conversion captured at activate.
    param_map: ParamMap,
    /// MIDI controller → parameter id, also captured at activate.
    midi_map: MidiMap,
    /// Shared with the main-thread half; see `Vst3Plugin::pending_edits`.
    pending_edits: Arc<Mutex<Vec<(ParamId, f64)>>>,
    instance: Arc<MainThread<Vst3Instance>>,
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
        midi_map: MidiMap,
        pending_edits: Arc<Mutex<Vec<(ParamId, f64)>>>,
        instance: Arc<MainThread<Vst3Instance>>,
    ) -> Vst3Processor {
        Vst3Processor {
            processor,
            config,
            input_changes: ParameterChanges::new(MAX_PARAM_QUEUES, MAX_POINTS_PER_PARAM),
            output_changes: ParameterChanges::new(MAX_PARAM_QUEUES, MAX_POINTS_PER_PARAM),
            input_events: EventList::new(MAX_EVENTS_PER_BLOCK),
            output_events: EventList::new(MAX_EVENTS_PER_BLOCK),
            input_ptrs: vec![std::ptr::null_mut(); config.total_input_channels() as usize],
            output_ptrs: vec![std::ptr::null_mut(); config.total_output_channels() as usize],
            input_buses: declared.inputs.iter().map(empty_bus).collect(),
            output_buses: declared.outputs.iter().map(empty_bus).collect(),
            param_map,
            midi_map,
            pending_edits,
            instance,
        }
    }
}

impl Drop for Vst3Processor {
    fn drop(&mut self) {
        self.instance.get().deactivate();
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
        if !buffers.matches_config(&self.config) {
            buffers.clear_output();
            return ProcessStatus::Error;
        }
        let frames = buffers.frame_count();
        if frames == 0 {
            return ProcessStatus::Continue;
        }

        self.input_changes.clear();
        self.output_changes.clear();
        self.input_events.clear();
        self.output_events.clear();

        // Main-thread edits go in first, at offset 0, so an event stream for
        // this block still overrides them.
        let mut pending = self.pending_edits.try_lock().ok();
        if let Some(pending) = pending.as_ref() {
            for &(id, plain) in pending.iter() {
                if let Some(normalized) = self.param_map.normalize(id, plain) {
                    self.input_changes.add_point(id.0, 0, normalized);
                }
            }
        }

        vst_events::fill_inputs(
            events,
            &self.param_map,
            &self.midi_map,
            &self.input_changes,
            &self.input_events,
        );
        if self.input_changes.overflowed()
            || self.input_events.overflowed()
            || !events.is_sorted_by_key(Event::sample_offset)
            || events.iter().any(|event| event.sample_offset() >= frames)
        {
            buffers.clear_output();
            return ProcessStatus::Error;
        }
        if let Some(pending) = pending.as_mut() {
            pending.clear();
        }
        drop(pending);
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

        // Refresh channel buffer pointers for each active declared bus.
        // Inactive buses retain zero channels and null buffers.
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
        if self.output_changes.overflowed() {
            out_events.mark_overflow();
        }

        // The plugin sets silence flags on the output bus when it has nothing
        // to say; honouring that is what lets a chain skip downstream work.
        // The main bus alone: `output_ptrs` may now span several buses, and a
        // silent main output says nothing about the aux ones.
        let main_width = self.config.output_channels as usize;
        if let Some(main) = self.output_buses.first()
            && main.silenceFlags != 0
            && main_width <= 64
        {
            let all_silent = (0..main_width).all(|c| main.silenceFlags & (1 << c) != 0);
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

/// Channel configuration for a declared bus in `ProcessData`.
///
/// A channel count of zero indicates an inactive/unconnected bus.
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

    let arrangements =
        |main: u32, aux: plugin_host_api::AuxBuses| -> Result<Vec<SpeakerArrangement>> {
            if main == 0 && !aux.is_empty() {
                return Err(HostError::UnsupportedBusConfig(
                    "aux buses require a connected main bus".into(),
                ));
            }
            std::iter::once(main)
                .filter(|&n| n != 0)
                .chain(aux.iter().map(u32::from))
                .map(|channels| match channels {
                    1 => Ok(SpeakerArr::kMono),
                    2 => Ok(SpeakerArr::kStereo),
                    _ => Err(HostError::UnsupportedBusConfig(
                        "only mono and stereo arrangements are supported".into(),
                    )),
                })
                .collect()
        };
    let mut inputs = arrangements(config.input_channels, config.aux_inputs)?;
    let mut outputs = arrangements(config.output_channels, config.aux_outputs)?;
    // Native negotiation is advisory; the resulting buses are the authority even on success.
    unsafe {
        processor.setBusArrangements(
            inputs.as_mut_ptr(),
            inputs.len() as i32,
            outputs.as_mut_ptr(),
            outputs.len() as i32,
        );
    }

    let mut declared = DeclaredBuses {
        inputs: Vec::new(),
        outputs: Vec::new(),
    };
    for (dir, wanted, buses) in [
        (BusDirections_::kInput as i32, &inputs, &mut declared.inputs),
        (
            BusDirections_::kOutput as i32,
            &outputs,
            &mut declared.outputs,
        ),
    ] {
        let count = unsafe { component.getBusCount(MediaTypes_::kAudio as i32, dir) };
        if count < 0 || (count as usize) < wanted.len() {
            return Err(HostError::UnsupportedBusConfig(
                "requested audio buses are missing".into(),
            ));
        }
        for index in 0..count {
            let arrangement = wanted.get(index as usize);
            let channels = if let Some(&wanted) = arrangement {
                let mut actual = 0;
                let mut info = unsafe { std::mem::zeroed() };
                if unsafe { processor.getBusArrangement(dir, index, &mut actual) } != kResultOk
                    || unsafe {
                        component.getBusInfo(MediaTypes_::kAudio as i32, dir, index, &mut info)
                    } != kResultOk
                    || actual != wanted
                    || info.channelCount != channel_count(wanted) as i32
                {
                    return Err(HostError::UnsupportedBusConfig(format!(
                        "audio bus {dir}:{index} differs from its requested arrangement"
                    )));
                }
                channel_count(actual)
            } else {
                0
            };
            buses.push(DeclaredBus { channels });
        }
    }
    // Validate both directions before changing which buses are active.
    for (media, dir, active) in [
        (MediaTypes_::kAudio, BusDirections_::kInput, inputs.len()),
        (MediaTypes_::kAudio, BusDirections_::kOutput, outputs.len()),
        (MediaTypes_::kEvent, BusDirections_::kInput, 1),
        (MediaTypes_::kEvent, BusDirections_::kOutput, 0),
    ] {
        let count = unsafe { component.getBusCount(media as i32, dir as i32) };
        for index in 0..count {
            check(
                unsafe {
                    component.activateBus(
                        media as i32,
                        dir as i32,
                        index,
                        u8::from((index as usize) < active),
                    )
                },
                "IComponent::activateBus",
            )?;
        }
    }
    Ok(declared)
}

/// How many channels a speaker arrangement's bits stand for.
fn channel_count(arrangement: SpeakerArrangement) -> usize {
    arrangement.count_ones() as usize
}

/// Read the controller's parameter list into the core's plain-valued model.
fn read_params(controller: &ComPtr<IEditController>) -> Result<Vec<ParamInfo>> {
    use vst3::Steinberg::Vst::ParameterInfo_::ParameterFlags_ as F;

    let count = unsafe { controller.getParameterCount() };
    let mut out = Vec::with_capacity(count.max(0) as usize);

    for index in 0..count {
        let mut raw: ParameterInfo = unsafe { std::mem::zeroed() };
        if unsafe { controller.getParameterInfo(index, &mut raw) } != kResultOk {
            return Err(HostError::InvalidState("parameter enumeration failed"));
        }

        let stepped = raw.stepCount > 0;
        // The edit controller provides the authoritative plain value range.
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
        // VST3 represents single parameter values without modulation metadata.

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

    Ok(out)
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
