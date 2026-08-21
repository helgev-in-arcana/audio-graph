//! A CLAP plugin that exists so `clap-host` can be tested against a real one.
//!
//! Not a product and not in the dependency graph of anything shipped: it is
//! built as a `.clap` and loaded through the same path a third-party plugin
//! takes — `clap_entry`, factory, `create_plugin`, `init`, extensions,
//! `activate`, `process` — so the backend is exercised end to end rather than
//! against a mock of itself.
//!
//! Its behaviour is chosen to make assertions exact:
//!
//! * `out = in * gain + offset`, so a test can predict every sample.
//! * The sidechain input is *added* at a fixed 0.5, so a test can tell whether
//!   an unwired aux port really was silent (§14.11 is about exactly this).
//! * A note on emits a constant, not a tone, so note routing can be checked
//!   without a spectrum.
//! * Latency is a parameter, so the host's latency plumbing has something to
//!   report.
//! * State is the four parameter values, so a save/load round trip is
//!   verifiable rather than opaque.

#![allow(clippy::missing_safety_doc)]

use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::{AtomicU32, Ordering};

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON, CLAP_EVENT_PARAM_VALUE,
    clap_event_note, clap_event_param_value, clap_input_events, clap_output_events,
};
use clap_sys::ext::audio_ports::{
    CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS, CLAP_PORT_STEREO, clap_audio_port_info,
    clap_plugin_audio_ports,
};
use clap_sys::ext::gui::CLAP_EXT_GUI;
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_plugin_latency};
use clap_sys::ext::note_ports::{
    CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI, clap_note_port_info,
    clap_plugin_note_ports,
};
use clap_sys::ext::params::{
    CLAP_EXT_PARAMS, CLAP_PARAM_IS_AUTOMATABLE, CLAP_PARAM_IS_MODULATABLE,
    CLAP_PARAM_IS_MODULATABLE_PER_NOTE_ID, CLAP_PARAM_IS_STEPPED, clap_param_info,
    clap_plugin_params,
};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::id::clap_id;
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::process::{CLAP_PROCESS_CONTINUE, CLAP_PROCESS_ERROR, clap_process};
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::string_sizes::{CLAP_NAME_SIZE, CLAP_PATH_SIZE};
use clap_sys::version::CLAP_VERSION;

/// The identity a host records when it binds to this plugin.
pub const PLUGIN_ID: &CStr = c"dev.audio-graph.clap-test-plugin";

/// Parameter ids, which a test asserts against by number.
pub const PARAM_GAIN: clap_id = 0;
pub const PARAM_OFFSET: clap_id = 1;
pub const PARAM_MODE: clap_id = 2;
pub const PARAM_LATENCY: clap_id = 3;
pub const PARAM_COUNT: u32 = 4;

/// How much of the sidechain input is mixed into the output.
///
/// Non-zero and not 1.0, so a test can tell "the sidechain was silent" apart
/// from both "it was not connected at all" and "it was copied verbatim".
pub const SIDECHAIN_GAIN: f32 = 0.5;

/// What one held note adds to every output sample.
pub const NOTE_LEVEL: f32 = 0.25;

/// Counts live instances, so a test can prove `destroy` actually runs.
static LIVE_INSTANCES: AtomicU32 = AtomicU32::new(0);

/// How many instances of this plugin currently exist in the process.
pub fn live_instances() -> u32 {
    LIVE_INSTANCES.load(Ordering::Acquire)
}

struct Params {
    gain: f64,
    offset: f64,
    mode: f64,
    latency: f64,
}

impl Default for Params {
    fn default() -> Params {
        Params {
            gain: 1.0,
            offset: 0.0,
            mode: 0.0,
            latency: 0.0,
        }
    }
}

impl Params {
    fn get(&self, id: clap_id) -> Option<f64> {
        Some(match id {
            PARAM_GAIN => self.gain,
            PARAM_OFFSET => self.offset,
            PARAM_MODE => self.mode,
            PARAM_LATENCY => self.latency,
            _ => return None,
        })
    }

    fn set(&mut self, id: clap_id, value: f64) {
        match id {
            PARAM_GAIN => self.gain = value.clamp(0.0, 2.0),
            PARAM_OFFSET => self.offset = value.clamp(-1.0, 1.0),
            PARAM_MODE => self.mode = value.clamp(0.0, 2.0).round(),
            PARAM_LATENCY => self.latency = value.clamp(0.0, 512.0).round(),
            _ => {}
        }
    }
}

mod gui;

pub(crate) struct Instance {
    /// The struct handed to the host. First field so the pointer the host holds
    /// is also the pointer to this allocation, which `from_host` relies on.
    raw: clap_plugin,
    params: Params,
    /// Notes currently held, by key. Bounded by the array, which is all a
    /// fixture needs.
    held: [bool; 128],
    active: bool,
    processing: bool,
    initialised: bool,
    /// Declared last so it drops last, which is the wrong order on purpose:
    /// the host is required to call `gui.destroy` before the instance goes, and
    /// a fixture that cleaned up after a host that forgot would hide the bug
    /// §5.3 exists to catch.
    gui: gui::Gui,
}

impl Instance {
    /// # Safety
    /// `plugin` must be a pointer this plugin handed out and still owns.
    pub(crate) unsafe fn from_host<'a>(plugin: *const clap_plugin) -> Option<&'a mut Instance> {
        if plugin.is_null() {
            return None;
        }
        let data = unsafe { (*plugin).plugin_data };
        (!data.is_null()).then(|| unsafe { &mut *data.cast::<Instance>() })
    }
}

// --- descriptor -----------------------------------------------------------

/// Lets a table of raw pointers live in a `static`.
///
/// Every pointer here is to a string literal with `'static` lifetime, so
/// sharing it across threads is sound; Rust just has no way to see that.
#[repr(transparent)]
struct SyncArray<T>(T);

// SAFETY: the contents are immutable and point only at `'static` data.
unsafe impl<T> Sync for SyncArray<T> {}

static FEATURES: SyncArray<[*const c_char; 4]> = SyncArray([
    c"audio-effect".as_ptr(),
    c"stereo".as_ptr(),
    c"utility".as_ptr(),
    std::ptr::null(),
]);

static DESCRIPTOR: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: PLUGIN_ID.as_ptr(),
    name: c"audio-graph CLAP test plugin".as_ptr(),
    vendor: c"audio-graph".as_ptr(),
    url: c"https://example.invalid/".as_ptr(),
    manual_url: c"".as_ptr(),
    support_url: c"".as_ptr(),
    version: c"1.0.0".as_ptr(),
    description: c"Deterministic gain/offset used to test the CLAP host backend".as_ptr(),
    features: FEATURES.0.as_ptr(),
};

// --- entry point ----------------------------------------------------------

/// The symbol a CLAP host looks for.
///
/// `no_mangle` and the exact name are the whole ABI contract.
#[unsafe(no_mangle)]
pub static clap_entry: clap_plugin_entry = clap_plugin_entry {
    clap_version: CLAP_VERSION,
    init: Some(entry_init),
    deinit: Some(entry_deinit),
    get_factory: Some(entry_get_factory),
};

/// Counts `init`/`deinit` so a test can prove the host balances them exactly
/// once per module however many handles it opens (ADR-7).
static ENTRY_DEPTH: AtomicU32 = AtomicU32::new(0);

/// How many times `clap_entry.init` has been called without a matching
/// `deinit`.
pub fn entry_depth() -> u32 {
    ENTRY_DEPTH.load(Ordering::Acquire)
}

unsafe extern "C" fn entry_init(_path: *const c_char) -> bool {
    ENTRY_DEPTH.fetch_add(1, Ordering::AcqRel);
    true
}

unsafe extern "C" fn entry_deinit() {
    ENTRY_DEPTH.fetch_sub(1, Ordering::AcqRel);
}

unsafe extern "C" fn entry_get_factory(id: *const c_char) -> *const c_void {
    if id.is_null() {
        return std::ptr::null();
    }
    if unsafe { CStr::from_ptr(id) } == CLAP_PLUGIN_FACTORY_ID {
        return (&raw const FACTORY).cast();
    }
    std::ptr::null()
}

static FACTORY: clap_plugin_factory = clap_plugin_factory {
    get_plugin_count: Some(factory_count),
    get_plugin_descriptor: Some(factory_descriptor),
    create_plugin: Some(factory_create),
};

unsafe extern "C" fn factory_count(_factory: *const clap_plugin_factory) -> u32 {
    1
}

unsafe extern "C" fn factory_descriptor(
    _factory: *const clap_plugin_factory,
    index: u32,
) -> *const clap_plugin_descriptor {
    if index == 0 {
        &raw const DESCRIPTOR
    } else {
        std::ptr::null()
    }
}

unsafe extern "C" fn factory_create(
    _factory: *const clap_plugin_factory,
    _host: *const clap_host_ptr,
    id: *const c_char,
) -> *const clap_plugin {
    if id.is_null() || unsafe { CStr::from_ptr(id) } != PLUGIN_ID {
        return std::ptr::null();
    }

    let mut instance = Box::new(Instance {
        raw: clap_plugin {
            desc: &raw const DESCRIPTOR,
            plugin_data: std::ptr::null_mut(),
            init: Some(plugin_init),
            destroy: Some(plugin_destroy),
            activate: Some(plugin_activate),
            deactivate: Some(plugin_deactivate),
            start_processing: Some(plugin_start_processing),
            stop_processing: Some(plugin_stop_processing),
            reset: Some(plugin_reset),
            process: Some(plugin_process),
            get_extension: Some(plugin_get_extension),
            on_main_thread: Some(plugin_on_main_thread),
        },
        params: Params::default(),
        held: [false; 128],
        active: false,
        processing: false,
        initialised: false,
        gui: gui::Gui::default(),
    });
    // Only once the box has its final address.
    instance.raw.plugin_data = (&raw mut *instance).cast::<c_void>();
    LIVE_INSTANCES.fetch_add(1, Ordering::AcqRel);
    &raw const Box::leak(instance).raw
}

/// The host struct is opaque to this fixture; it never calls back.
use clap_sys::host::clap_host as clap_host_ptr;

// --- plugin ---------------------------------------------------------------

unsafe extern "C" fn plugin_init(plugin: *const clap_plugin) -> bool {
    match unsafe { Instance::from_host(plugin) } {
        Some(instance) => {
            instance.initialised = true;
            true
        }
        None => false,
    }
}

unsafe extern "C" fn plugin_destroy(plugin: *const clap_plugin) {
    let Some(instance) = (unsafe { Instance::from_host(plugin) }) else {
        return;
    };
    let owned = unsafe { Box::from_raw((&raw mut *instance).cast::<Instance>()) };
    drop(owned);
    LIVE_INSTANCES.fetch_sub(1, Ordering::AcqRel);
}

unsafe extern "C" fn plugin_activate(
    plugin: *const clap_plugin,
    sample_rate: f64,
    _min_frames: u32,
    _max_frames: u32,
) -> bool {
    let Some(instance) = (unsafe { Instance::from_host(plugin) }) else {
        return false;
    };
    // A plugin is entitled to refuse a configuration, and a test needs a way to
    // make it: an impossible rate is the trigger.
    if sample_rate <= 0.0 || !instance.initialised {
        return false;
    }
    instance.active = true;
    true
}

unsafe extern "C" fn plugin_deactivate(plugin: *const clap_plugin) {
    if let Some(instance) = unsafe { Instance::from_host(plugin) } {
        instance.active = false;
    }
}

unsafe extern "C" fn plugin_start_processing(plugin: *const clap_plugin) -> bool {
    match unsafe { Instance::from_host(plugin) } {
        Some(instance) if instance.active => {
            instance.processing = true;
            true
        }
        _ => false,
    }
}

unsafe extern "C" fn plugin_stop_processing(plugin: *const clap_plugin) {
    if let Some(instance) = unsafe { Instance::from_host(plugin) } {
        instance.processing = false;
    }
}

unsafe extern "C" fn plugin_reset(plugin: *const clap_plugin) {
    if let Some(instance) = unsafe { Instance::from_host(plugin) } {
        instance.held = [false; 128];
    }
}

unsafe extern "C" fn plugin_on_main_thread(_plugin: *const clap_plugin) {}

unsafe extern "C" fn plugin_process(
    plugin: *const clap_plugin,
    process: *const clap_process,
) -> i32 {
    let Some(instance) = (unsafe { Instance::from_host(plugin) }) else {
        return CLAP_PROCESS_ERROR;
    };
    if process.is_null() || !instance.processing {
        return CLAP_PROCESS_ERROR;
    }
    let data = unsafe { &*process };

    // Events first, at offset 0 only: a fixture that honoured sample offsets
    // would be testing its own scheduler rather than the host's translation.
    unsafe { apply_events(instance, data.in_events) };

    let frames = data.frames_count as usize;
    let note_sum = instance.held.iter().filter(|&&on| on).count() as f32 * NOTE_LEVEL;
    let gain = instance.params.gain as f32;
    let offset = instance.params.offset as f32;

    if data.audio_outputs_count == 0 || data.audio_outputs.is_null() {
        return CLAP_PROCESS_ERROR;
    }
    let out = unsafe { &*data.audio_outputs };
    let main_in = unsafe { bus(data.audio_inputs, data.audio_inputs_count, 0) };
    let side_in = unsafe { bus(data.audio_inputs, data.audio_inputs_count, 1) };

    for channel in 0..out.channel_count as usize {
        let dst = unsafe { *out.data32.add(channel) };
        if dst.is_null() {
            return CLAP_PROCESS_ERROR;
        }
        let src = unsafe { channel_ptr(main_in, channel) };
        let side = unsafe { channel_ptr(side_in, channel) };
        for frame in 0..frames {
            let dry = src.map_or(0.0, |p| unsafe { *p.add(frame) });
            let sc = side.map_or(0.0, |p| unsafe { *p.add(frame) });
            unsafe { *dst.add(frame) = dry * gain + offset + sc * SIDECHAIN_GAIN + note_sum };
        }
    }

    CLAP_PROCESS_CONTINUE
}

/// # Safety
/// `buses` must be null or an array of at least `count` entries.
unsafe fn bus<'a>(
    buses: *const clap_audio_buffer,
    count: u32,
    index: u32,
) -> Option<&'a clap_audio_buffer> {
    (!buses.is_null() && index < count).then(|| unsafe { &*buses.add(index as usize) })
}

/// # Safety
/// `bus` must be a live descriptor whose `data32` has `channel_count` entries.
unsafe fn channel_ptr(bus: Option<&clap_audio_buffer>, channel: usize) -> Option<*const f32> {
    let bus = bus?;
    if bus.data32.is_null() || channel >= bus.channel_count as usize {
        return None;
    }
    let p = unsafe { *bus.data32.add(channel) };
    (!p.is_null()).then_some(p as *const f32)
}

/// # Safety
/// `events` must be null or a live input event list.
unsafe fn apply_events(instance: &mut Instance, events: *const clap_input_events) {
    if events.is_null() {
        return;
    }
    let (Some(size), Some(get)) = (unsafe { ((*events).size, (*events).get) }) else {
        return;
    };
    for index in 0..unsafe { size(events) } {
        let header = unsafe { get(events, index) };
        if header.is_null() {
            continue;
        }
        let h = unsafe { *header };
        if h.space_id != CLAP_CORE_EVENT_SPACE_ID {
            continue;
        }
        match h.type_ {
            CLAP_EVENT_PARAM_VALUE => {
                let e = unsafe { *header.cast::<clap_event_param_value>() };
                instance.params.set(e.param_id, e.value);
            }
            CLAP_EVENT_NOTE_ON => {
                let e = unsafe { *header.cast::<clap_event_note>() };
                if let Some(slot) = usize::try_from(e.key)
                    .ok()
                    .and_then(|k| instance.held.get_mut(k))
                {
                    *slot = true;
                }
            }
            CLAP_EVENT_NOTE_OFF => {
                let e = unsafe { *header.cast::<clap_event_note>() };
                if let Some(slot) = usize::try_from(e.key)
                    .ok()
                    .and_then(|k| instance.held.get_mut(k))
                {
                    *slot = false;
                }
            }
            _ => {}
        }
    }
}

unsafe extern "C" fn plugin_get_extension(
    _plugin: *const clap_plugin,
    id: *const c_char,
) -> *const c_void {
    if id.is_null() {
        return std::ptr::null();
    }
    let id = unsafe { CStr::from_ptr(id) };
    if id == CLAP_EXT_PARAMS {
        (&raw const EXT_PARAMS).cast()
    } else if id == CLAP_EXT_AUDIO_PORTS {
        (&raw const EXT_AUDIO_PORTS).cast()
    } else if id == CLAP_EXT_NOTE_PORTS {
        (&raw const EXT_NOTE_PORTS).cast()
    } else if id == CLAP_EXT_STATE {
        (&raw const EXT_STATE).cast()
    } else if id == CLAP_EXT_LATENCY {
        (&raw const EXT_LATENCY).cast()
    } else if id == CLAP_EXT_GUI {
        (&raw const gui::EXT_GUI).cast()
    } else {
        std::ptr::null()
    }
}

// --- clap.params ----------------------------------------------------------

static EXT_PARAMS: clap_plugin_params = clap_plugin_params {
    count: Some(params_count),
    get_info: Some(params_get_info),
    get_value: Some(params_get_value),
    value_to_text: Some(params_value_to_text),
    text_to_value: Some(params_text_to_value),
    flush: Some(params_flush),
};

unsafe extern "C" fn params_count(_plugin: *const clap_plugin) -> u32 {
    PARAM_COUNT
}

unsafe extern "C" fn params_get_info(
    _plugin: *const clap_plugin,
    index: u32,
    info: *mut clap_param_info,
) -> bool {
    if info.is_null() || index >= PARAM_COUNT {
        return false;
    }
    let (name, module, min, max, default, flags) = match index {
        0 => (
            "Gain",
            "",
            0.0,
            2.0,
            1.0,
            CLAP_PARAM_IS_AUTOMATABLE | CLAP_PARAM_IS_MODULATABLE,
        ),
        1 => (
            "Offset",
            "Tone",
            -1.0,
            1.0,
            0.0,
            // Per-note modulation exists on exactly one parameter, so a test
            // can tell `Capabilities::poly_modulation` apart from `modulation`.
            CLAP_PARAM_IS_AUTOMATABLE
                | CLAP_PARAM_IS_MODULATABLE
                | CLAP_PARAM_IS_MODULATABLE_PER_NOTE_ID,
        ),
        2 => (
            "Mode",
            "Tone",
            0.0,
            2.0,
            0.0,
            CLAP_PARAM_IS_AUTOMATABLE | CLAP_PARAM_IS_STEPPED,
        ),
        _ => ("Latency", "", 0.0, 512.0, 0.0, CLAP_PARAM_IS_STEPPED),
    };

    let out = unsafe { &mut *info };
    *out = clap_param_info {
        id: index,
        flags,
        cookie: std::ptr::null_mut(),
        name: [0; CLAP_NAME_SIZE],
        module: [0; CLAP_PATH_SIZE],
        min_value: min,
        max_value: max,
        default_value: default,
    };
    write_chars(&mut out.name, name);
    write_chars(&mut out.module, module);
    true
}

fn write_chars(dst: &mut [c_char], text: &str) {
    let bytes = text.as_bytes();
    let n = bytes.len().min(dst.len().saturating_sub(1));
    for (slot, &b) in dst.iter_mut().zip(&bytes[..n]) {
        *slot = b as c_char;
    }
    dst[n] = 0;
}

unsafe extern "C" fn params_get_value(
    plugin: *const clap_plugin,
    id: clap_id,
    out: *mut f64,
) -> bool {
    let (Some(instance), false) = (unsafe { Instance::from_host(plugin) }, out.is_null()) else {
        return false;
    };
    match instance.params.get(id) {
        Some(value) => {
            unsafe { *out = value };
            true
        }
        None => false,
    }
}

unsafe extern "C" fn params_value_to_text(
    _plugin: *const clap_plugin,
    id: clap_id,
    value: f64,
    out: *mut c_char,
    capacity: u32,
) -> bool {
    if out.is_null() || capacity == 0 || id >= PARAM_COUNT {
        return false;
    }
    // Units on purpose: `param_to_text` exists so the *plugin* formats, and a
    // test that only saw a bare number could not tell whether it had.
    let text = match id {
        PARAM_GAIN => format!("{value:.2} x"),
        PARAM_OFFSET => format!("{value:+.3}"),
        PARAM_MODE => ["Off", "Half", "Full"][(value.round() as usize).min(2)].to_string(),
        _ => format!("{} smp", value.round() as i64),
    };
    let dst = unsafe { std::slice::from_raw_parts_mut(out, capacity as usize) };
    write_chars(dst, &text);
    true
}

unsafe extern "C" fn params_text_to_value(
    _plugin: *const clap_plugin,
    id: clap_id,
    text: *const c_char,
    out: *mut f64,
) -> bool {
    if text.is_null() || out.is_null() || id >= PARAM_COUNT {
        return false;
    }
    let text = unsafe { CStr::from_ptr(text) }.to_string_lossy();
    let head: String = text
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
        .collect();
    match head.parse::<f64>() {
        Ok(value) => {
            unsafe { *out = value };
            true
        }
        Err(_) => false,
    }
}

unsafe extern "C" fn params_flush(
    plugin: *const clap_plugin,
    in_: *const clap_input_events,
    _out: *const clap_output_events,
) {
    if let Some(instance) = unsafe { Instance::from_host(plugin) } {
        unsafe { apply_events(instance, in_) };
    }
}

// --- clap.audio-ports -----------------------------------------------------

static EXT_AUDIO_PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports {
    count: Some(audio_ports_count),
    get: Some(audio_ports_get),
};

unsafe extern "C" fn audio_ports_count(_plugin: *const clap_plugin, is_input: bool) -> u32 {
    // Two inputs so the aux/sidechain path of §14.11 has something to bind to.
    if is_input { 2 } else { 1 }
}

unsafe extern "C" fn audio_ports_get(
    _plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_audio_port_info,
) -> bool {
    if info.is_null() {
        return false;
    }
    let name = match (is_input, index) {
        (true, 0) | (false, 0) => "Main",
        (true, 1) => "Sidechain",
        _ => return false,
    };
    let out = unsafe { &mut *info };
    *out = clap_audio_port_info {
        id: index,
        name: [0; CLAP_NAME_SIZE],
        flags: if index == 0 {
            CLAP_AUDIO_PORT_IS_MAIN
        } else {
            0
        },
        channel_count: 2,
        port_type: CLAP_PORT_STEREO.as_ptr(),
        in_place_pair: clap_sys::id::CLAP_INVALID_ID,
    };
    write_chars(&mut out.name, name);
    true
}

// --- clap.note-ports ------------------------------------------------------

static EXT_NOTE_PORTS: clap_plugin_note_ports = clap_plugin_note_ports {
    count: Some(note_ports_count),
    get: Some(note_ports_get),
};

unsafe extern "C" fn note_ports_count(_plugin: *const clap_plugin, is_input: bool) -> u32 {
    u32::from(is_input)
}

unsafe extern "C" fn note_ports_get(
    _plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_note_port_info,
) -> bool {
    if info.is_null() || !is_input || index != 0 {
        return false;
    }
    let out = unsafe { &mut *info };
    *out = clap_note_port_info {
        id: 0,
        supported_dialects: CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI,
        preferred_dialect: CLAP_NOTE_DIALECT_CLAP,
        name: [0; CLAP_NAME_SIZE],
    };
    write_chars(&mut out.name, "Notes");
    true
}

// --- clap.state -----------------------------------------------------------

static EXT_STATE: clap_plugin_state = clap_plugin_state {
    save: Some(state_save),
    load: Some(state_load),
};

/// The blob is the four values as little-endian doubles, in id order.
const STATE_SIZE: usize = 4 * 8;

unsafe extern "C" fn state_save(plugin: *const clap_plugin, stream: *const clap_ostream) -> bool {
    let (Some(instance), false) = (unsafe { Instance::from_host(plugin) }, stream.is_null()) else {
        return false;
    };
    let Some(write) = (unsafe { (*stream).write }) else {
        return false;
    };
    let mut blob = [0u8; STATE_SIZE];
    for (index, value) in [
        instance.params.gain,
        instance.params.offset,
        instance.params.mode,
        instance.params.latency,
    ]
    .into_iter()
    .enumerate()
    {
        blob[index * 8..(index + 1) * 8].copy_from_slice(&value.to_le_bytes());
    }

    // Written in two calls on purpose: a host that assumed one write per save
    // would pass a single-call test and fail against a real plugin.
    let mut at = 0usize;
    while at < blob.len() {
        let chunk = (blob.len() - at).min(16);
        let n = unsafe { write(stream, blob[at..].as_ptr().cast(), chunk as u64) };
        if n <= 0 {
            return false;
        }
        at += n as usize;
    }
    true
}

unsafe extern "C" fn state_load(plugin: *const clap_plugin, stream: *const clap_istream) -> bool {
    let (Some(instance), false) = (unsafe { Instance::from_host(plugin) }, stream.is_null()) else {
        return false;
    };
    let Some(read) = (unsafe { (*stream).read }) else {
        return false;
    };
    let mut blob = [0u8; STATE_SIZE];
    let mut at = 0usize;
    while at < blob.len() {
        let n = unsafe {
            read(
                stream,
                blob[at..].as_mut_ptr().cast(),
                (blob.len() - at) as u64,
            )
        };
        if n < 0 {
            return false;
        }
        if n == 0 {
            // Short blob: refuse rather than load half a preset.
            return false;
        }
        at += n as usize;
    }

    let value =
        |index: usize| f64::from_le_bytes(blob[index * 8..(index + 1) * 8].try_into().unwrap());
    instance.params.gain = value(0);
    instance.params.offset = value(1);
    instance.params.mode = value(2);
    instance.params.latency = value(3);
    true
}

// --- clap.latency ---------------------------------------------------------

static EXT_LATENCY: clap_plugin_latency = clap_plugin_latency {
    get: Some(latency_get),
};

unsafe extern "C" fn latency_get(plugin: *const clap_plugin) -> u32 {
    match unsafe { Instance::from_host(plugin) } {
        Some(instance) => instance.params.latency as u32,
        None => 0,
    }
}
