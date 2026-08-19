//! The shared implementation behind both exported classes.

use std::sync::Arc;

use nice_plug::prelude::*;
use plugin_host_api::{
    AudioBuffers, AudioConfig, BufferLayout, Event, EventSink, NoteEvent as ApiNote,
    ProcessStatus as ApiStatus, TimeContext,
};
use subhost_adapter::{SLOT_COUNT, SubHost, SubHostProcessor, WrapperState};

use crate::host_context::WrapperHostContext;
use crate::params::WrapperParams;

/// Which form the DAW loaded. Only the bus layout differs, but the sub-plugin
/// has to be activated with a matching input channel count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapperKind {
    Effect,
    Instrument,
}

pub struct Wrapper {
    params: Arc<WrapperParams>,
    context: Arc<WrapperHostContext>,
    /// Main-thread side. `None` until the DAW activates us.
    sub: SubHost,
    /// Audio-thread side, handed over by `activate`.
    processor: Option<SubHostProcessor>,

    /// Scratch reused every block; `process` must not allocate.
    slot_values: Vec<f64>,
    events: Vec<Event>,
    out_events: EventSink,
    input_scratch: Vec<f32>,
    output_scratch: Vec<f32>,

    kind: WrapperKind,
    channels: u32,
}

impl Default for Wrapper {
    fn default() -> Self {
        let context = Arc::new(WrapperHostContext::new());
        Wrapper {
            params: WrapperParams::new(),
            sub: SubHost::new(context.clone()),
            context,
            processor: None,
            slot_values: vec![0.0; SLOT_COUNT],
            events: Vec::new(),
            out_events: EventSink::new(),
            input_scratch: Vec::new(),
            output_scratch: Vec::new(),
            kind: WrapperKind::Effect,
            channels: 2,
        }
    }
}

impl Wrapper {
    pub fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    pub fn wrapper_params(&self) -> &Arc<WrapperParams> {
        &self.params
    }

    pub fn sub_host(&self) -> &SubHost {
        &self.sub
    }

    pub fn sub_host_mut(&mut self) -> &mut SubHost {
        &mut self.sub
    }

    /// Restore the wrapper's own state from the persisted blob.
    ///
    /// Called at activate rather than eagerly, because nice-plug restores
    /// persisted fields before activate and the sub-plugin has to be loaded
    /// with the sample rate the DAW is about to give us.
    fn restore_state(&mut self) {
        let json = self.params.state.0.read().unwrap().clone();
        if json.is_empty() {
            return;
        }
        match serde_json::from_str::<WrapperState>(&json) {
            Ok(state) => {
                for problem in self.sub.load_state(&state) {
                    // Not fatal by design (§8.3): a sub-plugin that cannot be
                    // found must not stop the project from opening, and the
                    // bindings are kept so reinstalling it brings them back.
                    log::warn!("audio-graph: {problem}");
                }
            }
            Err(e) => log::warn!("audio-graph: wrapper state unreadable: {e}"),
        }
    }

    /// Load a sub-plugin named by the environment, for development only.
    ///
    /// Until M4 there is no editor, so there is no way for a user to choose a
    /// sub-plugin — which would make it impossible to test the nesting in a
    /// real DAW, the one thing M3 is supposed to demonstrate. `AUDIO_GRAPH_SUB`
    /// names a `.vst3` to load, and `AUDIO_GRAPH_SUB_BIND` optionally gives a
    /// parameter id for slot 0 so automation can be checked too.
    ///
    /// Only consulted when nothing was restored from state, so it can never
    /// override a real project.
    fn load_development_override(&mut self) {
        // Never override a real project: state wins.
        if self.sub.is_loaded() {
            return;
        }
        let Ok(path) = std::env::var("AUDIO_GRAPH_SUB") else { return };
        if let Err(e) = self.sub.load(std::path::Path::new(&path), None) {
            log::warn!("audio-graph: AUDIO_GRAPH_SUB failed to load: {e}");
            return;
        }
        log::info!("audio-graph: loaded {path} via AUDIO_GRAPH_SUB");

        if let Ok(id) = std::env::var("AUDIO_GRAPH_SUB_BIND") {
            match id.parse::<u32>() {
                Ok(id) => {
                    if let Err(e) = self.sub.bind_slot(0, plugin_host_api::ParamId(id)) {
                        log::warn!("audio-graph: AUDIO_GRAPH_SUB_BIND: {e}");
                    }
                }
                Err(_) => log::warn!("audio-graph: AUDIO_GRAPH_SUB_BIND is not a number"),
            }
        }
        self.store_state();
    }

    /// Write the wrapper's state back into the persisted field.
    ///
    /// Called whenever the sub-plugin or the slot table changes, so the DAW has
    /// something current to save whenever it decides to.
    pub fn store_state(&self) {
        let state = self.sub.save_state();
        match serde_json::to_string(&state) {
            Ok(json) => *self.params.state.0.write().unwrap() = json,
            Err(e) => log::warn!("audio-graph: wrapper state unwritable: {e}"),
        }
    }

    /// Returns the latency to report, or `None` if activation failed.
    pub fn activate(
        &mut self,
        kind: WrapperKind,
        layout: &AudioIOLayout,
        config: &BufferConfig,
    ) -> Option<u32> {
        self.kind = kind;
        self.channels = layout.main_output_channels.map_or(2, |c| c.get());

        if self.sub.is_loaded() {
            // A second activate with a different configuration must not reuse
            // the old processor.
            self.deactivate();
        } else {
            self.restore_state();
            self.load_development_override();
        }

        let max_block = config.max_buffer_size;
        let input_channels = match kind {
            WrapperKind::Effect => layout.main_input_channels.map_or(0, |c| c.get()),
            WrapperKind::Instrument => 0,
        };

        self.input_scratch = vec![0.0; (input_channels.max(1) * max_block) as usize];
        self.output_scratch = vec![0.0; (self.channels * max_block) as usize];
        self.slot_values = vec![0.0; SLOT_COUNT];
        self.events = Vec::with_capacity(1024);
        self.out_events = EventSink::with_capacity(256);

        if !self.sub.is_loaded() {
            // Nothing loaded is a normal state, not a failure: the user has to
            // open the editor and pick something. The wrapper passes audio
            // through until then.
            return Some(0);
        }

        let audio_config = AudioConfig {
            sample_rate: config.sample_rate as f64,
            max_block_size: max_block,
            input_channels,
            output_channels: self.channels,
            offline: false,
        };

        match self.sub.activate(audio_config) {
            Ok(processor) => {
                self.processor = Some(processor);
                Some(self.sub.sub_latency())
            }
            Err(e) => {
                log::warn!("audio-graph: sub-plugin failed to activate: {e}");
                // Still a successful activation of *the wrapper*: it will pass
                // audio through. Refusing to load would lose the user's whole
                // patch over one plugin.
                Some(0)
            }
        }
    }

    pub fn deactivate(&mut self) {
        if let Some(processor) = self.processor.take() {
            self.sub.deactivate(processor);
        }
    }

    pub fn reset(&mut self) {
        if let Some(processor) = &mut self.processor {
            processor.reset();
        }
    }

    pub fn process<P: Plugin>(
        &mut self,
        buffer: &mut Buffer,
        context: &mut impl ProcessContext<P>,
    ) -> ProcessStatus {
        let frames = buffer.samples() as u32;
        let channels = buffer.channels() as u32;

        // Collect note input before touching audio: the sub-plugin wants a
        // single ordered event stream.
        self.events.clear();
        while let Some(event) = context.next_event() {
            if let Some(converted) = convert_note(&event) {
                if self.events.len() < self.events.capacity() {
                    self.events.push(Event::Note(converted));
                }
            }
        }

        let Some(processor) = &mut self.processor else {
            // Pass-through. `buffer` already holds the input, so an effect
            // needs no work; an instrument has nothing to play.
            if self.kind == WrapperKind::Instrument {
                for mut ch in buffer.iter_samples() {
                    for sample in ch.iter_mut() {
                        *sample = 0.0;
                    }
                }
            }
            return ProcessStatus::Normal;
        };

        self.params.slot_values(&mut self.slot_values);

        // nice-plug hands out per-channel slices; the host API wants one flat
        // planar block (§4.3), so the copy is the price of a boundary that can
        // later become shared memory.
        let frame_len = frames as usize;
        let input_channels = match self.kind {
            WrapperKind::Effect => channels.min(2),
            WrapperKind::Instrument => 0,
        };
        {
            let slices = buffer.as_slice_immutable();
            for ch in 0..input_channels as usize {
                let dst = &mut self.input_scratch[ch * frame_len..(ch + 1) * frame_len];
                dst.copy_from_slice(&slices[ch][..frame_len]);
            }
        }

        let out_channels = self.channels.min(channels);
        let transport = context.transport();
        let time = TimeContext {
            tempo_bpm: transport.tempo.unwrap_or(120.0),
            time_sig_numerator: transport.time_sig_numerator.unwrap_or(4),
            time_sig_denominator: transport.time_sig_denominator.unwrap_or(4),
            project_time_samples: transport.pos_samples().unwrap_or(0),
            project_time_music: transport.pos_beats().unwrap_or(0.0),
            bar_position_music: transport.bar_start_pos_beats().unwrap_or(0.0),
            playing: transport.playing,
            recording: transport.recording,
            loop_active: transport.loop_range_samples().is_some(),
        };

        self.out_events.clear();
        let status = {
            let mut buffers = AudioBuffers::new(
                &self.input_scratch[..(input_channels * frames).max(1) as usize],
                &mut self.output_scratch[..(out_channels * frames) as usize],
                input_channels,
                out_channels,
                frames,
                BufferLayout::Planar,
            );
            processor.process(
                &mut buffers,
                &self.slot_values,
                &self.events,
                &time,
                &mut self.out_events,
            )
        };

        if status == ApiStatus::Error {
            // Bypass rather than emit noise. The input is already in `buffer`.
            return ProcessStatus::Normal;
        }

        let output = buffer.as_slice();
        for ch in 0..out_channels as usize {
            output[ch][..frame_len]
                .copy_from_slice(&self.output_scratch[ch * frame_len..(ch + 1) * frame_len]);
        }
        // Channels the sub-plugin did not write must not keep the input.
        for ch in out_channels as usize..channels as usize {
            output[ch][..frame_len].fill(0.0);
        }

        match status {
            // The sub-plugin may have a tail we cannot measure, and an
            // instrument keeps sounding after its input stops.
            ApiStatus::Silent => ProcessStatus::Normal,
            _ => ProcessStatus::KeepAlive,
        }
    }

    /// Latency the sub-plugin asked to change since the last check (§7.4).
    pub fn take_latency_change(&self) -> Option<u32> {
        self.context.take_latency_change()
    }
}

/// nice-plug's note events into the host API's.
fn convert_note<S>(event: &NoteEvent<S>) -> Option<ApiNote> {
    Some(match *event {
        NoteEvent::NoteOn { timing, voice_id, channel, note, velocity } => ApiNote::NoteOn {
            note_id: voice_id.unwrap_or(note as i32),
            port: 0,
            channel: channel as i16,
            key: note as i16,
            velocity: velocity as f64,
            sample_offset: timing,
        },
        NoteEvent::NoteOff { timing, voice_id, channel, note, velocity } => ApiNote::NoteOff {
            note_id: voice_id.unwrap_or(note as i32),
            port: 0,
            channel: channel as i16,
            key: note as i16,
            velocity: velocity as f64,
            sample_offset: timing,
        },
        // Everything else — polyphonic expression, CCs, sysex — is dropped for
        // now. Note expression is a M5 source (§9.3) and is deliberately not
        // half-forwarded before the graph exists to consume it.
        _ => return None,
    })
}
