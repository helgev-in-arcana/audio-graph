//! The `clap_host` a plugin is handed, and the extensions it can ask us for.
//!
//! CLAP inverts VST3's arrangement: instead of one `IHostApplication` that
//! answers `queryInterface`, the host publishes a `get_extension` function and
//! a set of small vtables. The tables are pure function pointers, so they are
//! `static`; only the per-instance state below is allocated.
//!
//! Scope discipline is the same as `vst3-host`'s (ARCHITECTURE.md §7): nothing
//! here decides *policy*. Every request from a plugin is either recorded for the
//! owner to act on at a safe moment, or forwarded to the injected
//! [`plugin_host_api::HostContext`].

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use clap_sys::ext::audio_ports::{CLAP_EXT_AUDIO_PORTS, clap_host_audio_ports};
use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_host_gui};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_host_latency};
use clap_sys::ext::log::{
    CLAP_EXT_LOG, CLAP_LOG_DEBUG, CLAP_LOG_ERROR, CLAP_LOG_FATAL, CLAP_LOG_HOST_MISBEHAVING,
    CLAP_LOG_INFO, CLAP_LOG_PLUGIN_MISBEHAVING, CLAP_LOG_WARNING, clap_host_log, clap_log_severity,
};
use clap_sys::ext::note_ports::{
    CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI, clap_host_note_ports,
    clap_note_dialect,
};
use clap_sys::ext::params::{
    CLAP_EXT_PARAMS, CLAP_PARAM_RESCAN_ALL, CLAP_PARAM_RESCAN_INFO, CLAP_PARAM_RESCAN_TEXT,
    CLAP_PARAM_RESCAN_VALUES, clap_host_params, clap_param_clear_flags, clap_param_rescan_flags,
};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_host_state};
use clap_sys::ext::tail::{CLAP_EXT_TAIL, clap_host_tail};
use clap_sys::ext::thread_check::{CLAP_EXT_THREAD_CHECK, clap_host_thread_check};
use clap_sys::ext::timer_support::{
    CLAP_EXT_TIMER_SUPPORT, clap_host_timer_support, clap_plugin_timer_support,
};
use clap_sys::ext::voice_info::{CLAP_EXT_VOICE_INFO, clap_host_voice_info};
use clap_sys::host::clap_host;
use clap_sys::id::{CLAP_INVALID_ID, clap_id};
use clap_sys::plugin::clap_plugin;
use clap_sys::version::CLAP_VERSION;
use plugin_host_api::{HostContext, RestartReason};

/// How many timers one plugin may register.
///
/// A ceiling rather than guidance: the list is walked on every UI tick, and a
/// plugin that leaks registrations would otherwise make the host slower the
/// longer it is open.
const MAX_TIMERS: usize = 16;

/// Shortest timer period the host will honour, in milliseconds.
///
/// Plugins ask for 16ms to match a display; anything faster is a busy loop
/// dressed as a timer, and the UI thread is shared with the wrapper's own
/// editor.
const MIN_TIMER_PERIOD_MS: u32 = 8;

thread_local! {
    /// Non-zero while this thread is inside `clap_plugin::process`.
    ///
    /// This is the whole of the `clap.thread-check` answer for the audio side.
    /// A counter rather than a flag because a sub-plugin's `process` can run
    /// inside our own (§14.9), and the inner call must not clear the outer
    /// call's mark on the way out.
    static AUDIO_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Marks the calling thread as the audio thread for as long as it lives.
pub(crate) struct AudioThreadGuard;

impl AudioThreadGuard {
    pub(crate) fn enter() -> AudioThreadGuard {
        AUDIO_DEPTH.with(|d| d.set(d.get() + 1));
        AudioThreadGuard
    }
}

impl Drop for AudioThreadGuard {
    fn drop(&mut self) {
        AUDIO_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

fn on_audio_thread() -> bool {
    AUDIO_DEPTH.with(|d| d.get()) > 0
}

/// One timer a plugin asked the host to run.
#[derive(Debug, Clone, Copy)]
struct Timer {
    id: clap_id,
    period: Duration,
    next: Instant,
}

/// What the plugin has asked for since the owner last looked.
///
/// Requests are recorded rather than acted on, because almost all of them
/// arrive at moments where acting is illegal — `request_restart` during
/// `process`, `params.rescan` from inside a parameter read. The owner drains
/// this at a point of its own choosing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PendingRequests {
    /// The plugin wants deactivate/activate.
    pub restart: bool,
    /// The plugin wants `on_main_thread` called.
    pub callback: bool,
    /// The plugin wants processing started again after a `Sleep`.
    pub process: bool,
    /// Its parameter list or values changed; the raw CLAP rescan flags.
    pub param_rescan: u32,
    /// Its reported latency changed.
    pub latency: bool,
    /// Its bus layout changed.
    pub audio_ports: bool,
    /// Its voice count or capacity changed.
    pub voice_info: bool,
    /// Its editor asked to be resized, in logical pixels.
    pub gui_resize: Option<(u32, u32)>,
    /// Its editor closed itself.
    pub gui_closed: bool,
}

/// The host object one plugin instance is given.
///
/// Must never be moved once a plugin has been handed a pointer to it, which is
/// why every owner keeps it in a `Box` and hands out only `&HostShim`.
pub(crate) struct HostShim {
    /// The struct the plugin sees. `host_data` points back at this `HostShim`,
    /// so every callback can find its way home.
    raw: clap_host,
    /// Kept alive because `raw` holds borrowed pointers into them.
    _strings: HostStrings,

    context: Arc<dyn HostContext>,

    /// Set once the instance exists, so a callback that needs to ask the plugin
    /// something (its new latency, its new size) can.
    plugin: AtomicPtr<clap_plugin>,

    /// The thread the instance was created on. CLAP's `[main-thread]`
    /// annotation means this thread and no other.
    main_thread: ThreadId,

    restart: AtomicBool,
    callback: AtomicBool,
    process: AtomicBool,
    param_rescan: AtomicU32,
    latency: AtomicBool,
    audio_ports: AtomicBool,
    voice_info: AtomicBool,
    /// Packed `(width << 32) | height`, or `NO_RESIZE` for "nothing pending".
    gui_resize: AtomicU64,
    gui_closed: AtomicBool,

    /// `Mutex` rather than `RefCell`: the format calls timer registration
    /// `[main-thread]`, but a `RefCell` reachable from a struct the audio
    /// thread also touches is a data race waiting for one misbehaving plugin.
    timers: Mutex<Vec<Timer>>,
}

/// Sentinel for "no resize pending" in the packed size.
///
/// `u64::MAX` cannot collide with a real request: it would mean a window
/// 4294967295 pixels wide.
const NO_RESIZE: u64 = u64::MAX;

/// The host's own identity strings, owned so `raw` can borrow them.
struct HostStrings {
    name: CString,
    vendor: CString,
    url: CString,
    version: CString,
}

impl HostShim {
    /// Build the host object for one instance.
    ///
    /// Boxed because the plugin is handed `&raw` and CLAP gives the host no way
    /// to say the pointer moved.
    pub(crate) fn new(context: Arc<dyn HostContext>) -> Box<HostShim> {
        // A plugin that finds an interior nul in our own name has been given a
        // broken host, so the fallbacks here are for our bug, not the user's.
        let strings = HostStrings {
            name: CString::new(context.host_name()).unwrap_or_else(|_| c"audio-graph".into()),
            vendor: c"audio-graph".into(),
            url: c"https://github.com/".into(),
            version: CString::new(env!("CARGO_PKG_VERSION")).unwrap_or_else(|_| c"0".into()),
        };

        let mut shim = Box::new(HostShim {
            raw: clap_host {
                clap_version: CLAP_VERSION,
                host_data: std::ptr::null_mut(),
                name: strings.name.as_ptr(),
                vendor: strings.vendor.as_ptr(),
                url: strings.url.as_ptr(),
                version: strings.version.as_ptr(),
                get_extension: Some(get_extension),
                request_restart: Some(request_restart),
                request_process: Some(request_process),
                request_callback: Some(request_callback),
            },
            _strings: strings,
            context,
            plugin: AtomicPtr::new(std::ptr::null_mut()),
            main_thread: std::thread::current().id(),
            restart: AtomicBool::new(false),
            callback: AtomicBool::new(false),
            process: AtomicBool::new(false),
            param_rescan: AtomicU32::new(0),
            latency: AtomicBool::new(false),
            audio_ports: AtomicBool::new(false),
            voice_info: AtomicBool::new(false),
            gui_resize: AtomicU64::new(NO_RESIZE),
            gui_closed: AtomicBool::new(false),
            timers: Mutex::new(Vec::new()),
        });
        // Only now, once the box has its final address.
        let back = (&raw mut *shim).cast::<c_void>();
        shim.raw.host_data = back;
        shim
    }

    /// The pointer to hand to `create_plugin` and to every extension call.
    pub(crate) fn raw(&self) -> *const clap_host {
        &raw const self.raw
    }

    /// Record the instance, so callbacks that need to ask it something can.
    pub(crate) fn set_plugin(&self, plugin: *const clap_plugin) {
        self.plugin.store(plugin.cast_mut(), Ordering::Release);
    }

    /// Take everything the plugin has asked for since the last call.
    ///
    /// Draining rather than peeking: a request acted on twice is a restart the
    /// user did not ask for, or a resize that fights the one they are making.
    pub(crate) fn take_requests(&self) -> PendingRequests {
        let packed = self.gui_resize.swap(NO_RESIZE, Ordering::AcqRel);
        PendingRequests {
            restart: self.restart.swap(false, Ordering::AcqRel),
            callback: self.callback.swap(false, Ordering::AcqRel),
            process: self.process.swap(false, Ordering::AcqRel),
            param_rescan: self.param_rescan.swap(0, Ordering::AcqRel),
            latency: self.latency.swap(false, Ordering::AcqRel),
            audio_ports: self.audio_ports.swap(false, Ordering::AcqRel),
            voice_info: self.voice_info.swap(false, Ordering::AcqRel),
            gui_resize: (packed != NO_RESIZE)
                .then_some(((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32)),
            gui_closed: self.gui_closed.swap(false, Ordering::AcqRel),
        }
    }

    /// Fire any timer whose period has elapsed.
    ///
    /// Called from the owner's UI tick, on the main thread. Plugins use these
    /// to repaint and to poll their own worker threads, so an editor that never
    /// gets them looks frozen.
    pub(crate) fn tick_timers(&self) {
        let plugin = self.plugin.load(Ordering::Acquire);
        if plugin.is_null() {
            return;
        }
        let ext = unsafe { (*plugin).get_extension }
            .map(|get| unsafe { get(plugin, CLAP_EXT_TIMER_SUPPORT.as_ptr()) })
            .unwrap_or(std::ptr::null());
        if ext.is_null() {
            return;
        }
        let Some(on_timer) = (unsafe { (*ext.cast::<clap_plugin_timer_support>()).on_timer })
        else {
            return;
        };

        // The due list is collected under the lock and fired outside it: a
        // plugin is entitled to unregister a timer from inside its own
        // callback, and that call comes straight back through this mutex.
        let now = Instant::now();
        let due: Vec<clap_id> = {
            let Ok(mut timers) = self.timers.lock() else {
                return;
            };
            timers
                .iter_mut()
                .filter(|t| t.next <= now)
                .map(|t| {
                    t.next = now + t.period;
                    t.id
                })
                .collect()
        };
        for id in due {
            unsafe { on_timer(plugin, id) };
        }
    }
}

// SAFETY: every field is either immutable after construction or an atomic /
// mutex. CLAP requires the host object to be reachable from both threads —
// `request_restart` and friends are `[thread-safe]` — so this is the format's
// requirement rather than a convenience.
unsafe impl Send for HostShim {}
unsafe impl Sync for HostShim {}

/// Recover the shim from the pointer a plugin was handed.
///
/// # Safety
/// `host` must be a pointer this crate created and still owns.
unsafe fn shim<'a>(host: *const clap_host) -> Option<&'a HostShim> {
    if host.is_null() {
        return None;
    }
    let data = unsafe { (*host).host_data };
    (!data.is_null()).then(|| unsafe { &*data.cast::<HostShim>() })
}

// --- clap_host ------------------------------------------------------------

unsafe extern "C" fn get_extension(host: *const clap_host, id: *const c_char) -> *const c_void {
    if id.is_null() {
        return std::ptr::null();
    }
    let id = unsafe { CStr::from_ptr(id) };

    // Compared by name against the constants rather than by a table, because
    // the set is small and a table would still be a linear scan.
    let ptr: *const c_void = if id == CLAP_EXT_LOG {
        (&raw const HOST_LOG).cast()
    } else if id == CLAP_EXT_THREAD_CHECK {
        (&raw const HOST_THREAD_CHECK).cast()
    } else if id == CLAP_EXT_PARAMS {
        (&raw const HOST_PARAMS).cast()
    } else if id == CLAP_EXT_LATENCY {
        (&raw const HOST_LATENCY).cast()
    } else if id == CLAP_EXT_STATE {
        (&raw const HOST_STATE).cast()
    } else if id == CLAP_EXT_GUI {
        (&raw const HOST_GUI).cast()
    } else if id == CLAP_EXT_TIMER_SUPPORT {
        (&raw const HOST_TIMER_SUPPORT).cast()
    } else if id == CLAP_EXT_AUDIO_PORTS {
        (&raw const HOST_AUDIO_PORTS).cast()
    } else if id == CLAP_EXT_NOTE_PORTS {
        (&raw const HOST_NOTE_PORTS).cast()
    } else if id == CLAP_EXT_TAIL {
        (&raw const HOST_TAIL).cast()
    } else if id == CLAP_EXT_VOICE_INFO {
        (&raw const HOST_VOICE_INFO).cast()
    } else {
        std::ptr::null()
    };

    // Answering an extension we do not implement is worse than refusing it: a
    // plugin will call straight through the null members it finds.
    let _ = unsafe { shim(host) };
    ptr
}

unsafe extern "C" fn request_restart(host: *const clap_host) {
    if let Some(shim) = unsafe { shim(host) } {
        shim.restart.store(true, Ordering::Release);
        shim.context.request_restart(RestartReason::IoConfig);
    }
}

unsafe extern "C" fn request_process(host: *const clap_host) {
    if let Some(shim) = unsafe { shim(host) } {
        shim.process.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn request_callback(host: *const clap_host) {
    if let Some(shim) = unsafe { shim(host) } {
        shim.callback.store(true, Ordering::Release);
    }
}

// --- clap.log -------------------------------------------------------------

static HOST_LOG: clap_host_log = clap_host_log { log: Some(log_) };

unsafe extern "C" fn log_(
    _host: *const clap_host,
    severity: clap_log_severity,
    msg: *const c_char,
) {
    let text = unsafe { crate::util::from_cstr(msg) };
    match severity {
        CLAP_LOG_DEBUG => log::debug!("clap: {text}"),
        CLAP_LOG_INFO => log::info!("clap: {text}"),
        CLAP_LOG_WARNING => log::warn!("clap: {text}"),
        CLAP_LOG_ERROR | CLAP_LOG_FATAL => log::error!("clap: {text}"),
        // The two "misbehaving" levels are a plugin telling us who it blames.
        // Kept at warn either way: the user cannot act on it, and a plugin that
        // thinks the host is wrong is usually worth reading while debugging.
        CLAP_LOG_HOST_MISBEHAVING => log::warn!("clap says the host misbehaved: {text}"),
        CLAP_LOG_PLUGIN_MISBEHAVING => log::warn!("clap plugin misbehaved: {text}"),
        _ => log::info!("clap: {text}"),
    }
}

// --- clap.thread-check ----------------------------------------------------

static HOST_THREAD_CHECK: clap_host_thread_check = clap_host_thread_check {
    is_main_thread: Some(is_main_thread),
    is_audio_thread: Some(is_audio_thread),
};

unsafe extern "C" fn is_main_thread(host: *const clap_host) -> bool {
    match unsafe { shim(host) } {
        // Being inside `process` disqualifies the thread even if it is also the
        // one the instance was created on, which is exactly the case an offline
        // renderer creates.
        Some(shim) => !on_audio_thread() && std::thread::current().id() == shim.main_thread,
        None => false,
    }
}

unsafe extern "C" fn is_audio_thread(_host: *const clap_host) -> bool {
    on_audio_thread()
}

// --- clap.params ----------------------------------------------------------

static HOST_PARAMS: clap_host_params = clap_host_params {
    rescan: Some(params_rescan),
    clear: Some(params_clear),
    request_flush: Some(params_request_flush),
};

unsafe extern "C" fn params_rescan(host: *const clap_host, flags: clap_param_rescan_flags) {
    let Some(shim) = (unsafe { shim(host) }) else {
        return;
    };
    shim.param_rescan.fetch_or(flags, Ordering::AcqRel);

    // Translated for the wrapper, which does not speak CLAP: the distinction
    // that matters to it is whether the *set* of parameters changed (sockets
    // have to be rebuilt) or only their values or labels.
    let reason = if flags & (CLAP_PARAM_RESCAN_ALL | CLAP_PARAM_RESCAN_INFO) != 0 {
        RestartReason::ParamList
    } else if flags & CLAP_PARAM_RESCAN_TEXT != 0 {
        RestartReason::ParamTitles
    } else if flags & CLAP_PARAM_RESCAN_VALUES != 0 {
        RestartReason::ParamValues
    } else {
        return;
    };
    shim.context.request_restart(reason);
}

unsafe extern "C" fn params_clear(
    _host: *const clap_host,
    _param_id: clap_id,
    _flags: clap_param_clear_flags,
) {
    // Nothing to clear: v1 is Drive mode (ADR-5), so the wrapper is the sole
    // authority for every value and holds no automation or modulation of its
    // own that a plugin could ask it to drop.
}

unsafe extern "C" fn params_request_flush(host: *const clap_host) {
    // Recorded as a callback request rather than flushing here: flush is
    // `[main-thread]` when the plugin is inactive, and this call may arrive
    // from anywhere.
    if let Some(shim) = unsafe { shim(host) } {
        shim.callback.store(true, Ordering::Release);
    }
}

// --- clap.latency ---------------------------------------------------------

static HOST_LATENCY: clap_host_latency = clap_host_latency {
    changed: Some(latency_changed),
};

unsafe extern "C" fn latency_changed(host: *const clap_host) {
    if let Some(shim) = unsafe { shim(host) } {
        shim.latency.store(true, Ordering::Release);
        shim.context.request_restart(RestartReason::Latency);
    }
}

// --- clap.state -----------------------------------------------------------

static HOST_STATE: clap_host_state = clap_host_state {
    mark_dirty: Some(mark_dirty),
};

unsafe extern "C" fn mark_dirty(_host: *const clap_host) {
    // The wrapper saves the sub-plugin's blob whenever the DAW asks it for
    // state (§8.3); there is no separate dirty flag to set. Forwarding it as an
    // edit would be wrong — `param_edited` names a parameter, and this call
    // does not.
}

// --- clap.gui -------------------------------------------------------------

static HOST_GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(gui_resize_hints_changed),
    request_resize: Some(gui_request_resize),
    request_show: Some(gui_request_show),
    request_hide: Some(gui_request_hide),
    closed: Some(gui_closed),
};

unsafe extern "C" fn gui_resize_hints_changed(_host: *const clap_host) {
    // Only affects whether the user may drag the window edge, which the
    // container re-reads on the next tick anyway.
}

unsafe extern "C" fn gui_request_resize(host: *const clap_host, width: u32, height: u32) -> bool {
    let Some(shim) = (unsafe { shim(host) }) else {
        return false;
    };
    shim.gui_resize.store(
        (u64::from(width) << 32) | u64::from(height),
        Ordering::Release,
    );
    // True means "the host will honour it", which it will on the next tick.
    // Answering false here makes plugins that scale their own layout give up
    // and draw at the old size.
    true
}

unsafe extern "C" fn gui_request_show(_host: *const clap_host) -> bool {
    // The window is already open by the time a plugin can ask; a plugin that
    // wants to open its editor unprompted is asking the user's question for
    // them.
    false
}

unsafe extern "C" fn gui_request_hide(_host: *const clap_host) -> bool {
    false
}

unsafe extern "C" fn gui_closed(host: *const clap_host, _was_destroyed: bool) {
    if let Some(shim) = unsafe { shim(host) } {
        shim.gui_closed.store(true, Ordering::Release);
    }
}

// --- clap.timer-support ---------------------------------------------------

static HOST_TIMER_SUPPORT: clap_host_timer_support = clap_host_timer_support {
    register_timer: Some(register_timer),
    unregister_timer: Some(unregister_timer),
};

unsafe extern "C" fn register_timer(
    host: *const clap_host,
    period_ms: u32,
    timer_id: *mut clap_id,
) -> bool {
    if timer_id.is_null() {
        return false;
    }
    unsafe { *timer_id = CLAP_INVALID_ID };
    let Some(shim) = (unsafe { shim(host) }) else {
        return false;
    };
    let Ok(mut timers) = shim.timers.lock() else {
        return false;
    };
    if timers.len() >= MAX_TIMERS {
        log::warn!("clap plugin asked for more than {MAX_TIMERS} timers");
        return false;
    }
    // Ids are handed out densely and never reused within an instance, so a
    // stale `unregister_timer` cannot cancel somebody else's timer.
    let id = timers.iter().map(|t| t.id + 1).max().unwrap_or(0);
    let period = Duration::from_millis(u64::from(period_ms.max(MIN_TIMER_PERIOD_MS)));
    timers.push(Timer {
        id,
        period,
        next: Instant::now() + period,
    });
    unsafe { *timer_id = id };
    true
}

unsafe extern "C" fn unregister_timer(host: *const clap_host, timer_id: clap_id) -> bool {
    let Some(shim) = (unsafe { shim(host) }) else {
        return false;
    };
    let Ok(mut timers) = shim.timers.lock() else {
        return false;
    };
    let before = timers.len();
    timers.retain(|t| t.id != timer_id);
    before != timers.len()
}

// --- clap.audio-ports -----------------------------------------------------

static HOST_AUDIO_PORTS: clap_host_audio_ports = clap_host_audio_ports {
    is_rescan_flag_supported: Some(audio_ports_flag_supported),
    rescan: Some(audio_ports_rescan),
};

unsafe extern "C" fn audio_ports_flag_supported(_host: *const clap_host, _flag: u32) -> bool {
    // Everything a rescan can change is re-read wholesale at the next
    // deactivate/activate, so there is no flag this host cannot handle.
    true
}

unsafe extern "C" fn audio_ports_rescan(host: *const clap_host, _flags: u32) {
    if let Some(shim) = unsafe { shim(host) } {
        shim.audio_ports.store(true, Ordering::Release);
        // The graph builds a node's sockets from the bus layout (§14.2), so
        // this is a structural change and not just a restart.
        shim.context.request_restart(RestartReason::IoConfig);
    }
}

// --- clap.note-ports ------------------------------------------------------

static HOST_NOTE_PORTS: clap_host_note_ports = clap_host_note_ports {
    supported_dialects: Some(note_supported_dialects),
    rescan: Some(note_ports_rescan),
};

unsafe extern "C" fn note_supported_dialects(_host: *const clap_host) -> clap_note_dialect {
    // CLAP notes carry a note id, which is what per-voice work needs (§3.2);
    // MIDI is offered as well because a plugin that speaks only MIDI is common
    // and the core model has a raw-MIDI event for exactly this.
    CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI
}

unsafe extern "C" fn note_ports_rescan(host: *const clap_host, _flags: u32) {
    if let Some(shim) = unsafe { shim(host) } {
        shim.audio_ports.store(true, Ordering::Release);
        shim.context.request_restart(RestartReason::IoConfig);
    }
}

// --- clap.voice-info ------------------------------------------------------

static HOST_VOICE_INFO: clap_host_voice_info = clap_host_voice_info {
    changed: Some(voice_info_changed),
};

unsafe extern "C" fn voice_info_changed(host: *const clap_host) {
    if let Some(shim) = unsafe { shim(host) } {
        // Re-read on the next tick rather than here: the plugin may call this
        // from anywhere, and asking it a question back inside its own callback
        // is the shape of every reentrancy bug in this crate.
        shim.voice_info.store(true, Ordering::Release);
    }
}

// --- clap.tail ------------------------------------------------------------

static HOST_TAIL: clap_host_tail = clap_host_tail {
    changed: Some(tail_changed),
};

unsafe extern "C" fn tail_changed(_host: *const clap_host) {
    // The graph never trims a tail: a plugin node runs whenever the program
    // reaches it, so the length of its ring-out changes nothing the host has to
    // recompute.
}
