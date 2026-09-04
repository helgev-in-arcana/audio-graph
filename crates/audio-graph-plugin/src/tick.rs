//! The wrapper's background heartbeat: provides periodic calls onto the host's main thread.
//!
//! Hosted plugins may request periodic timer ticks or schedule main-thread callbacks
//! (e.g. for parameter rescanning, lazy state serialization, or GUI resizing). This
//! ticking mechanism runs continuously throughout the lifetime of the plugin instance,
//! ensuring sub-plugins are serviced even when the editor window is closed.
//!
//! # Thread dispatch
//!
//! A dedicated background thread handles timing intervals and posts tasks to the main
//! thread via nice-plug's `execute_gui` (which maps to `request_callback()` under CLAP
//! or the main message loop under VST3).
//!
//! # Timing intervals
//!
//! When sub-plugins are loaded, ticks run at [`BUSY_MS`] (60 Hz). When no sub-plugins
//! are active, the interval relaxes to [`IDLE_MS`] to minimize host UI thread overhead.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crate::host_context::WrapperHostContext;
use crate::shared::Shared;

/// The period while at least one sub-plugin is loaded, in milliseconds.
///
/// Only bookkeeping happens per tick — the DAW pumps the actual messages — so
/// 60 Hz is generous. `clap-host` floors a plugin's own requested timers at
/// 8 ms, so this is what those effectively resolve to.
const BUSY_MS: u32 = 16;

/// The period while nothing is loaded, in milliseconds.
///
/// An empty wrapper has nobody to tick, and waking the host's UI thread sixty
/// times a second to discover that is exactly the resident cost worth avoiding.
const IDLE_MS: u32 = 200;

/// The only background task this plugin has.
#[derive(Clone, Copy)]
pub enum Task {
    /// Drive every loaded sub-plugin for one main-thread tick.
    Tick,
}

/// What the waiting thread and the main-thread half say to each other.
pub struct TickState {
    /// Set when a tick has been posted and not yet run. Stops ticks piling up
    /// behind a main thread that is busy — a queue of them would all run back
    /// to back and none of them would mean anything.
    pending: AtomicBool,
    period_ms: AtomicU32,
    stop: Mutex<bool>,
    wake: Condvar,
}

impl TickState {
    pub fn new() -> Arc<TickState> {
        Arc::new(TickState {
            pending: AtomicBool::new(false),
            period_ms: AtomicU32::new(IDLE_MS),
            stop: Mutex::new(false),
            wake: Condvar::new(),
        })
    }
}

/// Run one tick. Call only on the host's main thread.
///
/// Skips rather than waits if the state is already borrowed further up the
/// stack: this runs from the host's callback, and the callback can arrive
/// while a command that is loading a plugin is dispatching messages.
pub fn run(shared: &Arc<Shared>, context: &Arc<WrapperHostContext>, state: &TickState) {
    state.pending.store(false, Ordering::Release);

    // Before the thread check, because it does not need a thread: freeing a
    // superseded program the audio thread handed back is a swap and a drop.
    // Everything below is main-thread-only, and on one host — VST3 under Linux
    // with our editor closed — that check never passes, so anything left below
    // it would grow without bound for the life of the instance.
    shared.reclaim();

    if !shared.on_main_thread() {
        // nice-plug runs GUI callbacks on a worker thread when it has no run
        // loop to post to, which on Linux is VST3 with our editor closed. Said
        // once rather than every tick: it is a property of the host, so it will
        // not change, and the reason to say it at all is that a tick which
        // silently does nothing for a whole session is indistinguishable from
        // one that ran and found nothing to do.
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            log::warn!(
                "audio-graph: the host runs its main-thread callbacks on a worker thread; \
                 sub-plugin timers and callbacks will not run while this editor is closed"
            );
        });
        return;
    }

    // Our own windows, where their event source is ours to turn — X11. Before
    // anything else, so a close the user asked for this frame is already
    // recorded by the time the editors are ticked.
    plugin_host::poll();

    // Whatever the editor asked for and could not do itself: every button in
    // the editor arrives here, on whichever thread it was pressed from.
    shared.run_posted();

    let busy = {
        let Some(mut main) = shared.try_main() else {
            return;
        };
        main.host.tick_editors();
        main.host.any_loaded()
    };

    // After the editors, because a plugin says its latency has moved from its
    // own main-thread callback and that is what turns one. Recompiling is what
    // makes the new number the graph's; `Wrapper::process` is what tells the
    // DAW, being the only place the wrapper is handed something that can.
    if context.take_latency_change() && shared.refresh_latencies() {
        shared.publish_graph();
    }

    // Last, so the snapshot the next frame draws includes everything this tick
    // did.
    shared.publish_view();

    // An open editor needs the full rate whether or not anything is loaded: it
    // is the only thing turning our event loop, and off the main thread it is
    // also the only thing carrying the user's clicks across.
    let busy = busy || shared.editor_open();
    state
        .period_ms
        .store(if busy { BUSY_MS } else { IDLE_MS }, Ordering::Relaxed);
}

/// The thread that does the waiting.
///
/// Owned by the plugin instance, so it lives exactly as long as the instance
/// does — which is the whole point of moving it off the editor.
pub struct Ticker {
    state: Arc<TickState>,
    join: Option<JoinHandle<()>>,
}

impl Ticker {
    /// Start ticking. `post` must put [`Task::Tick`] onto the host's main
    /// thread; it is called from the ticking thread, never from this one.
    pub fn spawn(state: Arc<TickState>, post: impl Fn() + Send + 'static) -> Ticker {
        let thread_state = state.clone();
        let join = std::thread::Builder::new()
            .name("audio-graph tick".into())
            .spawn(move || {
                let state = thread_state;
                loop {
                    let period =
                        Duration::from_millis(state.period_ms.load(Ordering::Relaxed) as u64);
                    let mut stop = state.stop.lock();
                    if !*stop {
                        state.wake.wait_for(&mut stop, period);
                    }
                    if *stop {
                        return;
                    }
                    drop(stop);

                    // Already one in flight: the main thread is busy, and a
                    // second tick would only queue behind the first.
                    if !state.pending.swap(true, Ordering::AcqRel) {
                        post();
                    }
                }
            });
        match join {
            Ok(join) => Ticker {
                state,
                join: Some(join),
            },
            Err(e) => {
                log::warn!("audio-graph: could not start the tick thread: {e}");
                Ticker { state, join: None }
            }
        }
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        *self.state.stop.lock() = true;
        self.state.wake.notify_all();
        if let Some(join) = self.join.take() {
            // Joined rather than detached: the thread posts work that reaches
            // this instance, and letting it outlive the instance is how you
            // get a tick against a half-destroyed plugin.
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wait until `posts` reaches `want`, or give up.
    ///
    /// Deliberately not "sleep for N ms and count": the tick rate is a request,
    /// not a contract — the host's callback decides the real interval — and a
    /// count-in-a-window assertion measures the runner's scheduler more than it
    /// measures this module. CI hosts are slow and uneven (the macOS runners
    /// coalesce short timers, and every runner is oversubscribed by
    /// `cargo test`'s own parallelism), so we poll and let a slow machine take
    /// as long as it needs.
    fn wait_for_posts(posts: &AtomicU32, want: u32) -> u32 {
        // Generous enough that only a genuinely stuck ticker reaches it.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let seen = posts.load(Ordering::Relaxed);
            if seen >= want || std::time::Instant::now() >= deadline {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The thing the whole module exists for: it keeps posting, on its own,
    /// with nobody's window open.
    #[test]
    fn it_keeps_posting_until_dropped() {
        let state = TickState::new();
        state.period_ms.store(10, Ordering::Relaxed);

        let posts = Arc::new(AtomicU32::new(0));
        let counter = posts.clone();
        // Stands in for the main thread running the task: without clearing
        // `pending`, exactly one post would ever be made.
        let ran = state.clone();
        let ticker = Ticker::spawn(state, move || {
            counter.fetch_add(1, Ordering::Relaxed);
            ran.pending.store(false, Ordering::Release);
        });

        let posted = wait_for_posts(&posts, 5);
        assert!(posted >= 5, "the ticker stalled after {posted} ticks");

        // And dropping it stops them, rather than leaving a thread posting at
        // an instance that is going away. Read the count after the drop has
        // joined the thread: a tick already in flight when we asked it to stop
        // is allowed to land, and would otherwise race this.
        drop(ticker);
        let after = posts.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(posts.load(Ordering::Relaxed), after);
    }

    /// A main thread that never gets round to the task must not accumulate a
    /// queue of ticks that all fire back to back once it does.
    #[test]
    fn a_tick_nobody_ran_is_not_posted_twice() {
        let state = TickState::new();
        state.period_ms.store(5, Ordering::Relaxed);

        let posts = Arc::new(AtomicU32::new(0));
        let counter = posts.clone();
        let ticker = Ticker::spawn(state, move || {
            counter.fetch_add(1, Ordering::Relaxed);
        });

        // Nothing ever clears `pending`, so once the first tick is posted the
        // count must stay at one however long the ticker runs.
        assert_eq!(wait_for_posts(&posts, 1), 1, "the first tick never posted");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(posts.load(Ordering::Relaxed), 1);
        drop(ticker);
        assert_eq!(posts.load(Ordering::Relaxed), 1);
    }
}
