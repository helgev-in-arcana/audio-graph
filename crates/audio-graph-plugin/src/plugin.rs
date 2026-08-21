//! The shared implementation behind both exported classes.

use std::sync::Arc;

use nice_plug::prelude::*;
use plugin_host_api::{
    AudioConfig, Event, EventSink, NoteEvent as ApiNote, ProcessStatus as ApiStatus, TimeContext,
};
use subhost_adapter::{SLOT_COUNT, SlotSchedule, SubHost, WrapperState};
use audio_graph_engine::{BlockContext, Engine, Graph};

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
    /// Channel width of each of the wrapper's own input buses, main first.
    daw_inputs: Vec<u16>,
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
            daw_inputs: Vec::new(),
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
            // A fresh instance, or a project saved before the wrapper ever
            // wrote anything. Publish the defaults rather than leaving the
            // field empty: what we are running under should be in the file.
            self.shared.publish_graph();
            self.shared.store_state();
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
        self.shared.adopt_default_patch();
        self.shared.publish_graph();
        // Publish it back even when nothing was restored. A project saved
        // without the editor ever being opened would otherwise store the empty
        // string, and then the defaults it was running under -- the sub-block
        // size among them -- would not be in the file at all.
        self.shared.store_state();
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
        if self.shared.main().host.is_loaded(0) {
            return;
        }
        let Ok(path) = std::env::var("AUDIO_GRAPH_SUB") else {
            return;
        };
        if let Err(e) = self
            .shared
            .main()
            .host
            .load(0, std::path::Path::new(&path), None)
        {
            log::warn!("audio-graph: AUDIO_GRAPH_SUB failed to load: {e}");
            return;
        }
        log::info!("audio-graph: loaded {path} via AUDIO_GRAPH_SUB");

        if let Ok(id) = std::env::var("AUDIO_GRAPH_SUB_BIND") {
            match id.parse::<u32>() {
                Ok(id) => {
                    if let Err(e) =
                        self.shared
                            .main()
                            .host
                            .bind_slot(0, 0, plugin_host_api::ParamId(id))
                    {
                        log::warn!("audio-graph: AUDIO_GRAPH_SUB_BIND: {e}");
                    }
                }
                Err(_) => log::warn!("audio-graph: AUDIO_GRAPH_SUB_BIND is not a number"),
            }
        }
        self.shared.adopt_default_patch();
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

        if self.shared.main().host.is_loaded(0) {
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
        // The wrapper's own input buses: the main one, then whatever the DAW
        // gave us for aux (§14.11). An `AudioIn` node names a bus by index into
        // this, so it is what the engine has to be told about.
        self.daw_inputs = std::iter::once(input_channels)
            .filter(|&c| c > 0)
            .chain(layout.aux_input_ports.iter().map(|c| c.get()))
            .map(|c| c.min(2) as u16)
            .collect();
        let total_in: u32 = self.daw_inputs.iter().map(|&c| u32::from(c)).sum();

        self.input_scratch = vec![0.0; (total_in.max(1) * max_block) as usize];
        self.output_scratch = vec![0.0; (self.channels * max_block) as usize];
        self.daw_slots = vec![0.0; SLOT_COUNT];
        self.events = Vec::with_capacity(1024);
        self.out_events = EventSink::with_capacity(256);
        // Every allocation the audio path needs happens here. `SlotSchedule`
        // is sized for the finest sub-block on offer, so the user can change
        // the modulation rate mid-playback without this being redone.
        self.schedule = SlotSchedule::new(max_block, self.shared.quantum());
        // The graph's audio buffers (§14.7). Sized for the ceilings rather than
        // for the current patch, so a recompile never asks for memory.
        self.engine.prepare(max_block, &self.daw_inputs.clone());
        // The editor needs it to show a delay's floor in seconds (§14.4). It
        // never reaches the audio path this way — that gets the rate from the
        // transport, block by block.
        self.shared.set_sample_rate(config.sample_rate);

        let audio_config = AudioConfig {
            sample_rate: config.sample_rate as f64,
            max_block_size: max_block,
            input_channels,
            output_channels: self.channels,
            aux_inputs: Default::default(),
            offline: false,
        };
        // Remembered even when nothing is loaded: the editor uses it to
        // activate whatever the user picks next, without waiting for the DAW to
        // call `activate` again.
        let mut state = self.shared.main();
        state.config = Some(audio_config);

        if !state.host.any_loaded() {
            // Nothing loaded is a normal state, not a failure: the user has to
            // open the editor and pick something. The wrapper passes audio
            // through until then.
            return Some(0);
        }

        let io = state.instance_io.clone();
        let graph_params = state.graph_params.clone();
        match state.host.activate(audio_config, &io, &graph_params) {
            Ok(processor) => {
                // What to report depends on how the audio is routed. A graph
                // knows its own longest path (§14.6); the direct path is one
                // plugin, so its latency is the plugin's.
                let latency = self.engine.latency().max(state.host.sub_latency(0));
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
        aux: &mut AuxiliaryBuffers,
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
        // Nothing loaded is not the same as nothing to do: a patch can be a
        // delay line and a mix with no sub-plugin anywhere in it (§14.4), and
        // passing the input through would be exactly the invisible route
        // §14.13 got rid of. The graph runs either way; a plugin node with no
        // plugin behind it produces silence, which `NoNodes` is.
        let processor = state.processor.as_mut();

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
        // The main bus first, then each aux bus, packed — the same layout a
        // plugin's input region uses (§14.11).
        let mut at = 0usize;
        {
            let slices = buffer.as_slice_immutable();
            for ch in 0..input_channels as usize {
                let dst = &mut self.input_scratch[at..at + frame_len];
                dst.copy_from_slice(&slices[ch][..frame_len]);
                at += frame_len;
            }
        }
        for bus in 0..aux.inputs.len() {
            let slices = aux.inputs[bus].as_slice_immutable();
            let width = self
                .daw_inputs
                .get(usize::from(input_channels > 0) + bus)
                .copied()
                .unwrap_or(0) as usize;
            for ch in 0..width {
                let dst = &mut self.input_scratch[at..at + frame_len];
                match slices.get(ch) {
                    Some(src) => dst.copy_from_slice(&src[..frame_len]),
                    // A DAW is allowed to hand us fewer channels than it
                    // promised on a bus nobody connected.
                    None => dst.fill(0.0),
                }
                at += frame_len;
            }
        }
        let total_in = at / frame_len.max(1);

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
        let status = if self.engine.has_audio() {
            // The graph decides where the audio goes and which plugins see it
            // (§14). Sub-plugins are reached through `AudioNodes`, so the
            // engine still knows nothing about what a plugin is.
            let mut loaded;
            let mut empty = audio_graph_engine::NoNodes;
            let nodes: &mut dyn audio_graph_engine::AudioNodes = match processor {
                Some(processor) => {
                    loaded =
                        processor.nodes(&self.schedule, &self.events, &time, &mut self.out_events);
                    &mut loaded
                }
                None => &mut empty,
            };
            self.engine.run_audio(
                &audio_graph_engine::AudioContext {
                    frames,
                    quantum: self.schedule.quantum(),
                    sample_rate: transport.sample_rate as f64,
                    // The same buffer the parameter lanes ride in. The audio
                    // half reads only the delay-time range out of it (§14.5).
                    lanes: self.schedule.rows(),
                    lanes_per_row: subhost_adapter::LANES,
                },
                &self.input_scratch[..(total_in as u32 * frames).max(1) as usize],
                &mut self.output_scratch[..(out_channels * frames) as usize],
                nodes,
            );
            ApiStatus::Continue
        } else {
            // Nothing is drawn between the input and the output, so nothing
            // comes out. The wrapper used to pass audio through here — and to
            // run a lone sub-plugin straight through — but an audible route the
            // canvas does not show is a route the user cannot edit, so the only
            // routing left is the one in the graph. `Graph::default_patch`
            // draws the through-connection a new instance starts with.
            for ch in 0..channels as usize {
                buffer.as_slice()[ch][..frame_len].fill(0.0);
            }
            return ProcessStatus::Normal;
        };

        if status == ApiStatus::Error {
            // Silence rather than noise, and rather than a bypass nothing on
            // the canvas asked for.
            for ch in 0..channels as usize {
                buffer.as_slice()[ch][..frame_len].fill(0.0);
            }
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
        // The DAW's automation fills the slot lanes; the rest are the graph's
        // own parameter lanes (§14.12) and start from nothing. Zeroing rather
        // than leaving the previous sub-block's values means a lane the graph
        // stops driving stops sending, instead of repeating a stale value.
        let slots = daw_slots.len().min(values.len());
        values[..slots].copy_from_slice(&daw_slots[..slots]);
        values[slots..].fill(0.0);
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

#[cfg(test)]
mod tests {
    use super::*;
    use subhost_adapter::LANES;
    use audio_graph_engine::{Graph, MathOp, NodeKind, Rate, Waveform, compile};

    /// A schedule carries more than the DAW's slots (§14.12), and `run_graph`
    /// fills the difference itself.
    ///
    /// Worth its own test because nothing else goes through this function: the
    /// engine's tests drive it directly with a slot-sized slice, so widening
    /// the schedule broke only the real audio path, and only in a DAW.
    #[test]
    fn a_block_is_filled_to_the_schedules_width_whether_or_not_a_graph_runs() {
        let daw_slots = vec![0.42; SLOT_COUNT];
        let mut engine = Engine::new();
        let mut schedule = SlotSchedule::new(512, 32);

        // No program: every sub-block is the DAW's values, and the graph's own
        // lanes are quiet.
        run_graph(
            &mut engine,
            &mut schedule,
            &daw_slots,
            &[],
            128,
            32,
            48_000.0,
            120.0,
        );
        assert!(schedule.blocks() > 0);
        for index in 0..schedule.blocks() {
            let block = schedule.block(index);
            assert_eq!(block.len(), LANES);
            assert!(block[..SLOT_COUNT].iter().all(|&v| v == 0.42));
            assert!(
                block[SLOT_COUNT..].iter().all(|&v| v == 0.0),
                "a lane nothing drives has to be quiet"
            );
        }

        // With one: same width, and the driven slot has moved.
        let mut graph = Graph::new();
        let lfo = graph.add(
            NodeKind::Lfo {
                waveform: Waveform::Saw,
                rate: Rate::Hz(4.0),
                phase: 0.0,
                depth: 0.5,
                offset: 0.5,
            },
            [0.0, 0.0],
        );
        let scale = graph.add(
            NodeKind::Math {
                op: MathOp::Multiply,
                b: 1.0,
            },
            [100.0, 0.0],
        );
        let out = graph.add(NodeKind::SlotOut { slot: 7 }, [200.0, 0.0]);
        graph.connect(lfo, 0, scale, 0);
        graph.connect(scale, 0, out, 0);

        let handoff = audio_graph_engine::Handoff::new();
        handoff.send(Box::new(compile(&graph, SLOT_COUNT).unwrap()));
        assert!(engine.adopt(&handoff));

        run_graph(
            &mut engine,
            &mut schedule,
            &daw_slots,
            &[],
            128,
            32,
            48_000.0,
            120.0,
        );
        for index in 0..schedule.blocks() {
            let block = schedule.block(index);
            assert_eq!(block.len(), LANES);
            assert_eq!(block[0], 0.42, "an undriven slot stays the DAW's");
            assert!(
                block[SLOT_COUNT..].iter().all(|&v| v == 0.0),
                "no parameter socket is wired, so no lane is driven"
            );
        }
    }
}
