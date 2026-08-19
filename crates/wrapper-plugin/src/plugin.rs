//! The shared implementation behind both exported classes.

use std::sync::Arc;

use nice_plug::prelude::*;
use plugin_host_api::{
    AudioBuffers, AudioConfig, BufferLayout, Event, EventSink, NoteEvent as ApiNote,
    ProcessStatus as ApiStatus, TimeContext,
};
use subhost_adapter::{SLOT_COUNT, SlotSchedule, SubHost, WrapperState};
use wrapper_engine::{BlockContext, Engine, Graph};

use crate::host_context::WrapperHostContext;
use crate::params::WrapperParams;
use crate::shared::Shared;

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
    /// The sub-plugin and its processor, shared with the editor.
    ///
    /// Behind a lock rather than held by value, because the editor can load a
    /// different sub-plugin at any moment; see [`crate::shared`].
    shared: Arc<Shared>,

    /// The node graph, running (§9). Holds its own program; the editor
    /// publishes new ones through `shared`.
    engine: Engine,
    /// Slot values at each sub-block boundary (§9.2).
    schedule: SlotSchedule,

    /// Scratch reused every block; `process` must not allocate.
    ///
    /// The DAW's automation, before the graph has had a say.
    daw_slots: Vec<f64>,
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
        let params = WrapperParams::new();
        let shared = Shared::new(SubHost::new(context.clone()), params.clone());
        Wrapper {
            params,
            context,
            shared,
            engine: Engine::new(),
            schedule: SlotSchedule::new(0, subhost_adapter::DEFAULT_QUANTUM),
            daw_slots: vec![0.0; SLOT_COUNT],
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

    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
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
                self.shared.set_quantum(state.sub_block);
                match state
                    .graph
                    .as_ref()
                    .map(|g| serde_json::from_value::<Graph>(g.clone()))
                {
                    Some(Ok(mut graph)) => {
                        // A graph saved against a different slot count, or by a
                        // version whose node kinds have since changed, can hold
                        // links that no longer mean anything.
                        graph.prune();
                        self.shared.main().graph = graph;
                    }
                    Some(Err(e)) => log::warn!("audio-graph: node graph unreadable: {e}"),
                    None => {}
                }
                for problem in self.shared.main().host.load_state(&state) {
                    // Not fatal by design (§8.3): a sub-plugin that cannot be
                    // found must not stop the project from opening, and the
                    // bindings are kept so reinstalling it brings them back.
                    log::warn!("audio-graph: {problem}");
                }
            }
            Err(e) => log::warn!("audio-graph: wrapper state unreadable: {e}"),
        }
        self.shared.publish_graph();
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
        if self.shared.main().host.is_loaded() {
            return;
        }
        let Ok(path) = std::env::var("AUDIO_GRAPH_SUB") else {
            return;
        };
        if let Err(e) = self
            .shared
            .main()
            .host
            .load(std::path::Path::new(&path), None)
        {
            log::warn!("audio-graph: AUDIO_GRAPH_SUB failed to load: {e}");
            return;
        }
        log::info!("audio-graph: loaded {path} via AUDIO_GRAPH_SUB");

        if let Ok(id) = std::env::var("AUDIO_GRAPH_SUB_BIND") {
            match id.parse::<u32>() {
                Ok(id) => {
                    if let Err(e) = self
                        .shared
                        .main()
                        .host
                        .bind_slot(0, plugin_host_api::ParamId(id))
                    {
                        log::warn!("audio-graph: AUDIO_GRAPH_SUB_BIND: {e}");
                    }
                }
                Err(_) => log::warn!("audio-graph: AUDIO_GRAPH_SUB_BIND is not a number"),
            }
        }
        self.shared.store_state();
    }

    /// Write the wrapper's state back into the persisted field.
    ///
    /// Called whenever the sub-plugin or the slot table changes, so the DAW has
    /// something current to save whenever it decides to.
    pub fn store_state(&self) {
        self.shared.store_state();
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

        if self.shared.main().host.is_loaded() {
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
        self.daw_slots = vec![0.0; SLOT_COUNT];
        self.events = Vec::with_capacity(1024);
        self.out_events = EventSink::with_capacity(256);
        // Every allocation the audio path needs happens here. `SlotSchedule`
        // is sized for the finest sub-block on offer, so the user can change
        // the modulation rate mid-playback without this being redone.
        self.schedule = SlotSchedule::new(max_block, self.shared.quantum());

        let audio_config = AudioConfig {
            sample_rate: config.sample_rate as f64,
            max_block_size: max_block,
            input_channels,
            output_channels: self.channels,
            offline: false,
        };
        // Remembered even when nothing is loaded: the editor uses it to
        // activate whatever the user picks next, without waiting for the DAW to
        // call `activate` again.
        let mut state = self.shared.main();
        state.config = Some(audio_config);

        if !state.host.is_loaded() {
            // Nothing loaded is a normal state, not a failure: the user has to
            // open the editor and pick something. The wrapper passes audio
            // through until then.
            return Some(0);
        }

        match state.host.activate(audio_config) {
            Ok(processor) => {
                let latency = state.host.sub_latency();
                drop(state);
                self.shared.audio().processor = Some(processor);
                Some(latency)
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
        let processor = self.shared.audio().processor.take();
        if let Some(processor) = processor {
            self.shared.main().host.deactivate(processor);
        }
        // The audio thread will not run again until the next activate, so give
        // the program back now rather than leaving the main thread's `Handoff`
        // holding a value nobody will ever collect.
        drop(self.engine.release());
    }

    pub fn reset(&mut self) {
        self.engine.reset();
        // Blocking, not `try_lock`: a skipped reset leaves the sub-plugin with
        // stale parameter state and hanging notes, and this is called when the
        // transport jumps rather than while audio is flowing.
        if let Some(processor) = &mut self.shared.audio().processor {
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

        // Pick up a freshly compiled graph, if the editor has published one.
        // Lock-free in both directions, so this costs nothing on the blocks
        // where nothing has changed — which is nearly all of them.
        self.engine.adopt(self.shared.programs());

        // `try_lock`, never `lock`. The main thread holds this only to start or
        // stop a sub-plugin; missing it means a few blocks pass through
        // unprocessed, which is the audible cost of swapping a plugin
        // mid-playback, and never a blocked audio thread. Graph edits do not
        // come this way at all.
        let mut state = match self.shared.try_audio() {
            Some(state) => state,
            None => return pass_through(buffer, self.kind),
        };
        let Some(processor) = &mut state.processor else {
            // Pass-through. `buffer` already holds the input, so an effect
            // needs no work; an instrument has nothing to play.
            return pass_through(buffer, self.kind);
        };

        self.params.slot_values(&mut self.daw_slots);
        run_graph(
            &mut self.engine,
            &mut self.schedule,
            &self.daw_slots,
            &self.events,
            frames,
            self.shared.quantum(),
            context.transport().sample_rate as f64,
            context.transport().tempo.unwrap_or(120.0),
        );
        // What the editor's meters show. The DAW's own parameter value stops
        // being the answer the moment the graph drives a slot.
        self.shared
            .report_slots(self.schedule.block(self.schedule.blocks() - 1));

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
                &self.schedule,
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

/// Fill the sub-block schedule: the DAW's automation, with the graph's outputs
/// written over the slots it drives (§9.2).
///
/// Note events are folded into the engine as the sub-block boundaries pass
/// them, so a modulator reading pressure sees the value that was current at
/// that point in the block rather than the one the block ended on.
///
/// A free function rather than a method because it needs three fields of
/// `Wrapper` mutably at once while the audio lock is held, and spelling the
/// borrows out is clearer than arguing with the compiler about them.
#[allow(clippy::too_many_arguments)]
fn run_graph(
    engine: &mut Engine,
    schedule: &mut SlotSchedule,
    daw_slots: &[f64],
    events: &[Event],
    frames: u32,
    quantum: u32,
    sample_rate: f64,
    tempo_bpm: f64,
) {
    if schedule.quantum() != quantum {
        // Allocation-free by construction; see `SlotSchedule`.
        schedule.set_quantum(quantum);
    }
    let blocks = schedule.begin(frames);

    if !engine.has_program() {
        // Nothing to evaluate. One value per slot for the whole block is
        // exactly the shape the wrapper produced before M5, so a project with
        // no graph behaves identically — including sending no more events.
        schedule.fill(daw_slots);
        return;
    }

    let mut next_event = 0;
    for index in 0..blocks {
        let start = schedule.offset(index);
        while next_event < events.len() && events[next_event].sample_offset() <= start {
            if let Event::Note(note) = events[next_event] {
                engine.note(&note);
            }
            next_event += 1;
        }

        let context = BlockContext {
            sample_rate,
            tempo_bpm,
            frames: schedule.frames_of(index),
        };
        let values = schedule.block_mut(index);
        values.copy_from_slice(daw_slots);
        engine.run(&context, values);
    }

    // Whatever is left lands after the final boundary. Folding it in now means
    // the next block starts from the right state rather than rediscovering it.
    for event in &events[next_event..] {
        if let Event::Note(note) = *event {
            engine.note(&note);
        }
    }
}

/// Leave the input alone (an effect) or silence the output (an instrument).
///
/// The fallback whenever there is no sub-plugin to run, whether because none is
/// loaded or because the editor currently holds the lock.
fn pass_through(buffer: &mut Buffer, kind: WrapperKind) -> ProcessStatus {
    if kind == WrapperKind::Instrument {
        for mut ch in buffer.iter_samples() {
            for sample in ch.iter_mut() {
                *sample = 0.0;
            }
        }
    }
    ProcessStatus::Normal
}

/// nice-plug's note events into the host API's.
///
/// Everything the graph can read has to arrive here first. Before M5 only note
/// on and off were forwarded, because note expression was a §9.3 source with
/// nothing yet to consume it; now there is.
fn convert_note<S>(event: &NoteEvent<S>) -> Option<ApiNote> {
    use plugin_host_api::NoteExpression as Expr;

    // The poly expressions all carry the same fields under different names, so
    // the shape is factored out and only the value differs.
    fn expression(
        timing: u32,
        voice_id: Option<i32>,
        channel: u8,
        note: u8,
        expression: Expr,
        value: f32,
    ) -> ApiNote {
        ApiNote::Expression {
            note_id: voice_id.unwrap_or(note as i32),
            port: 0,
            channel: channel as i16,
            key: note as i16,
            expression,
            value: value as f64,
            sample_offset: timing,
        }
    }

    Some(match *event {
        NoteEvent::NoteOn {
            timing,
            voice_id,
            channel,
            note,
            velocity,
        } => ApiNote::NoteOn {
            note_id: voice_id.unwrap_or(note as i32),
            port: 0,
            channel: channel as i16,
            key: note as i16,
            velocity: velocity as f64,
            sample_offset: timing,
        },
        NoteEvent::NoteOff {
            timing,
            voice_id,
            channel,
            note,
            velocity,
        } => ApiNote::NoteOff {
            note_id: voice_id.unwrap_or(note as i32),
            port: 0,
            channel: channel as i16,
            key: note as i16,
            velocity: velocity as f64,
            sample_offset: timing,
        },
        NoteEvent::VoiceTerminated {
            timing,
            voice_id,
            channel,
            note,
        } => ApiNote::NoteEnd {
            note_id: voice_id.unwrap_or(note as i32),
            port: 0,
            channel: channel as i16,
            key: note as i16,
            sample_offset: timing,
        },
        NoteEvent::PolyPressure {
            timing,
            voice_id,
            channel,
            note,
            pressure,
        } => expression(timing, voice_id, channel, note, Expr::Pressure, pressure),
        NoteEvent::PolyVolume {
            timing,
            voice_id,
            channel,
            note,
            gain,
        } => expression(timing, voice_id, channel, note, Expr::Volume, gain),
        NoteEvent::PolyPan {
            timing,
            voice_id,
            channel,
            note,
            pan,
        } => expression(timing, voice_id, channel, note, Expr::Pan, pan),
        NoteEvent::PolyTuning {
            timing,
            voice_id,
            channel,
            note,
            tuning,
        } => expression(timing, voice_id, channel, note, Expr::Tuning, tuning),
        NoteEvent::PolyVibrato {
            timing,
            voice_id,
            channel,
            note,
            vibrato,
        } => expression(timing, voice_id, channel, note, Expr::Vibrato, vibrato),
        NoteEvent::PolyExpression {
            timing,
            voice_id,
            channel,
            note,
            expression: value,
        } => expression(timing, voice_id, channel, note, Expr::Expression, value),
        NoteEvent::PolyBrightness {
            timing,
            voice_id,
            channel,
            note,
            brightness,
        } => expression(
            timing,
            voice_id,
            channel,
            note,
            Expr::Brightness,
            brightness,
        ),
        // CCs, pitch bend and sysex still go nowhere. They are host-level MIDI
        // rather than per-note expression, and the v1 node set (§9.3) has no
        // source that reads them; forwarding them to the sub-plugin blindly
        // would be a separate decision about MIDI routing.
        _ => return None,
    })
}
