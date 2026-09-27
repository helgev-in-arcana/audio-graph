//! Which parameter the user last moved in each sub-plugin's own window.
//!
//! What "learn" on a plugin node reads: arm it, move a control in the plugin's
//! window, and that parameter gets a socket. The edit reaches the wrapper on
//! whichever thread the format delivers it — VST3's `performEdit` on the UI
//! thread, CLAP's output events on the audio thread while it is processing —
//! so the record is a table of atomics that neither side has to lock.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use audio_graph_engine::Touch;

use crate::config::MAX_INSTANCES;

/// One entry per instance: the parameter id in the low half, and in the high
/// half the number of the edit that set it, so a second touch of the same
/// parameter is still a new touch.
pub struct Touched {
    last: [AtomicU64; MAX_INSTANCES],
    edits: AtomicU32,
}

impl Default for Touched {
    fn default() -> Self {
        Touched {
            last: std::array::from_fn(|_| AtomicU64::new(0)),
            edits: AtomicU32::new(0),
        }
    }
}

impl Touched {
    /// The user moved parameter `param` of `instance`. Any thread; lock-free.
    pub fn record(&self, instance: u32, param: u32) {
        let Some(slot) = self.last.get(instance as usize) else {
            return;
        };
        // Starts at 1, so an entry of 0 means "never touched". Wrapping after
        // four billion edits only matters to a learn armed across the wrap.
        let edit = self
            .edits
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
            .max(1);
        slot.store(
            (u64::from(edit) << 32) | u64::from(param),
            Ordering::Relaxed,
        );
    }

    /// Every instance's last touch, for the editor to draw from.
    pub fn snapshot(&self) -> Vec<Touch> {
        self.last
            .iter()
            .map(|slot| {
                let entry = slot.load(Ordering::Relaxed);
                Touch {
                    edit: (entry >> 32) as u32,
                    param: entry as u32,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Touching the same parameter twice is two touches, and an instance
    /// nobody touched says so.
    #[test]
    fn each_touch_is_new_even_on_the_same_parameter() {
        let touched = Touched::default();
        assert_eq!(touched.snapshot()[1].edit, 0, "never touched");
        touched.record(1, 7);
        let first = touched.snapshot()[1];
        touched.record(1, 7);
        let second = touched.snapshot()[1];
        assert_eq!((first.param, second.param), (7, 7));
        assert!(second.edit > first.edit);
        touched.record(MAX_INSTANCES as u32, 3);
    }
}
