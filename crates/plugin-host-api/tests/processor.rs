use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use plugin_host_api::{
    AudioBuffers, BufferLayout, Event, EventSink, ProcessStatus, Processor, SubPluginProcessor,
    TimeContext, reclaim_main_thread,
};

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    static DEALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

struct Allocator;

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.try_with(Cell::get).unwrap_or(false) {
            let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        }
        // SAFETY: the allocation request is passed unchanged to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if COUNTING.try_with(Cell::get).unwrap_or(false) {
            let _ = DEALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        }
        // SAFETY: pointer and layout are the original allocation's pair.
        unsafe { System.dealloc(pointer, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

struct Counter {
    calls: usize,
    dropped: Arc<AtomicUsize>,
    _scratch: Vec<u8>,
}

impl SubPluginProcessor for Counter {
    fn process(
        &mut self,
        _: &mut AudioBuffers<'_>,
        _: &[Event],
        _: &TimeContext,
        _: &mut EventSink,
    ) -> ProcessStatus {
        self.calls += 1;
        ProcessStatus::Continue
    }

    fn reset(&mut self) {
        self.calls = 0;
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// Processing and returning a handle allocate nothing, and audio never frees its payload.
#[test]
fn processing_and_foreign_return_do_not_touch_the_allocator() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let mut processor = Processor::new(Counter {
        calls: 0,
        dropped: dropped.clone(),
        _scratch: vec![0; 4096],
    });
    std::thread::spawn(move || {
        let _ = std::thread::current().id();
        let mut output = [0.0; 32];
        let mut buffers = AudioBuffers::new(&[], &mut output, 0, 1, 32, BufferLayout::Planar);
        let mut sink = EventSink::new();
        let time = TimeContext::default();
        ALLOCATIONS.set(0);
        DEALLOCATIONS.set(0);
        COUNTING.set(true);
        for _ in 0..1000 {
            processor.process(&mut buffers, &[], &time, &mut sink);
        }
        processor.reset();
        processor.deactivate();
        COUNTING.set(false);
        assert_eq!(ALLOCATIONS.get(), 0);
        assert_eq!(DEALLOCATIONS.get(), 0);
    })
    .join()
    .unwrap();
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
    reclaim_main_thread();
    assert_eq!(dropped.load(Ordering::Relaxed), 1);
}
