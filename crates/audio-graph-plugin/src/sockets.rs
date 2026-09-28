//! What each param socket is carrying, carried from the audio thread to the
//! editor.
//!
//! The audio thread copies the whole register file after each block; the main
//! thread says, at publish, which register is which socket. Neither half knows
//! which sockets the editor will ask about, so a control that starts showing
//! its input needs no change here or in the compiler.

use std::array;
use std::sync::atomic::{AtomicU64, Ordering, fence};

use audio_graph_engine::{MAX_REGISTERS, NodeId, Reg};
use parking_lot::Mutex;

/// A param output socket: the node and its output port.
pub type Socket = (NodeId, u8);

/// Engine publications start at one; zero is "no program adopted yet", and
/// here also "the values are being replaced".
const NONE: u64 = 0;

pub struct LiveSockets {
    /// The publication whose registers `values` holds, or [`NONE`] while they
    /// change hands between two programs.
    publication: AtomicU64,
    /// f64 bits, indexed by register.
    values: [AtomicU64; MAX_REGISTERS],
    /// Which register holds which socket, and the first publication that
    /// numbers them this way.
    ///
    /// Kept across publications that number them the same, which is almost
    /// all of them: dragging a control recompiles every frame, and a map keyed
    /// to one publication would lose the values for a frame each time.
    map: Mutex<(u64, Vec<(Socket, Reg)>)>,
}

impl Default for LiveSockets {
    fn default() -> LiveSockets {
        LiveSockets {
            publication: AtomicU64::new(NONE),
            values: array::from_fn(|_| AtomicU64::new(0)),
            map: Mutex::new((u64::MAX, Vec::new())),
        }
    }
}

impl LiveSockets {
    /// Main thread, before handing a program over: whatever the audio thread
    /// reports from here on may be numbered by `sockets`.
    ///
    /// Until [`LiveSockets::published`] says which publication that is, no
    /// value is shown rather than one read through the wrong map.
    pub fn publishing(&self, sockets: &[(Socket, Reg)]) {
        let mut map = self.map.lock();
        if map.1 != sockets {
            *map = (u64::MAX, sockets.to_vec());
        }
    }

    /// Main thread, after handing a program over.
    pub fn published(&self, publication: u64) {
        let mut map = self.map.lock();
        map.0 = map.0.min(publication);
    }

    /// Audio thread, after each block. Lock-free and allocation-free.
    pub fn report(&self, publication: u64, registers: &[f64]) {
        if self.publication.load(Ordering::Relaxed) != publication {
            self.publication.store(NONE, Ordering::Relaxed);
            fence(Ordering::Release);
        }
        for (cell, &value) in self.values.iter().zip(registers) {
            cell.store(value.to_bits(), Ordering::Relaxed);
        }
        self.publication.store(publication, Ordering::Release);
    }

    /// What each mapped socket carried at the end of the last block, or
    /// nothing if the program running is not one the map describes.
    pub fn read(&self) -> Vec<(Socket, f64)> {
        let map = self.map.lock();
        // A reader that lands in the middle of a program change tries again
        // rather than blanking every value for a frame: the change is a few
        // hundred stores.
        for _ in 0..4 {
            let before = self.publication.load(Ordering::Acquire);
            if before == NONE || before < map.0 {
                return Vec::new();
            }
            let values: Vec<(Socket, f64)> = map
                .1
                .iter()
                .filter_map(|&(socket, reg)| {
                    let bits = self.values.get(reg as usize)?.load(Ordering::Relaxed);
                    Some((socket, f64::from_bits(bits)))
                })
                .collect();
            fence(Ordering::Acquire);
            if self.publication.load(Ordering::Relaxed) == before {
                return values;
            }
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A value is only ever read through the map its program was compiled
    /// with.
    ///
    /// Registers are renumbered by a recompile that adds a node, so an old
    /// program's values read through the new map would put one socket's
    /// number on another socket.
    #[test]
    fn values_from_a_program_the_map_does_not_describe_are_not_shown() {
        let live = LiveSockets::default();
        live.publishing(&[((1, 0), 0)]);
        live.published(1);
        live.report(1, &[0.5]);
        assert_eq!(live.read(), vec![((1, 0), 0.5)]);

        live.publishing(&[((2, 0), 0), ((1, 0), 1)]);
        assert!(live.read().is_empty(), "no publication yet for the new map");
        live.published(2);
        assert!(live.read().is_empty(), "the audio thread still runs 1");
        live.report(2, &[0.25, 0.5]);
        assert_eq!(live.read(), vec![((2, 0), 0.25), ((1, 0), 0.5)]);
    }

    /// Recompiling to the same numbering keeps the values on screen.
    ///
    /// Dragging any control recompiles every frame; a live value that blanked
    /// until each new program had run a block would flicker the whole time.
    #[test]
    fn a_recompile_that_numbers_the_same_does_not_blank_the_values() {
        let live = LiveSockets::default();
        live.publishing(&[((1, 0), 0)]);
        live.published(1);
        live.report(1, &[0.5]);

        live.publishing(&[((1, 0), 0)]);
        live.published(2);
        assert_eq!(live.read(), vec![((1, 0), 0.5)]);
    }
}
