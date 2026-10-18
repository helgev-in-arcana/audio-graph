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
