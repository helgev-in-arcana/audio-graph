//! The traits every backend implements (ARCHITECTURE.md §4).
//!
//! Two rules drive the shape here:
//!
//! * Nothing that cannot cross a process boundary appears in a signature, so
//!   an out-of-process backend (ADR-6) is a drop-in replacement rather than a
//!   rewrite. That is why there are no single-shot getters and no `Arc`s.
//! * Main-thread and audio-thread surfaces are *different traits*, so calling
//!   `process` on an inactive plugin is a compile error rather than a rule in a
//!   document. `activate` hands out the processor by value; you cannot hold one
//!   without having activated.

use crate::buffers::{AudioBuffers, AudioConfig};
use crate::events::{Event, EventSink, TimeContext};
use crate::params::{Capabilities, ParamId, ParamInfo, ParamSnapshot};
use crate::Result;

/// What the sub-plugin reported about its output for this block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStatus {
    /// Output is silent and will stay silent until new input arrives.
    Silent,
    /// Output is non-silent, or a tail is still ringing out.
    Continue,
    /// The plugin failed; the caller should bypass it.
    Error,
}

/// Services the host offers a backend.
///
/// `vst3-host` never builds an `IHostApplication` of its own — it is injected
/// through this trait. That keeps "forwarded from the DAW" out of the core's
/// vocabulary entirely, so a standalone scanner and the nested wrapper are
/// expressed by the same types (§7).
///
/// All methods are called on the main thread.
pub trait HostContext: Send + Sync {
    /// Shown to the plugin; some plugins branch on it.
    fn host_name(&self) -> &str;

    /// The plugin asked to be restarted (parameters changed, latency changed,
    /// I/O changed). The host decides when to honour it.
    fn request_restart(&self, reason: RestartReason);

    /// The plugin's reported latency changed. `subhost-adapter` combines this
    /// with the wrapper's own latency and reports the sum to the DAW (§7.4).
    fn latency_changed(&self, samples: u32) {
        let _ = samples;
        self.request_restart(RestartReason::Latency);
    }

    /// The sub-plugin edited a parameter from its own GUI.
    ///
    /// v1 swallows this — in Drive mode the wrapper is the sole authority for
    /// values, so there is nothing to forward to the DAW (§7.5) — but it is
    /// still logged.
    fn param_edited(&self, id: ParamId, plain: f64) {
        let _ = (id, plain);
    }
}

/// Why a plugin wants to be restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartReason {
    /// Parameter values changed behind the host's back.
    ParamValues,
    /// Parameter titles/units changed; re-read the list.
    ParamTitles,
    /// The parameter *set* changed (added/removed).
    ParamList,
    Latency,
    /// Bus arrangement changed.
    IoConfig,
}

/// Main-thread surface of a loaded sub-plugin.
///
/// Deliberately not `Send`: both VST3 and CLAP pin these calls to the thread
/// that created the instance.
pub trait SubPluginMain {
    /// Full parameter list. Batched by construction — there is no
    /// `param(id)` accessor anywhere in this API (§4.1).
    fn params(&self) -> &[ParamInfo];

    fn capabilities(&self) -> Capabilities;

    /// Current values of every parameter, in one round trip.
    fn snapshot(&self) -> ParamSnapshot;

    /// Format the value the way the plugin itself would.
    ///
    /// Delegated rather than formatted locally: units and enum labels are the
    /// plugin's business, not ours.
    fn param_to_text(&self, id: ParamId, plain: f64) -> Option<String>;

    fn param_from_text(&self, id: ParamId, text: &str) -> Option<f64>;

    /// Set a parameter outside of processing (main thread, e.g. loading a
    /// preset or the user turning a knob while stopped).
    fn set_param(&mut self, id: ParamId, plain: f64) -> Result<()>;

    /// Opaque state blob. Contents belong to the plugin.
    fn save_state(&self) -> Result<Vec<u8>>;

    fn load_state(&mut self, data: &[u8]) -> Result<()>;

    /// Reported processing latency in samples, valid once activated.
    fn latency_samples(&self) -> u32;

    /// Enter the processing phase, yielding the audio-thread half.
    ///
    /// Ownership transfer is the point: while the processor exists, the
    /// configuration cannot be changed.
    fn activate(&mut self, config: AudioConfig) -> Result<Box<dyn SubPluginProcessor>>;

    /// Leave the processing phase. The processor must be handed back so it
    /// cannot outlive the active state.
    fn deactivate(&mut self, processor: Box<dyn SubPluginProcessor>);
}

/// Audio-thread surface. `Send` so it can be moved to the audio thread once,
/// but never `Sync` — one thread owns it.
pub trait SubPluginProcessor: Send {
    /// Render one block. Must not allocate, lock, or block.
    ///
    /// `events` is ordered by `sample_offset`.
    fn process(
        &mut self,
        buffers: &mut AudioBuffers<'_>,
        events: &[Event],
        context: &TimeContext,
        out_events: &mut EventSink,
    ) -> ProcessStatus;

    /// Discard tails and internal state (transport jump, panic button).
    fn reset(&mut self);
}
