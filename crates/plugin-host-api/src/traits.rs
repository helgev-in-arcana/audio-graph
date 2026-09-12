//! Core traits for plugin hosting backends.
//!
//! Two rules drive the shape here:
//!
//! * Nothing that cannot cross a process boundary appears in a signature, so an
//!   out-of-process backend is a drop-in replacement rather than a rewrite.
//!   That is why there are no single-shot getters and no `Arc`s.
//! * Main-thread and audio-thread surfaces are *different traits*
//!   ([`SubPluginMain`] and [`SubPluginProcessor`]), so calling `process` on an
//!   inactive plugin is a compile error rather than a rule in a document.
//!   `activate` hands out the processor by value; you cannot hold one without
//!   having activated.

use crate::buffers::{AudioBuffers, AudioConfig};
use crate::events::{Event, EventSink, TimeContext};
use crate::params::{Capabilities, IoLayout, ParamId, ParamInfo, ParamSnapshot, VoiceInfo};
use crate::{Processor, Result};

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
/// expressed by the same types.
///
/// All methods are called on the main thread. Native restart requests are
/// coalesced and delivered by `SubPluginMain::tick`; GUI parameter edits and
/// activation latency can be reported synchronously. Callbacks must schedule
/// reconfiguration rather than reenter the same plugin. Call a final tick
/// before normal shutdown; undelivered requests are discarded on destruction.
pub trait HostContext: Send + Sync {
    /// Shown to the plugin; some plugins branch on it.
    fn host_name(&self) -> &str;

    /// The plugin asked to be restarted (parameters changed, latency changed,
    /// I/O changed). The host decides when to honour it.
    fn request_restart(&self, reason: RestartReason);

    /// The plugin's reported latency changed. `subhost-adapter` combines this
    /// with the wrapper's own latency and reports the sum to the DAW.
    fn latency_changed(&self, samples: u32) {
        let _ = samples;
        self.request_restart(RestartReason::Latency);
    }

    /// The sub-plugin edited a parameter from its own GUI.
    ///
    /// Swallowed today: the wrapper is the sole authority for values, so there
    /// is nothing to forward to the DAW. It is still logged.
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

/// Result of an explicit metadata refresh on the owning main thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MetadataUpdate {
    Unchanged,
    Refreshed,
    /// Return the processor, then retry before reading metadata for reactivation.
    NeedsDeactivation,
}

/// Main-thread surface of a loaded sub-plugin.
///
/// Deliberately not `Send`: both VST3 and CLAP pin these calls to the thread
/// that created the instance.
pub trait SubPluginMain {
    /// Service callbacks on the owning main thread, even with no editor open.
    /// Requests arriving during delivery remain pending for a subsequent tick.
    fn tick(&mut self) {}

    /// Complete pending descriptor updates, preserving requests that cannot yet be applied.
    /// A failure leaves the update pending and must not be followed by activation
    /// until a later refresh succeeds.
    fn refresh_metadata(&mut self) -> Result<MetadataUpdate> {
        Ok(MetadataUpdate::Unchanged)
    }

    /// Request main-bus widths while inactive. Read `io_layout` afterwards even
    /// on refusal: native negotiation may select a different arrangement.
    /// Auxiliary buses keep their declared widths until activation.
    fn request_main_bus_channels(&mut self, input: u16, output: u16) -> Result<()> {
        let layout = self.io_layout();
        if layout.main_input_channels() == input
            && layout.outputs.first().map_or(0, |bus| bus.channels) == output
        {
            Ok(())
        } else {
            Err(crate::HostError::UnsupportedBusConfig(
                "main bus widths are fixed".into(),
            ))
        }
    }

    /// Full parameter list. Batched by construction — there is no `param(id)`
    /// accessor anywhere in this API.
    fn params(&self) -> &[ParamInfo];

    fn capabilities(&self) -> Capabilities;

    /// How many voices an instrument has, if the format has a way to ask.
    ///
    /// Defaulted to `None` rather than made mandatory: CLAP's `voice-info` has
    /// no VST3 counterpart, and a backend that invented a number would be
    /// making one up. Nothing in the engine branches on it — it is reported,
    /// the way `params` is.
    fn voice_info(&self) -> Option<VoiceInfo> {
        None
    }

    /// Which note dialects the plugin's note input accepts, named.
    ///
    /// Purely diagnostic, and empty for a format that has no such notion —
    /// VST3 does not, having exactly one way to deliver a note. The question
    /// this answers is whether any plugin here would actually benefit from us
    /// speaking MIDI 2.0, which is otherwise easy to guess at and hard to know.
    fn note_dialects(&self) -> Vec<&'static str> {
        Vec::new()
    }

    /// Input ports whose native note dialect reports voice completion with `NoteEnd`.
    /// A host may reclaim other ports' note bookkeeping at delivery of NoteOff;
    /// that policy does not describe the duration of their audible release tails.
    fn note_end_ports(&self) -> Vec<i16> {
        Vec::new()
    }

    /// The plugin's audio buses and note input/output layout.
    ///
    /// Read after loading and used to build the node's sockets. Batched for the
    /// same reason as `params`: one round trip, so an out-of-process backend is
    /// not a per-bus conversation.
    fn io_layout(&self) -> IoLayout;

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

    /// Enter the processing phase, retaining every resource used by the processor.
    ///
    /// The returned handle stops its own activation when released on this thread,
    /// or when this thread next reclaims released resources.
    /// An existing activation must finish before another can start.
    /// Success guarantees that the native buffer plan matches `config`; unsupported
    /// dimensions must be rejected rather than silently adapted.
    fn activate(&mut self, config: AudioConfig) -> Result<Processor>;
}

/// Audio-thread surface. `Send` so it can be moved to the audio thread once,
/// but never `Sync` — one thread owns it.
pub trait SubPluginProcessor: Send {
    /// Render one block. Must not allocate, lock, or block.
    ///
    /// `events` is ordered by `sample_offset`.
    /// Buffers must match the activation's channels and aux buses, use planar
    /// layout, and fit its maximum block size. A mismatch returns `Error` with
    /// cleared audio output, without entering native processing. A matching
    /// zero-frame block does not enter native processing either.
    /// Output events append to `out_events` with call-relative sample offsets.
    /// The caller clears the sink before its collection interval and handles
    /// `overflowed()` afterwards; backends propagate native capacity failures.
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
