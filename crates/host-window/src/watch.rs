//! Watching a plugin's timers and file descriptors on its behalf.
//!
//! A plugin cannot wait on anything itself: the host owns the loop. So whatever
//! it needs to be woken for, it hands to the host. Both formats say so, in
//! their own words — VST3 as `Linux::IRunLoop` off the plugin frame, CLAP as
//! `clap.timer-support` and `clap.posix-fd-support`. The words differ; the
//! bookkeeping does not, so it is written once here and each backend keeps only
//! its own vocabulary.
//!
//! [`TimerWheel`] is every platform's; [`FdWatch`] is Linux's alone, because
//! nowhere else does a plugin reach its own event source through a descriptor.
//!
//! Two rules are the reason this is shared rather than copied. Both are the
//! kind that is easy to write correctly once and easy to forget the second
//! time:
//!
//! * **Nothing is called with the list locked.** A plugin is entitled to
//!   register or drop a descriptor from inside its own callback, and that call
//!   comes straight back through here.
//! * **Liveness is re-checked against the live list before every callback**,
//!   not against the copy being walked — an earlier callback in the same turn
//!   may have dropped a later one.

use std::sync::Mutex;
use std::time::{Duration, Instant};

#[cfg(all(unix, not(target_os = "macos")))]
use std::os::fd::RawFd;

/// How many of each one plugin may ask for.
///
/// A ceiling rather than guidance: the lists are walked on every UI tick, and a
/// plugin that registers in a loop and never unregisters would otherwise make
/// the host slower the longer it is open.
const MAX_WATCHED: usize = 16;

/// The shortest period a timer will actually be run at.
///
/// A plugin asking for zero means "as often as you can", and taking that
/// literally would spend the UI thread on one plugin's timer.
const MIN_PERIOD: Duration = Duration::from_millis(8);

/// The longest, so that a period cannot reach `Instant` arithmetic as a number
/// big enough to overflow it. The value comes from the plugin, and a plugin's
/// arithmetic mistake must not be the host's panic.
const MAX_PERIOD: Duration = Duration::from_secs(3600);

/// What a plugin wants to be told about.
#[cfg(all(unix, not(target_os = "macos")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Interest {
    pub read: bool,
    pub write: bool,
}

#[cfg(all(unix, not(target_os = "macos")))]
impl Interest {
    pub const READ: Interest = Interest {
        read: true,
        write: false,
    };
}

/// What actually happened to a descriptor.
#[cfg(all(unix, not(target_os = "macos")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Readiness {
    pub read: bool,
    pub write: bool,
    /// The descriptor errored or the far end hung up. Reported whether or not
    /// it was asked for: a plugin that did not ask to hear about errors is
    /// better told than left waiting on a descriptor that is never coming back.
    pub error: bool,
}

#[cfg(all(unix, not(target_os = "macos")))]
impl Readiness {
    fn is_anything(self) -> bool {
        self.read || self.write || self.error
    }
}

/// Descriptors a plugin has asked the host to watch.
///
/// `K` is whatever the caller needs handed back — the format's own name for the
/// thing that registered. Descriptors are identified by the descriptor, which
/// is what both formats unregister by.
#[cfg(all(unix, not(target_os = "macos")))]
pub struct FdWatch<K> {
    watched: Mutex<Vec<(K, RawFd, Interest)>>,
}

#[cfg(all(unix, not(target_os = "macos")))]
impl<K> Default for FdWatch<K> {
    fn default() -> FdWatch<K> {
        FdWatch {
            watched: Mutex::new(Vec::new()),
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
impl<K: Copy> FdWatch<K> {
    /// Start watching `fd`. False if it is already watched or there is no room.
    pub fn watch(&self, key: K, fd: RawFd, interest: Interest) -> bool {
        let Ok(mut watched) = self.watched.lock() else {
            return false;
        };
        if watched.iter().any(|(_, other, _)| *other == fd) {
            // Both formats say to modify rather than register twice, and a
            // duplicate would have the plugin told twice per turn.
            return false;
        }
        if watched.len() >= MAX_WATCHED {
            log::warn!("a plugin asked to watch more than {MAX_WATCHED} descriptors");
            return false;
        }
        watched.push((key, fd, interest));
        true
    }

    /// Change what `fd` is watched for. False if it is not watched.
    pub fn modify(&self, fd: RawFd, interest: Interest) -> bool {
        let Ok(mut watched) = self.watched.lock() else {
            return false;
        };
        match watched.iter_mut().find(|(_, other, _)| *other == fd) {
            Some(entry) => {
                entry.2 = interest;
                true
            }
            None => false,
        }
    }

    /// Stop watching `fd`. False if it was not watched.
    pub fn forget(&self, fd: RawFd) -> bool {
        self.forget_by(|_, other| *other == fd)
    }

    /// Stop watching whatever `doomed` picks out.
    ///
    /// The two formats unregister by different things — CLAP names the
    /// descriptor, VST3 names the handler — and neither is wrong, so the
    /// predicate is the caller's.
    pub fn forget_by(&self, doomed: impl Fn(&K, &RawFd) -> bool) -> bool {
        let Ok(mut watched) = self.watched.lock() else {
            return false;
        };
        let before = watched.len();
        watched.retain(|(key, fd, _)| !doomed(key, fd));
        before != watched.len()
    }

    fn is_watched(&self, fd: RawFd) -> bool {
        self.watched
            .lock()
            .is_ok_and(|watched| watched.iter().any(|(_, other, _)| *other == fd))
    }

    /// Tell `ready` about every watched descriptor that has something for it.
    ///
    /// Does not block: the caller has a frame to get back to.
    pub fn dispatch(&self, ready: impl Fn(K, RawFd, Readiness)) {
        let watched: Vec<(K, RawFd, Interest)> = {
            let Ok(watched) = self.watched.lock() else {
                return;
            };
            if watched.is_empty() {
                return;
            }
            watched.clone()
        };

        let mut polls: Vec<libc::pollfd> = watched
            .iter()
            .map(|(_, fd, interest)| libc::pollfd {
                fd: *fd,
                events: events_of(*interest),
                revents: 0,
            })
            .collect();
        // Zero timeout, so this costs a syscall and never a wait.
        if unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, 0) } <= 0 {
            return;
        }

        for (poll, (key, fd, _)) in polls.iter().zip(&watched) {
            match outcome_of(poll.revents) {
                Outcome::Quiet => {}
                Outcome::Gone => {
                    self.forget(*fd);
                }
                Outcome::Ready(readiness) => {
                    // Against the live list, not the copy: an earlier callback
                    // in this very loop may have dropped this one.
                    if self.is_watched(*fd) {
                        ready(*key, *fd, readiness);
                    }
                }
            }
        }
    }
}

/// Timers a plugin has asked the host to run.
///
/// `K` identifies a timer to the plugin: the id the host handed out under CLAP,
/// the handler under VST3.
pub struct TimerWheel<K> {
    timers: Mutex<Vec<Timer<K>>>,
}

struct Timer<K> {
    key: K,
    period: Duration,
    due: Instant,
}

impl<K> Default for TimerWheel<K> {
    fn default() -> TimerWheel<K> {
        TimerWheel {
            timers: Mutex::new(Vec::new()),
        }
    }
}

impl<K: Copy + PartialEq> TimerWheel<K> {
    /// Start a timer, or re-time one already running under `key`.
    ///
    /// The period is clamped: see [`MIN_PERIOD`] and [`MAX_PERIOD`].
    pub fn arm(&self, key: K, period: Duration) -> bool {
        let Ok(mut timers) = self.timers.lock() else {
            return false;
        };
        let period = period.clamp(MIN_PERIOD, MAX_PERIOD);
        let due = Instant::now() + period;
        if let Some(timer) = timers.iter_mut().find(|t| t.key == key) {
            timer.period = period;
            timer.due = due;
            return true;
        }
        if timers.len() >= MAX_WATCHED {
            log::warn!("a plugin asked for more than {MAX_WATCHED} timers");
            return false;
        }
        timers.push(Timer { key, period, due });
        true
    }

    /// Stop a timer. False if it was not running.
    pub fn disarm(&self, key: K) -> bool {
        let Ok(mut timers) = self.timers.lock() else {
            return false;
        };
        let before = timers.len();
        timers.retain(|t| t.key != key);
        before != timers.len()
    }

    fn is_armed(&self, key: K) -> bool {
        self.timers
            .lock()
            .is_ok_and(|timers| timers.iter().any(|t| t.key == key))
    }

    /// Run every timer that has come due.
    pub fn dispatch(&self, fire: impl Fn(K)) {
        let now = Instant::now();
        let due: Vec<K> = {
            let Ok(mut timers) = self.timers.lock() else {
                return;
            };
            timers
                .iter_mut()
                .filter(|timer| timer.due <= now)
                .map(|timer| {
                    // From now, not from when it was due: a UI thread that
                    // stalled must not come back to a burst of catch-up ticks.
                    timer.due = now + timer.period;
                    timer.key
                })
                .collect()
        };

        for key in due {
            // Same rule as descriptors: an earlier callback may have stopped
            // this one.
            if !self.is_armed(key) {
                continue;
            }
            fire(key);
        }
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
/// What one descriptor's `revents` means for it.
#[cfg(all(unix, not(target_os = "macos")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// Nothing happened.
    Quiet,
    /// The descriptor is not open. A plugin that closed one without
    /// unregistering would otherwise have it reported every turn for the rest
    /// of the session, so it is dropped rather than reported.
    Gone,
    /// Something the plugin asked about, or an error it did not ask about but
    /// is better off being told.
    Ready(Readiness),
}

#[cfg(all(unix, not(target_os = "macos")))]
fn outcome_of(revents: libc::c_short) -> Outcome {
    if revents & libc::POLLNVAL != 0 {
        return Outcome::Gone;
    }
    let readiness = readiness_of(revents);
    if readiness.is_anything() {
        Outcome::Ready(readiness)
    } else {
        Outcome::Quiet
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn events_of(interest: Interest) -> libc::c_short {
    let mut events = 0;
    if interest.read {
        events |= libc::POLLIN;
    }
    if interest.write {
        events |= libc::POLLOUT;
    }
    events
}

#[cfg(all(unix, not(target_os = "macos")))]
fn readiness_of(revents: libc::c_short) -> Readiness {
    Readiness {
        read: revents & libc::POLLIN != 0,
        write: revents & libc::POLLOUT != 0,
        error: revents & (libc::POLLERR | libc::POLLHUP) != 0,
    }
}

#[cfg(all(test, unix, not(target_os = "macos")))]
mod descriptors {
    use super::*;
    use std::cell::RefCell;

    /// A pipe with a byte in it, which is the smallest thing that is reliably
    /// readable without a plugin to produce one.
    struct Pipe {
        read: RawFd,
        write: RawFd,
    }

    impl Pipe {
        fn new() -> Pipe {
            let mut ends = [0; 2];
            assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
            Pipe {
                read: ends[0],
                write: ends[1],
            }
        }

        fn fill(&self) {
            let byte = 1u8;
            assert_eq!(
                unsafe { libc::write(self.write, (&raw const byte).cast(), 1) },
                1
            );
        }
    }

    impl Drop for Pipe {
        fn drop(&mut self) {
            unsafe {
                libc::close(self.read);
                libc::close(self.write);
            }
        }
    }

    #[test]
    fn a_descriptor_with_something_to_read_is_reported() {
        let pipe = Pipe::new();
        let watch = FdWatch::default();
        assert!(watch.watch(7u32, pipe.read, Interest::READ));

        let seen = RefCell::new(Vec::new());
        watch.dispatch(|key, fd, readiness| seen.borrow_mut().push((key, fd, readiness)));
        assert!(seen.borrow().is_empty(), "an empty pipe has nothing to say");

        pipe.fill();
        watch.dispatch(|key, fd, readiness| seen.borrow_mut().push((key, fd, readiness)));
        let seen = seen.into_inner();
        assert_eq!(seen.len(), 1);
        assert_eq!((seen[0].0, seen[0].1), (7, pipe.read));
        assert!(seen[0].2.read);
    }

    #[test]
    fn a_descriptor_dropped_from_inside_a_callback_is_not_reported_after() {
        let first = Pipe::new();
        let second = Pipe::new();
        first.fill();
        second.fill();

        let watch = FdWatch::default();
        watch.watch(1u32, first.read, Interest::READ);
        watch.watch(2u32, second.read, Interest::READ);

        // What a plugin is entitled to do, and what a copy-walking loop would
        // get wrong: the first callback unregisters the second.
        let seen = RefCell::new(Vec::new());
        watch.dispatch(|key, _, _| {
            seen.borrow_mut().push(key);
            watch.forget(second.read);
        });
        assert_eq!(seen.into_inner(), vec![1]);
    }

    #[test]
    fn a_descriptor_is_watched_once_and_forgotten_once() {
        let pipe = Pipe::new();
        let watch = FdWatch::default();
        assert!(watch.watch(1u32, pipe.read, Interest::READ));
        assert!(
            !watch.watch(1u32, pipe.read, Interest::READ),
            "no duplicate"
        );
        assert!(watch.modify(pipe.read, Interest::default()));
        assert!(watch.forget(pipe.read));
        assert!(!watch.forget(pipe.read), "already forgotten");
        assert!(!watch.modify(pipe.read, Interest::READ));
    }

    /// What a descriptor's `revents` is taken to mean.
    ///
    /// A pure function rather than a test that closes a descriptor and looks at
    /// what happens: descriptor numbers are reused the moment they are freed,
    /// and this crate's own tests open sockets on other threads, so such a test
    /// asserts on whatever happened to inherit the number.
    #[test]
    fn a_descriptor_that_is_not_open_is_dropped_rather_than_reported() {
        assert_eq!(outcome_of(libc::POLLNVAL), Outcome::Gone);
        // Even alongside something that would otherwise look like news.
        assert_eq!(outcome_of(libc::POLLNVAL | libc::POLLIN), Outcome::Gone);

        assert_eq!(outcome_of(0), Outcome::Quiet);
        assert_eq!(
            outcome_of(libc::POLLIN),
            Outcome::Ready(Readiness {
                read: true,
                ..Readiness::default()
            })
        );
        // Not asked for, still told: a descriptor that has hung up is not
        // coming back, and a plugin waiting on it needs to know.
        assert_eq!(
            outcome_of(libc::POLLHUP),
            Outcome::Ready(Readiness {
                error: true,
                ..Readiness::default()
            })
        );
    }
}

#[cfg(test)]
mod timers {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn a_timer_fires_once_its_period_is_up() {
        let wheel = TimerWheel::default();
        assert!(wheel.arm(1u32, Duration::from_millis(0)));

        let fired = RefCell::new(Vec::new());
        wheel.dispatch(|key| fired.borrow_mut().push(key));
        assert!(fired.borrow().is_empty(), "not due yet");

        // The floor, not the zero that was asked for.
        std::thread::sleep(MIN_PERIOD * 2);
        wheel.dispatch(|key| fired.borrow_mut().push(key));
        assert_eq!(&*fired.borrow(), &[1]);
    }

    #[test]
    fn a_timer_stopped_from_inside_a_callback_does_not_fire() {
        let wheel = TimerWheel::default();
        wheel.arm(1u32, MIN_PERIOD);
        wheel.arm(2u32, MIN_PERIOD);
        std::thread::sleep(MIN_PERIOD * 2);

        let fired = RefCell::new(Vec::new());
        wheel.dispatch(|key| {
            fired.borrow_mut().push(key);
            wheel.disarm(2);
        });
        assert_eq!(fired.into_inner(), vec![1]);
    }

    #[test]
    fn arming_a_running_timer_re_times_it_rather_than_adding_one() {
        let wheel = TimerWheel::default();
        wheel.arm(1u32, MIN_PERIOD);
        wheel.arm(1u32, MIN_PERIOD);
        std::thread::sleep(MIN_PERIOD * 2);

        let fired = RefCell::new(Vec::new());
        wheel.dispatch(|key| fired.borrow_mut().push(key));
        assert_eq!(fired.into_inner(), vec![1]);
        assert!(wheel.disarm(1));
        assert!(!wheel.disarm(1));
    }

    /// A period big enough to overflow `Instant` arithmetic must not reach it.
    #[test]
    fn an_absurd_period_is_clamped_rather_than_panicking() {
        let wheel = TimerWheel::default();
        assert!(wheel.arm(1u32, Duration::MAX));
        wheel.dispatch(|_| panic!("nothing is due a second after being armed"));
    }
}
