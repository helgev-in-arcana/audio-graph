use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, ThreadId};

use super::{BlockContext, Engine, ParamEvalContext};
use crate::ir::{CustomParam, Operand, ParamBindings, ParamKernel, Program};
use crate::nodes::{KeyParam, KeyParamMode, ParamPort, Plugin, PluginPorts, SlotIn};
use crate::{Graph, NodeKind, ProgramPublisher, compile};
use plugin_host::{Event, NoteEvent};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Allocations {
    allocated: usize,
    freed: usize,
}

thread_local! {
    static COUNTS: Cell<Option<Allocations>> = const { Cell::new(None) };
}

struct CountingAllocator;

fn record(allocated: usize, freed: usize) {
    let _ = COUNTS.try_with(|counts| {
        if let Some(mut current) = counts.get() {
            current.allocated += allocated;
            current.freed += freed;
            counts.set(Some(current));
        }
    });
}

// SAFETY: every request is forwarded with its original pointer and layout to
// System; counting uses only a const-initialized thread-local Cell.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(1, 0);
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(1, 0);
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        record(0, 1);
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(1, 1);
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn measured<T>(run: impl FnOnce() -> T) -> (T, Allocations) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            COUNTS.with(|counts| counts.set(None));
        }
    }
    COUNTS.with(|counts| {
        assert!(counts.get().is_none());
        counts.set(Some(Allocations::default()));
    });
    let reset = Reset;
    let result = run();
    let counts = COUNTS.with(|counts| counts.get().unwrap());
    drop(reset);
    (result, counts)
}

/// Both allocation and destruction are visible inside the measured scope.
#[test]
fn allocation_counting_observes_both_sides_of_an_owned_value() {
    let (_, counts) = measured(|| {
        let value = Box::new([0u8; 32]);
        drop(std::hint::black_box(value));
    });
    assert_eq!(counts.allocated, 1);
    assert_eq!(counts.freed, 1);
}

fn program() -> Program {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0; 2]);
    let key = graph.add(
        NodeKind::KeyParam(KeyParam {
            mode: KeyParamMode::Toggle,
            keys: vec![24, 25, 26],
            values: vec![0.2, 0.8, 0.4],
            mute_keys: true,
        }),
        [0.0; 2],
    );
    graph.connect(notes, 0, key, 0);
    for slot in 0..3 {
        let input = graph.add(NodeKind::SlotIn(SlotIn { slot }), [0.0; 2]);
        graph.connect(input, 0, key, slot as u8 + 1);
    }
    let sink = graph.add(
        NodeKind::Plugin(Plugin {
            instance: 0,
            ports: PluginPorts {
                params: vec![ParamPort {
                    id: 0,
                    name: String::new(),
                }],
                ..Default::default()
            },
        }),
        [0.0; 2],
    );
    graph.connect(key, 0, sink, 0);
    compile(&graph, 3).unwrap()
}

fn context() -> BlockContext {
    BlockContext {
        sample_rate: 48_000.0,
        tempo_bpm: 120.0,
        frames: 8,
        offset: 0,
        row: 0,
        block: 8,
    }
}

#[derive(Debug)]
struct Pair;

impl ParamKernel for Pair {
    fn eval(&self, cx: &mut ParamEvalContext<'_>) {
        assert_eq!(cx.inputs.len(), 3);
        assert_eq!(cx.outputs.len(), 2);
        assert!(cx.latch.is_none());
        assert!(cx.notes.is_none());
        cx.outputs[0] = cx.inputs.read(0) + cx.inputs.read(1);
        cx.outputs[1] = cx.inputs.read(2) - cx.inputs.read(1);
    }
}

/// A kernel receives local input order, immediate operands and its complete
/// output slice without access to unrelated registers or node state.
#[test]
fn a_custom_kernel_reads_scattered_inputs_and_writes_multiple_outputs() {
    let mut program = program();
    let custom = &mut program.custom_params[0];
    let [Operand::Reg(first), _, Operand::Reg(third)] = *custom.bindings.inputs else {
        panic!("the three value inputs are wired")
    };
    let start = custom.bindings.outputs.start;
    assert_eq!(custom.bindings.outputs.end as usize, program.registers);
    custom.bindings = ParamBindings {
        inputs: Box::new([
            Operand::Reg(third),
            Operand::Value(0.25),
            Operand::Reg(first),
        ]),
        outputs: start..start + 2,
        latch: None,
        notes: None,
    };
    custom.kernel = Box::new(Pair);
    program.registers += 1;

    let publisher = ProgramPublisher::default();
    publisher.publish(program, 48_000.0);
    let mut engine = Engine::new();
    assert!(engine.adopt(&publisher));
    let mut slots = [0.375, 0.99, 0.5, 0.0];
    engine.run(&context(), &mut slots);
    assert_eq!(slots[3], 0.75);
    assert_eq!(&engine.registers()[start as usize..], &[0.75, 0.125]);
    assert_eq!(engine.registers()[0..start as usize], [0.375, 0.99, 0.5]);
}

#[derive(Debug)]
struct DropRecord {
    owner: ThreadId,
    count: AtomicUsize,
    wrong_thread: AtomicBool,
}

impl DropRecord {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            owner: thread::current().id(),
            count: AtomicUsize::new(0),
            wrong_thread: AtomicBool::new(false),
        })
    }

    fn assert_drops(&self, expected: usize) {
        assert_eq!(self.count.load(Ordering::Relaxed), expected);
        assert!(!self.wrong_thread.load(Ordering::Relaxed));
    }
}

#[derive(Debug)]
struct TrackedKernel {
    inner: Box<dyn ParamKernel>,
    record: Arc<DropRecord>,
}

impl ParamKernel for TrackedKernel {
    fn eval(&self, cx: &mut ParamEvalContext<'_>) {
        self.inner.eval(cx);
    }
}

impl Drop for TrackedKernel {
    fn drop(&mut self) {
        if thread::current().id() != self.record.owner {
            self.record.wrong_thread.store(true, Ordering::Relaxed);
        }
        self.record.count.fetch_add(1, Ordering::Relaxed);
    }
}

fn tracked_program(record: &Arc<DropRecord>) -> Program {
    let mut program = program();
    let custom = program.custom_params.pop().unwrap();
    program.custom_params.push(CustomParam {
        kernel: Box::new(TrackedKernel {
            inner: custom.kernel,
            record: Arc::clone(record),
        }),
        ..custom
    });
    program
}

/// Pending replacement, reclamation and release destroy kernels on the
/// publishing thread; prepared adoption, evaluation and peek allocate nothing.
#[test]
fn custom_kernels_follow_program_ownership_without_audio_thread_allocations() {
    let records = [DropRecord::new(), DropRecord::new(), DropRecord::new()];
    let publisher = ProgramPublisher::default();
    publisher.publish(tracked_program(&records[0]), 48_000.0);
    publisher.publish(tracked_program(&records[1]), 48_000.0);
    records[0].assert_drops(1);
    records[1].assert_drops(0);

    let engine = Engine::new();
    let mut engine = thread::scope(|scope| {
        scope
            .spawn(|| {
                let mut engine = engine;
                let (adopted, adoption) = measured(|| engine.adopt(&publisher));
                assert!(adopted);
                assert_eq!(adoption, Allocations::default());
                let mut slots = [0.2, 0.8, 0.4, 0.0];
                let events = [Event::Note(NoteEvent::NoteOn {
                    note_id: None,
                    port: 0,
                    channel: 0,
                    key: 24,
                    velocity: 1.0,
                    sample_offset: 7,
                })];
                let (_, evaluation) = measured(|| {
                    engine.begin_block(&events);
                    engine.run(&context(), &mut slots);
                });
                assert_eq!(evaluation, Allocations::default());
                assert_eq!(slots[3], 0.2);
                let (_, peek) = measured(|| {
                    engine.run_stage_end(
                        0,
                        &BlockContext {
                            frames: 0,
                            offset: 8,
                            row: 1,
                            ..context()
                        },
                        &mut slots,
                    );
                });
                assert_eq!(peek, Allocations::default());
                assert_eq!(slots[3], 0.8);
                engine
            })
            .join()
            .unwrap()
    });
    records[1].assert_drops(0);

    publisher.publish(tracked_program(&records[2]), 48_000.0);
    thread::scope(|scope| {
        scope
            .spawn(|| {
                let (adopted, adoption) = measured(|| engine.adopt(&publisher));
                assert!(adopted);
                assert_eq!(adoption, Allocations::default());
            })
            .join()
            .unwrap();
    });
    records[1].assert_drops(0);
    publisher.reclaim();
    records[1].assert_drops(1);
    records[2].assert_drops(0);

    let (released, release) = measured(|| engine.release());
    assert_eq!(release, Allocations::default());
    records[2].assert_drops(0);
    drop(released);
    records[2].assert_drops(1);
    assert_eq!(engine.publication(), 0);
}

fn audio_note_program() -> Program {
    audio_note_program_with_pairs(1)
}

fn audio_note_program_with_pairs(pairs: usize) -> Program {
    use crate::nodes::{
        AudioIn, AudioOut, AudioTremolo, Beats, KeySwitchMode, MidiDelay, Ratio, TremoloKeys,
        TremoloRow,
    };

    let mut graph = Graph::new();
    let mut audio = graph.add(
        NodeKind::AudioIn(AudioIn {
            bus: 0,
            channels: 2,
        }),
        [0.0; 2],
    );
    let mut notes = graph.add(NodeKind::NoteIn, [0.0; 2]);
    for _ in 0..pairs {
        let delay = graph.add(
            NodeKind::MidiDelay(MidiDelay {
                seconds: 0.001,
                beats: Beats::new(1, 64),
                sync: false,
            }),
            [0.0; 2],
        );
        let cut = graph.add(
            NodeKind::AudioTremolo(AudioTremolo {
                channels: 2,
                keys: TremoloKeys {
                    mode: KeySwitchMode::Select,
                    rows: vec![TremoloRow {
                        key: 24,
                        step: Beats::new(1, 32),
                    }],
                    stop_key: 23,
                    share: Ratio::EVEN,
                },
                fade_in_ms: 2.0,
                fade_out_ms: 3.0,
            }),
            [0.0; 2],
        );
        graph.connect(audio, 0, cut, 0);
        graph.connect(notes, 0, delay, 0);
        graph.connect(delay, 0, cut, 1);
        audio = cut;
        notes = delay;
    }
    let out = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0; 2],
    );
    graph.connect(audio, 0, out, 0);
    compile(&graph, 0).unwrap()
}

struct AudioBlock {
    schedule: super::SlotSchedule,
    output: [f32; 128],
    ended: Vec<crate::notes::Ended>,
}

impl AudioBlock {
    fn new() -> Self {
        Self {
            schedule: super::SlotSchedule::new(
                crate::ir::MAX_GRAPH_PARAMS + crate::ir::MAX_AUDIO_LANES,
                64,
                16,
            )
            .unwrap(),
            output: [0.0; 128],
            ended: Vec::with_capacity(crate::notes::MAX_LIVE_NOTES),
        }
    }

    fn run(&mut self, engine: &mut Engine, events: &[Event]) {
        engine.run_block(
            &mut self.schedule,
            &[],
            events,
            64,
            super::Granularity {
                resolution: 16,
                quantum: 32,
            },
            48_000.0,
            120.0,
            &[1.0; 128],
            &mut self.output,
            &mut subhost_adapter::NoInstances,
        );
        engine.end_block([], &mut self.ended);
    }
}

/// Prepared note and audio kernels allocate and free nothing during adoption, evaluation, reset or removal.
#[test]
fn audio_and_note_custom_nodes_keep_the_audio_thread_allocation_free() {
    let publisher = ProgramPublisher::default();
    let program = audio_note_program();
    assert_eq!(program.custom_audio.len(), 1);
    assert_eq!(program.custom_notes.len(), 1);
    publisher.publish(program, 48_000.0);
    let mut engine = Engine::new();
    engine.prepare(64, &[2]);
    let mut block = AudioBlock::new();
    let events = [Event::Note(NoteEvent::NoteOn {
        note_id: None,
        port: 0,
        channel: 0,
        key: 24,
        velocity: 1.0,
        sample_offset: 47,
    })];
    let (_, counts) = measured(|| {
        assert!(engine.adopt(&publisher));
        block.run(&mut engine, &events);
        block.run(&mut engine, &[]);
        engine.reset_notes(&mut block.ended);
        engine.reset();
        engine.reset_everything(&mut block.ended);
    });
    assert_eq!(counts, Allocations::default());
    publisher.publish(audio_note_program(), 48_000.0);
    let (_, counts) = measured(|| {
        assert!(engine.adopt(&publisher));
        block.run(&mut engine, &events);
    });
    assert_eq!(counts, Allocations::default());
    publisher.publish(Program::empty(), 48_000.0);
    let (_, counts) = measured(|| {
        assert!(engine.adopt(&publisher));
        block.run(&mut engine, &[]);
    });
    assert_eq!(counts, Allocations::default());
    publisher.reclaim();
}

#[derive(Debug)]
struct ProbeState {
    processed: usize,
    reset: Option<crate::ir::ResetReason>,
    retained: Box<[u8; 32]>,
    record: Arc<DropRecord>,
}

impl crate::ir::KernelState for ProbeState {
    fn reset(&mut self, reason: crate::ir::ResetReason) {
        self.processed = 0;
        self.reset = Some(reason);
        self.retained.fill(0);
    }
}

impl Drop for ProbeState {
    fn drop(&mut self) {
        if thread::current().id() != self.record.owner {
            self.record.wrong_thread.store(true, Ordering::Relaxed);
        }
        self.record.count.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct ProbeKernel<const KIND: u8> {
    setting: usize,
    record: Arc<DropRecord>,
}

impl<const KIND: u8> ProbeKernel<KIND> {
    fn state(&self) -> ProbeState {
        ProbeState {
            processed: 0,
            reset: None,
            retained: Box::new([0; 32]),
            record: Arc::clone(&self.record),
        }
    }
}

impl<const KIND: u8> Drop for ProbeKernel<KIND> {
    fn drop(&mut self) {
        if thread::current().id() != self.record.owner {
            self.record.wrong_thread.store(true, Ordering::Relaxed);
        }
        self.record.count.fetch_add(1, Ordering::Relaxed);
    }
}

impl<const KIND: u8> crate::ir::AudioKernel for ProbeKernel<KIND> {
    type State = ProbeState;

    fn create_state(&self) -> Self::State {
        self.state()
    }

    fn eval(&self, state: &mut Self::State, cx: &mut super::AudioEvalContext<'_>) {
        use super::audio_context::{AudioIo, AudioSamples};
        fn pass(audio: &mut impl AudioSamples) {
            for channel in 0..audio.channels() {
                for frame in 0..audio.frames() {
                    audio.write(channel, frame, audio.read(channel, frame));
                }
            }
        }
        let frames = match &mut cx.audio {
            AudioIo::InPlace(audio) => {
                pass(audio);
                audio.frames()
            }
            AudioIo::Separate(audio) => {
                pass(audio);
                audio.frames()
            }
        };
        state.processed += frames * self.setting;
    }
}

impl<const KIND: u8> crate::ir::NoteKernel for ProbeKernel<KIND> {
    type State = ProbeState;

    fn create_state(&self) -> Self::State {
        self.state()
    }

    fn eval(&self, state: &mut Self::State, cx: &mut super::NoteEvalContext<'_>) {
        state.processed += cx.frames() as usize * self.setting;
        for &event in cx.input {
            cx.output.push(event);
        }
    }
}

fn probe_program<const KIND: u8>(record: &Arc<DropRecord>, setting: usize) -> Program {
    let mut program = audio_note_program();
    let audio = &mut program.custom_audio[0];
    *audio = crate::ir::CustomAudio::new(
        audio.node,
        ProbeKernel::<KIND> {
            setting,
            record: Arc::clone(record),
        },
    );
    let note = &mut program.custom_notes[0];
    *note = crate::ir::CustomNote::new(
        note.node,
        std::mem::take(&mut note.parameters),
        ProbeKernel::<KIND> {
            setting,
            record: Arc::clone(record),
        },
    );
    for stream in &mut program.note_streams {
        if stream.node == note.node {
            stream.kind = crate::ir::NoteStreamKind::Custom(note.processor.kind());
        }
    }
    program
}

fn assert_probe_states(
    engine: &mut Engine,
    processed: usize,
    reason: Option<crate::ir::ResetReason>,
) {
    for runtime in [
        engine.custom_audio[0].processor.state_mut(),
        engine.notes.custom[0].processor.state_mut(),
    ] {
        let state = runtime.downcast_ref::<ProbeState>().unwrap();
        assert_eq!(state.processed, processed);
        assert_eq!(state.reset, reason);
    }
}

/// Reordering same-kind runtimes preserves each node's state, applies its new settings, and retires boxes off-thread.
#[test]
fn reordered_audio_and_note_kernels_keep_state_by_node_identity() {
    fn program(record: &Arc<DropRecord>, settings: [usize; 4]) -> Program {
        let mut program = audio_note_program_with_pairs(2);
        assert_eq!(program.custom_audio.len(), 2);
        assert_eq!(program.custom_notes.len(), 2);
        for (audio, setting) in program.custom_audio.iter_mut().zip(settings[..2].iter()) {
            *audio = crate::ir::CustomAudio::new(
                audio.node,
                ProbeKernel::<0> {
                    setting: *setting,
                    record: Arc::clone(record),
                },
            );
        }
        for (note, setting) in program.custom_notes.iter_mut().zip(settings[2..].iter()) {
            *note = crate::ir::CustomNote::new(
                note.node,
                std::mem::take(&mut note.parameters),
                ProbeKernel::<0> {
                    setting: *setting,
                    record: Arc::clone(record),
                },
            );
            for stream in &mut program.note_streams {
                if stream.node == note.node {
                    stream.kind = crate::ir::NoteStreamKind::Custom(note.processor.kind());
                }
            }
        }
        program
    }

    fn assert_states<T: crate::ir::RuntimeEntry>(
        entries: &mut [T],
        expected: [(crate::ir::NodeId, usize); 2],
    ) {
        assert_eq!(entries.len(), expected.len());
        for (entry, (node, processed)) in entries.iter_mut().zip(expected) {
            assert_eq!(entry.node(), node);
            let state = entry
                .runtime()
                .state_mut()
                .downcast_ref::<ProbeState>()
                .unwrap();
            assert_eq!(state.processed, processed);
            assert_eq!(state.reset, None);
        }
    }

    let record = DropRecord::new();
    let publisher = ProgramPublisher::default();
    let first = program(&record, [1, 5, 2, 7]);
    let audio_nodes = [first.custom_audio[0].node, first.custom_audio[1].node];
    let note_nodes = [first.custom_notes[0].node, first.custom_notes[1].node];
    publisher.publish(first, 48_000.0);
    let mut engine = Engine::new();
    engine.prepare(64, &[2]);
    let mut block = AudioBlock::new();
    thread::scope(|scope| {
        scope
            .spawn(|| {
                let (_, counts) = measured(|| {
                    assert!(engine.adopt(&publisher));
                    block.run(&mut engine, &[]);
                });
                assert_eq!(counts, Allocations::default());
                assert_states(
                    &mut engine.custom_audio,
                    [(audio_nodes[0], 64), (audio_nodes[1], 320)],
                );
                assert_states(
                    &mut engine.notes.custom,
                    [(note_nodes[0], 128), (note_nodes[1], 448)],
                );
            })
            .join()
            .unwrap();
    });

    let mut next = program(&record, [11, 13, 17, 19]);
    next.custom_audio.reverse();
    next.custom_notes.reverse();
    for op in &mut next.audio_ops {
        if let crate::ir::AudioOp::Custom { index, .. } = op {
            *index = 1 - *index;
        }
    }
    for op in &mut next.note_ops {
        if let crate::ir::NoteOp::Custom { index, .. } = op {
            *index = 1 - *index;
        }
    }
    publisher.publish(next, 48_000.0);
    thread::scope(|scope| {
        scope
            .spawn(|| {
                let (_, counts) = measured(|| {
                    assert!(engine.adopt(&publisher));
                    assert_states(
                        &mut engine.custom_audio,
                        [(audio_nodes[1], 320), (audio_nodes[0], 64)],
                    );
                    assert_states(
                        &mut engine.notes.custom,
                        [(note_nodes[1], 448), (note_nodes[0], 128)],
                    );
                    block.run(&mut engine, &[]);
                });
                assert_eq!(counts, Allocations::default());
                assert_states(
                    &mut engine.custom_audio,
                    [(audio_nodes[1], 1152), (audio_nodes[0], 768)],
                );
                assert_states(
                    &mut engine.notes.custom,
                    [(note_nodes[1], 1664), (note_nodes[0], 1216)],
                );
            })
            .join()
            .unwrap();
    });
    record.assert_drops(0);
    publisher.reclaim();
    record.assert_drops(0);
    let (released, counts) = measured(|| engine.release());
    assert_eq!(counts, Allocations::default());
    drop(released);
    record.assert_drops(8);
    drop(engine);
    record.assert_drops(16);
}

/// Unregistered kernel types keep state only across matching node and kind, take new settings, and die on the publisher thread.
#[test]
fn unknown_note_and_audio_kernels_migrate_reset_and_retire_without_rt_destruction() {
    let record = DropRecord::new();
    let publisher = ProgramPublisher::default();
    publisher.publish(probe_program::<0>(&record, 9), 48_000.0);
    publisher.publish(probe_program::<0>(&record, 1), 48_000.0);
    record.assert_drops(4);
    let mut engine = Engine::new();
    engine.prepare(64, &[2]);
    let mut block = AudioBlock::new();
    thread::scope(|scope| {
        scope
            .spawn(|| {
                let (_, counts) = measured(|| {
                    assert!(engine.adopt(&publisher));
                    block.run(&mut engine, &[]);
                });
                assert_eq!(counts, Allocations::default());
                assert_probe_states(&mut engine, 64, None);
            })
            .join()
            .unwrap()
    });

    publisher.publish(probe_program::<0>(&record, 3), 48_000.0);
    thread::scope(|scope| {
        scope
            .spawn(|| {
                let (_, counts) = measured(|| {
                    assert!(engine.adopt(&publisher));
                    assert_probe_states(&mut engine, 64, None);
                    block.run(&mut engine, &[]);
                });
                assert_eq!(counts, Allocations::default());
                assert_probe_states(&mut engine, 256, None);
                for reason in [
                    crate::ir::ResetReason::Notes,
                    crate::ir::ResetReason::Transport,
                    crate::ir::ResetReason::All,
                ] {
                    let (_, counts) = measured(|| match reason {
                        crate::ir::ResetReason::Notes => engine.reset_notes(&mut block.ended),
                        crate::ir::ResetReason::Transport => engine.reset(),
                        crate::ir::ResetReason::All => engine.reset_everything(&mut block.ended),
                    });
                    assert_eq!(counts, Allocations::default());
                    assert_probe_states(&mut engine, 0, Some(reason));
                    block.run(&mut engine, &[]);
                }
            })
            .join()
            .unwrap()
    });

    publisher.publish(probe_program::<1>(&record, 2), 48_000.0);
    thread::scope(|scope| {
        scope
            .spawn(|| {
                let (_, counts) = measured(|| {
                    assert!(engine.adopt(&publisher));
                    assert_probe_states(&mut engine, 0, None);
                    block.run(&mut engine, &[]);
                });
                assert_eq!(counts, Allocations::default());
                assert_probe_states(&mut engine, 128, None);
            })
            .join()
            .unwrap()
    });
    let mut changed_nodes = probe_program::<1>(&record, 4);
    changed_nodes.custom_audio[0].node += 100;
    changed_nodes.custom_notes[0].node += 100;
    publisher.publish(changed_nodes, 48_000.0);
    thread::scope(|scope| {
        scope
            .spawn(|| {
                let (_, counts) = measured(|| {
                    assert!(engine.adopt(&publisher));
                    assert_probe_states(&mut engine, 0, None);
                    block.run(&mut engine, &[]);
                });
                assert_eq!(counts, Allocations::default());
                assert_probe_states(&mut engine, 256, None);
            })
            .join()
            .unwrap()
    });
    publisher.reclaim();
    let (released, counts) = measured(|| engine.release());
    assert_eq!(counts, Allocations::default());
    drop(released);
    record.assert_drops(16);
    drop(engine);
    record.assert_drops(20);
}
