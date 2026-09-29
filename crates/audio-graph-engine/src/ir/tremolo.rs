//! What a key-switched tremolo is told: which keys start it, how fast each of
//! them cuts, and how much of every step sounds.
//!
//! One description for the note half and the audio half alike, because the
//! point of having both is that they cut in the same places. Two nodes wired
//! to the same stream and set the same way follow the same grid to the
//! sample.

/// How many keys one tremolo answers to, each with a speed of its own.
///
/// A `Mix`'s ceiling, for a `Mix`'s reason: past it the node is a wall of
/// rows, and it is also as many bits as a `u8` has, which is what holds the
/// rows being pressed.
pub const MAX_TREMOLO_ROWS: usize = 8;

/// How many tremolos, of either half, one program may have. A ceiling because
/// each holds its notes in a table sized once, in
/// [`Engine::new`][crate::Engine::new].
pub const MAX_TREMOLOS: usize = 16;

/// How many notes one MIDI tremolo keeps track of at once — every key on the
/// keyboard. A note past that goes through untouched rather than being lost.
pub const TREMOLO_NOTES: usize = 128;

/// A tremolo's settings, as the engine reads them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TremoloSpec {
    /// Whether a key starts the tremolo until another key says otherwise
    /// (`true`), or only while it is held (`false`).
    pub latch: bool,
    /// The keys, first `rows` of them.
    pub keys: [u8; MAX_TREMOLO_ROWS],
    /// The length of one step for each key, in beats.
    pub steps: [f64; MAX_TREMOLO_ROWS],
    pub rows: u8,
    /// The key that stops a latched tremolo. Held keys stop by being let go,
    /// so a spec that does not latch has none.
    pub stop: Option<u8>,
    /// How much of each step sounds, 0..=1.
    pub share: f64,
}

impl TremoloSpec {
    /// Which row `key` starts, if any.
    pub fn row_of(&self, key: i16) -> Option<usize> {
        self.keys[..usize::from(self.rows).min(MAX_TREMOLO_ROWS)]
            .iter()
            .position(|&k| i16::from(k) == key)
    }

    /// Whether `key` steers this tremolo rather than being played through it.
    pub fn steers(&self, key: i16) -> bool {
        self.row_of(key).is_some() || (self.latch && self.stop.map(i16::from) == Some(key))
    }
}
