//! The shared implementation behind both exported classes.

use std::sync::Arc;

use crate::config::{LANES, SLOT_COUNT, SUB_HOST};
use crate::state::WrapperState;
use audio_graph_engine::{BlockContext, Ended, Engine, Graph, MAX_LIVE_NOTES};
use nice_plug::prelude::*;
use plugin_host::{
    AudioConfig, Event, EventSink, NoteEvent as ApiNote, ProcessStatus as ApiStatus, TimeContext,
};
use subhost_adapter::{SlotSchedule, SubHost};

use crate::host_context::WrapperHostContext;
use crate::params::WrapperParams;
use crate::shared::Shared;
use crate::tick::{Task, TickState, Ticker};

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

    /// The active node graph engine. Holds its own compiled program; the editor
    /// publishes updates through `shared`.
    engine: Engine,
    /// Slot and parameter values evaluated at each sub-block boundary.
    schedule: SlotSchedule,

    /// Scratch reused every block; `process` must not allocate.
    ///
    /// The DAW's automation, before the graph has had a say.
    daw_slots: Vec<f64>,
    events: Vec<Event>,
    out_events: EventSink,
    /// Notes the graph has finished with, to be handed back to the DAW.
    ///
    /// Owned and sized at activate: the audio thread may not allocate, and the
    /// list has to live somewhere between the engine filling it and the
    /// wrapper sending it.
    ended_notes: Vec<Ended>,
    input_scratch: Vec<f32>,
    /// Channel width of each of the wrapper's own input buses, main first.
    daw_inputs: Vec<u16>,
    output_scratch: Vec<f32>,

    kind: WrapperKind,
    channels: u32,

    /// The periodic main-thread tick CLAP requires of us (see [`crate::tick`]).
    ///
    /// Started when nice-plug hands over the executor that can reach the main
    /// thread, which is at instance creation and not when a window opens. The
    /// state outlives the thread because the task executor holds it too.
    tick_state: Arc<TickState>,
    ticker: Option<Ticker>,
}

impl Default for Wrapper {
    fn default() -> Self {
        let context = Arc::new(WrapperHostContext::new());
        let params = WrapperParams::new();
        let shared = Shared::new(SubHost::new(context.clone(), SUB_HOST), params.clone());
        Wrapper {
            params,
            context,
            shared,
            engine: Engine::new(),
            schedule: SlotSchedule::new(LANES, 0, subhost_adapter::DEFAULT_QUANTUM),
            daw_slots: vec![0.0; SLOT_COUNT],
            events: Vec::new(),
            out_events: EventSink::new(),
            ended_notes: Vec::new(),
            input_scratch: Vec::new(),
            daw_inputs: Vec::new(),
            output_scratch: Vec::new(),
            kind: WrapperKind::Effect,
            channels: 2,
            tick_state: TickState::new(),
            ticker: None,
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

    /// What runs a [`Task`] once it has reached the main thread.
    ///
    /// Queried once, at instance creation, before [`Wrapper::start_ticking`].
    pub fn task_executor(&mut self) -> impl Fn(Task) + Send + 'static {
        let shared = self.shared.clone();
        let state = self.tick_state.clone();
        move |task| match task {
            Task::Tick => crate::tick::run(&shared, &state),
        }
    }

    /// Begin ticking, using `post` to reach the main thread.
    ///
    /// Called from `editor()`, which is where nice-plug hands over the only
    /// thing that can do that — and which it calls once when the instance is
    /// created, whether or not the user ever opens a window. That is the
    /// property the whole arrangement rests on.
    pub fn start_ticking(&mut self, post: impl Fn() + Send + 'static) {
        if self.ticker.is_none() {
            self.ticker = Some(Ticker::spawn(self.tick_state.clone(), post));
        }
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
                        self.shared.patch().graph = graph;
                    }
                    Some(Err(e)) => log::warn!("audio-graph: node graph unreadable: {e}"),
                    None => {}
                }
                for problem in self.shared.main().host.load_state(&state.sub_host_state()) {
                    // Not fatal by design: a sub-plugin that cannot be found
                    // must not stop the project from opening, and the bindings
                    // are kept so reinstalling it brings them back.
                    log::warn!("audio-graph: {problem}");
                }
            }
            Err(e) => log::warn!("audio-graph: wrapper state unreadable: {e}"),
        }
        self.shared.adopt_default_patch();
        self.shared.publish_graph();
        // Publish it back even when nothing was restored. A project saved
        // without the editor ever being opened would otherwise store the empty
        // string, and then the defaults it was running under — the sub-block
        // size among them — would not be in the file at all.
        self.shared.store_state();
    }

    /// Load a sub-plugin specified via environment variables, used for headless development and testing.
    ///
    /// `AUDIO_GRAPH_SUB` specifies a `.vst3` or `.clap` path to load, and `AUDIO_GRAPH_SUB_BIND`
    /// optionally specifies a parameter ID for slot 0.
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
                            .bind_slot(0, 0, plugin_host::ParamId(id))
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

        // A processor built for an earlier configuration must not be reused,
        // and an instance that has never run has none to give back, so this
        // costs nothing on a first activation.
        self.deactivate();

        // Whether the DAW has put a project or a preset in front of us since we
        // last looked. Asking instead whether a sub-plugin is loaded answers a
        // different question, and gets both halves of this wrong: a patch whose
        // plugins sit in instances other than the first would be read back in
        // on every activation, and a preset dropped on a patch that does have
        // one would never be read at all.
        if self.shared.state_is_unseen() {
            // Nothing to start the blob's sub-plugins against yet: the
            // configuration this activation carries is settled further down,
            // and starting them here as well would leave a processor that
            // never gets handed back.
            self.shared.main().config = None;
            self.restore_state();
            self.load_development_override();
        }

        let max_block = config.max_buffer_size;
        let input_channels = match kind {
            WrapperKind::Effect => layout.main_input_channels.map_or(0, |c| c.get()),
            WrapperKind::Instrument => 0,
        };
        // The wrapper's own input buses: the main one, then whatever the DAW
        // gave us for aux. An `AudioIn` node names a bus by index into this, so
        // it is what the engine has to be told about.
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
        self.ended_notes = Vec::with_capacity(MAX_LIVE_NOTES);
        // Every allocation the audio path needs happens here. `SlotSchedule`
        // is sized for the finest sub-block on offer, so the user can change
        // the modulation rate mid-playback without this being redone.
        self.schedule = SlotSchedule::new(LANES, max_block, self.shared.quantum());
        // The graph's audio buffers, sized for the ceilings rather than for the
        // current patch, so a recompile never asks for memory.
        self.engine.prepare(max_block, &self.daw_inputs.clone());
        // The editor needs it to show a delay's floor in seconds. It never
        // reaches the audio path this way — that gets the rate from the
        // transport, block by block.
        self.shared.set_sample_rate(config.sample_rate);

        let audio_config = AudioConfig {
            sample_rate: config.sample_rate as f64,
            max_block_size: max_block,
            input_channels,
            output_channels: self.channels,
            aux_inputs: Default::default(),
            aux_outputs: Default::default(),
            // A DAW re-activates on every render mode change, so the mode
            // named here is the one this run is under. A sub-plugin that
            // trades accuracy for latency to keep up with a sound card is
            // entitled to know when it no longer has to.
            offline: config.process_mode == ProcessMode::Offline,
        };
        // Remembered even when nothing is loaded: the editor uses it to
        // activate whatever the user picks next, without waiting for the DAW to
        // call `activate` again.
        self.shared.main().config = Some(audio_config);

        // The engine holds no program at this point — `deactivate` hands it
        // back, and a first activation has never had one — so without this the
        // graph would fall silent for as long as the DAW keeps this
        // configuration, and a bounce would write that silence to the file.
        // It is also the only moment the delay rings can be sized for the rate
        // the DAW has just named.
        self.shared.send_fresh_program();

        {
            let mut state = self.shared.main();
            // Nothing loaded is a normal state, not a failure: the user has to
            // open the editor and pick something, and the graph draws a
            // through-connection until they do.
            if state.host.any_loaded() {
                let io = state.instance_io.clone();
                let graph_params = state.graph_params.clone();
                match state.host.activate(audio_config, &io, &graph_params) {
                    Ok(processor) => {
                        drop(state);
                        self.shared.audio().processor = Some(processor);
                    }
                    // Still a successful activation of *the wrapper*: the rest
                    // of the graph runs, and a plugin node with no plugin
                    // behind it produces silence. Refusing to load would lose
                    // the user's whole patch over one plugin.
                    Err(e) => log::warn!("audio-graph: sub-plugin failed to activate: {e}"),
                }
            }
        }

        // Activating a plugin is what makes it answerable about its latency, so
        // a node can be carrying a number from before its plugin had one — or
        // from a project whose plugin is no longer installed, where the honest
        // answer is none at all. The DAW is told what the graph costs, and that
        // can only be right if the nodes in it are.
        if self.shared.refresh_latencies() {
            self.shared.send_fresh_program();
        }
        Some(self.shared.latency())
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
            if let Some(converted) = convert_note(&event)
                && self.events.len() < self.events.capacity()
            {
                self.events.push(Event::Note(converted));
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
        // delay line and a mix with no sub-plugin anywhere in it, and passing
        // the input through would be exactly the invisible route the graph
        // exists to make visible. The graph runs either way; a plugin node with
        // no plugin behind it produces silence, which `NoInstances` is.
        let processor = state.processor.as_mut();

        self.params.slot_values(&mut self.daw_slots);
        let runnable = begin_graph(
            &mut self.engine,
            &mut self.schedule,
            &self.daw_slots,
            &self.events,
            frames,
            self.shared.quantum(),
        );

        // nice-plug hands out per-channel slices; the host API wants one flat
        // planar block, so the copy is the price of a boundary that could later
        // become shared memory.
        let frame_len = frames as usize;
        let input_channels = match self.kind {
            WrapperKind::Effect => channels.min(2),
            WrapperKind::Instrument => 0,
        };
        // The main bus first, then each aux bus, packed — the same layout a
        // plugin's input region uses.
        let mut at = 0usize;
        {
            let slices = buffer.as_slice_immutable();
            for src in slices.iter().take(input_channels as usize) {
                let dst = &mut self.input_scratch[at..at + frame_len];
                dst.copy_from_slice(&src[..frame_len]);
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
            loop_range_music: transport.loop_range_beats(),
            loop_range_seconds: transport.loop_range_seconds(),
        };

        self.out_events.clear();
        // The graph decides where the audio goes and which plugins see it.
        // Sub-plugins are reached through `AudioInstances`, so the engine
        // still knows nothing about what a plugin is.
        if runnable {
            run_stages(
                &mut self.engine,
                &mut self.schedule,
                processor,
                &mut self.out_events,
                &time,
                AudioIo {
                    frames,
                    daw_in: &self.input_scratch[..(total_in as u32 * frames).max(1) as usize],
                    daw_out: &mut self.output_scratch[..(out_channels * frames) as usize],
                },
                transport.sample_rate as f64,
                transport.tempo.unwrap_or(120.0),
            );
        }
        // What the editor's meters show. The DAW's own parameter value stops
        // being the answer the moment the graph drives a slot.
        self.shared
            .report_slots(self.schedule.block(self.schedule.blocks() - 1));

        let status = if self.engine.has_audio() {
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
            settle_notes(
                &mut self.engine,
                &self.out_events,
                &mut self.ended_notes,
                context,
            );
            return ProcessStatus::Normal;
        };

        // After the audio half, because that is where a note is handed to a
        // sub-plugin and where the sub-plugin's answer comes back.
        settle_notes(
            &mut self.engine,
            &self.out_events,
            &mut self.ended_notes,
            context,
        );

        if status == ApiStatus::Error {
            // Silence rather than noise, and rather than a bypass nothing on
            // the canvas asked for.
            for ch in 0..channels as usize {
                buffer.as_slice()[ch][..frame_len].fill(0.0);
            }
            return ProcessStatus::Normal;
        }

        let output = buffer.as_slice();
        for (ch, out) in output.iter_mut().take(out_channels as usize).enumerate() {
            out[..frame_len]
                .copy_from_slice(&self.output_scratch[ch * frame_len..(ch + 1) * frame_len]);
        }
        // Channels the sub-plugin did not write must not keep the input.
        for out in &mut output[out_channels as usize..channels as usize] {
            out[..frame_len].fill(0.0);
        }

        match status {
            // The sub-plugin may have a tail we cannot measure, and an
            // instrument keeps sounding after its input stops.
            ApiStatus::Silent => ProcessStatus::Normal,
            _ => ProcessStatus::KeepAlive,
        }
    }

    /// Latency the sub-plugin asked to change since the last check.
    pub fn take_latency_change(&self) -> Option<u32> {
        self.context.take_latency_change()
    }
}

/// Tell the DAW about the notes the graph has finished with.
///
/// A sub-plugin's `NOTE_END` says it is done with a note; a note every
/// sub-plugin is done with — or that reached none of them, because every branch
/// was gated shut — is one the DAW can stop holding a voice for. Saying so is
/// the honest answer either way, and CLAP asks for it.
///
/// VST3 has no `NOTE_END` to send back, so nothing arrives from that side; its
/// backend ends the note when the note-off is delivered instead, which is the
/// closest the format allows.
///
/// A free function rather than a method because the caller is holding a borrow
/// of the shared audio state for the whole block.
fn settle_notes<P: Plugin>(
    engine: &mut Engine,
    from_plugins: &EventSink,
    ended: &mut Vec<Ended>,
    context: &mut impl ProcessContext<P>,
) {
    ended.clear();
    engine.end_block(from_plugins.events(), ended);
    for note in ended.iter() {
        context.send_event(NoteEvent::VoiceTerminated {
            timing: 0,
            voice_id: note.daw_id,
            channel: note.channel.clamp(0, 15) as u8,
            note: note.key.clamp(0, 127) as u8,
        });
    }
}

/// Fill the sub-block schedule with host automation and graph evaluation outputs.
///
/// Note events are folded into the engine as the sub-block boundaries pass
/// them, so a modulator reading pressure sees the value that was current at
/// that point in the block rather than the one the block ended on.
///
/// A free function rather than a method because it needs three fields of
/// `Wrapper` mutably at once while the audio lock is held, and spelling the
/// borrows out is clearer than arguing with the compiler about them.
#[allow(clippy::too_many_arguments)]
/// Sets the block's lane grid up. Returns false when there is nothing to run.
fn begin_graph(
    engine: &mut Engine,
    schedule: &mut SlotSchedule,
    daw_slots: &[f64],
    events: &[Event],
    frames: u32,
    quantum: u32,
) -> bool {
    if schedule.quantum() != quantum {
        // Allocation-free by construction; see `SlotSchedule`.
        schedule.set_quantum(quantum);
    }
    let blocks = schedule.begin(frames);

    if !engine.has_program() {
        // Nothing to evaluate. One value per slot for the whole block is
        // exactly the shape the wrapper produced before it had a graph, so a
        // project with no graph behaves identically — including sending no more
        // events.
        schedule.fill(daw_slots);
        return false;
    }

    // The DAW's automation fills the slot lanes; the rest are the graph's own
    // parameter lanes and start from nothing. Zeroing rather than leaving the
    // previous sub-block's values means a lane the graph stops driving stops
    // sending, instead of repeating a stale value. Done for every row up
    // front, because the stages below write into these same rows and a second
    // pass of zeroing would wipe what the stage before it left.
    for index in 0..blocks {
        let values = schedule.block_mut(index);
        let slots = daw_slots.len().min(values.len());
        values[..slots].copy_from_slice(&daw_slots[..slots]);
        values[slots..].fill(0.0);
    }

    // The whole block's stream goes in once, before anything runs: every
    // note gets an id of the graph's own here, and every stage has to agree
    // about which note is which. From here the events flow along the graph's
    // own wires, and the only thing a row needs is where its sub-block sits.
    engine.begin_block(events);
    true
}

/// One DAW block: every stage, in order, over every sub-block.
///
/// A stage's parameters run for the whole block before its audio does, and its
/// audio before the next stage's parameters — which is what lets a parameter
/// be read off audio at all. Only the stage holding a feedback loop steps
/// sub-block by sub-block; the rest are called once.
///
/// The sub-plugins are bound to the schedule *inside* the loop rather than
/// once around it. The parameter half writes the lane rows and the adapter
/// reads them, so the two borrows have to take turns; binding per stage is
/// what keeps them from overlapping, and costs a few reference copies.
#[allow(clippy::too_many_arguments)]
fn run_stages(
    engine: &mut Engine,
    schedule: &mut SlotSchedule,
    processor: Option<&mut subhost_adapter::SubHostProcessors>,
    out_events: &mut EventSink,
    time: &TimeContext,
    audio: AudioIo<'_>,
    sample_rate: f64,
    tempo_bpm: f64,
) {
    let AudioIo {
        frames,
        daw_in,
        daw_out,
    } = audio;
    let blocks = schedule.blocks();
    let quantum = schedule.quantum();
    engine.clear_output(daw_out);

    let mut processor = processor;
    for stage in 0..engine.stages() {
        for index in 0..blocks {
            let context = BlockContext {
                sample_rate,
                tempo_bpm,
                frames: schedule.frames_of(index),
                offset: schedule.offset(index),
                row: index as u32,
                block: frames,
            };
            engine.run_stage(stage, &context, schedule.block_mut(index));
        }

        let mut loaded;
        let mut empty = subhost_adapter::NoInstances;
        let nodes: &mut dyn subhost_adapter::AudioInstances = match processor.as_deref_mut() {
            Some(processor) => {
                loaded = processor.bind(&*schedule, time, out_events);
                &mut loaded
            }
            None => &mut empty,
        };
        engine.run_audio_stage(
            stage,
            &audio_graph_engine::AudioContext {
                frames,
                quantum,
                sample_rate,
                // The same buffer the parameter lanes ride in. The audio half
                // reads only its own range of lane numbers out of it.
                lanes: schedule.rows(),
                lanes_per_row: LANES,
            },
            daw_in,
            daw_out,
            nodes,
        );
    }
}

/// What one block of audio is, as far as [`run_stages`] is concerned.
struct AudioIo<'a> {
    frames: u32,
    daw_in: &'a [f32],
    daw_out: &'a mut [f32],
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

/// Convert nice-plug note events into plugin_host note and expression events.
///
/// Forwards note-on, note-off, voice termination, and per-note expressions (pressure,
/// volume, pan, tuning, vibrato, brightness) to the engine.
/// The host's `voice_id` is passed through as-is, including its absence.
/// Substituting the key number where the host supplies none would make an
/// invented id indistinguishable from one the host actually numbered, and
/// would collide the moment the same key overlapped itself.
fn convert_note<S>(event: &NoteEvent<S>) -> Option<ApiNote> {
    use plugin_host::NoteExpression as Expr;

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
            note_id: voice_id,
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
            note_id: voice_id,
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
            note_id: voice_id,
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
            note_id: voice_id,
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
        NoteEvent::MidiCC {
            timing,
            channel,
            cc,
            value,
        } => ApiNote::Cc {
            port: 0,
            channel: channel as i16,
            cc,
            value: value as f64,
            sample_offset: timing,
        },
        // nice-plug normalizes bend to `0..=1` with 0.5 at rest; the core model
        // is signed, so centre lands on an exact zero either way.
        NoteEvent::MidiPitchBend {
            timing,
            channel,
            value,
        } => ApiNote::PitchBend {
            port: 0,
            channel: channel as i16,
            value: value as f64 * 2.0 - 1.0,
            sample_offset: timing,
        },
        NoteEvent::MidiChannelPressure {
            timing,
            channel,
            pressure,
        } => ApiNote::ChannelPressure {
            port: 0,
            channel: channel as i16,
            value: pressure as f64,
            sample_offset: timing,
        },
        NoteEvent::MidiProgramChange {
            timing,
            channel,
            program,
        } => ApiNote::Midi {
            port: 0,
            data: [0xc0 | (channel & 0x0f), program & 0x7f, 0],
            sample_offset: timing,
        },
        // SysEx is generic over the plugin's own message type, which we do not
        // define, so there is nothing here to forward it as.
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CC, bend and channel pressure reaching the graph is the whole point of
    /// asking the host for `MidiConfig::MidiCCs`. Dropping them here instead
    /// would leave nothing downstream able to observe the difference.
    #[test]
    fn controllers_convert_instead_of_being_dropped() {
        let cc = convert_note(&NoteEvent::<()>::MidiCC {
            timing: 3,
            channel: 1,
            cc: 64,
            value: 1.0,
        });
        assert!(matches!(
            cc,
            Some(ApiNote::Cc {
                channel: 1,
                cc: 64,
                value,
                sample_offset: 3,
                ..
            }) if value == 1.0
        ));

        // nice-plug centres bend at 0.5; the core model centres it at zero.
        let bend = convert_note(&NoteEvent::<()>::MidiPitchBend {
            timing: 0,
            channel: 0,
            value: 0.5,
        });
        assert!(matches!(bend, Some(ApiNote::PitchBend { value, .. }) if value == 0.0));

        let pressure = convert_note(&NoteEvent::<()>::MidiChannelPressure {
            timing: 0,
            channel: 0,
            pressure: 0.25,
        });
        assert!(matches!(
            pressure,
            Some(ApiNote::ChannelPressure { value, .. }) if value == 0.25
        ));
    }

    /// The host's answer about voice identity is passed through, absence
    /// included. Substituting the key number here would make an invented id
    /// indistinguishable from a real one.
    #[test]
    fn a_missing_voice_id_stays_missing() {
        let on = convert_note(&NoteEvent::<()>::NoteOn {
            timing: 0,
            voice_id: None,
            channel: 0,
            note: 60,
            velocity: 1.0,
        });
        assert!(matches!(on, Some(ApiNote::NoteOn { note_id: None, .. })));
    }
    use audio_graph_engine::{
        Graph, Lfo, Math, MathOp, NodeKind, ParamPort, Plugin, PluginPorts, Rate, Waveform, compile,
    };

    /// Helper to create a sub-plugin node with a parameter port for testing.
    ///
    /// The parameter lane is mapped immediately following the host slot table (`SINK_LANE`).
    fn param_sink(graph: &mut Graph) -> audio_graph_engine::NodeId {
        graph.add(
            NodeKind::Plugin(Plugin {
                instance: 0,
                ports: PluginPorts {
                    params: vec![ParamPort {
                        id: 0,
                        name: "p".into(),
                    }],
                    ..PluginPorts::default()
                },
            }),
            [200.0, 0.0],
        )
    }

    /// The lane `param_sink`'s parameter is driven through.
    const SINK_LANE: usize = SLOT_COUNT;

    /// The parameter side of one block, driven the way `process` drives it.
    ///
    /// No audio between the stages, which is right for a graph that has none.
    fn fill_lanes(engine: &mut Engine, schedule: &mut SlotSchedule, daw_slots: &[f64]) {
        if !begin_graph(engine, schedule, daw_slots, &[], 128, 32) {
            return;
        }
        for stage in 0..engine.stages() {
            for index in 0..schedule.blocks() {
                let context = BlockContext {
                    sample_rate: 48_000.0,
                    tempo_bpm: 120.0,
                    frames: schedule.frames_of(index),
                    offset: schedule.offset(index),
                    row: index as u32,
                    block: 128,
                };
                engine.run_stage(stage, &context, schedule.block_mut(index));
            }
        }
    }

    /// Every sub-block is filled to the schedule's width, whether or not a
    /// graph is loaded.
    #[test]
    fn a_block_is_filled_to_the_schedules_width_whether_or_not_a_graph_runs() {
        let daw_slots = vec![0.42; SLOT_COUNT];
        let mut engine = Engine::new();
        let mut schedule = SlotSchedule::new(LANES, 512, 32);

        // No program: every sub-block is the DAW's values, and the graph's own
        // lanes are quiet.
        fill_lanes(&mut engine, &mut schedule, &daw_slots);
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

        // With one: same width, and the graph's own lane now carries a value.
        let mut graph = Graph::new();
        let lfo = graph.add(
            NodeKind::Lfo(Lfo {
                waveform: Waveform::Saw,
                rate: Rate::Hz(4.0),
                phase: 0.0,
                depth: 0.5,
                offset: 0.5,
            }),
            [0.0, 0.0],
        );
        let scale = graph.add(
            NodeKind::Math(Math {
                op: MathOp::Multiply,
                b: 1.0,
            }),
            [100.0, 0.0],
        );
        let out = param_sink(&mut graph);
        graph.connect(lfo, 0, scale, 0);
        graph.connect(scale, 0, out, 0);

        let handoff = audio_graph_engine::Handoff::new();
        handoff.send(Box::new(compile(&graph, SLOT_COUNT).unwrap()));
        assert!(engine.adopt(&handoff));

        fill_lanes(&mut engine, &mut schedule, &daw_slots);
        for index in 0..schedule.blocks() {
            let block = schedule.block(index);
            assert_eq!(block.len(), LANES);
            assert_eq!(block[0], 0.42, "an undriven slot stays the DAW's");
            assert!(
                block[SINK_LANE] != 0.0 || index == 0,
                "the graph's own lane carries what it drives"
            );
        }
    }
}
