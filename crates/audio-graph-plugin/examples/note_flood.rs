//! Writes a MIDI file that makes AudioGraph lose notes on purpose, for seeing
//! the notice about it in a DAW.
//!
//! ```sh
//! cargo run -p audio-graph-plugin --example note_flood -- note_flood.mid
//! ```
//!
//! Put the file on the track feeding AudioGraph and play it. Two things happen,
//! a bar apart:
//!
//! - **Beat 1:** more distinct notes held at once than the wrapper tracks
//!   ([`MAX_LIVE_NOTES`]), spread over several channels because one channel
//!   has only 128 keys. The notice says notes were cut off.
//! - **Beat 5:** more note-ons at one instant than the wrapper takes in a
//!   block. The notice says note events were dropped — and more notes cut
//!   off, since the ones that do get in are held as well.
//!
//! Needs a DAW that keeps MIDI channels on the way to the plugin; one that
//! folds every channel into one sends duplicate keys instead, and far fewer
//! notes survive to be held.

use std::io::Write;

use audio_graph_engine::MAX_LIVE_NOTES;

/// Ticks per quarter note.
const PPQ: u32 = 480;

/// Past the wrapper's intake of 1024 events a block, with room to spare.
const BURST: usize = 1100;

fn main() -> std::io::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "note_flood.mid".to_owned());

    let held = MAX_LIVE_NOTES + 44;
    let mut events = Vec::new();
    events.extend(chord(held, 0, 2 * PPQ));
    events.extend(chord(BURST, 4 * PPQ, 5 * PPQ));
    events.sort_by_key(|&(tick, status, _)| (tick, status & 0xF0 == 0x90));

    let mut track = Vec::new();
    let mut now = 0;
    for (tick, status, key) in events {
        variable_length(&mut track, tick - now);
        now = tick;
        let velocity = if status & 0xF0 == 0x90 { 100 } else { 0 };
        track.extend([status, key, velocity]);
    }
    track.extend([0x00, 0xFF, 0x2F, 0x00]);

    let mut file = std::fs::File::create(&path)?;
    file.write_all(b"MThd")?;
    file.write_all(&6u32.to_be_bytes())?;
    // Format 0, one track, PPQ ticks per quarter note.
    file.write_all(&0u16.to_be_bytes())?;
    file.write_all(&1u16.to_be_bytes())?;
    file.write_all(&(PPQ as u16).to_be_bytes())?;
    file.write_all(b"MTrk")?;
    file.write_all(&(track.len() as u32).to_be_bytes())?;
    file.write_all(&track)?;

    println!("wrote {path}: {held} held notes at beat 1, {BURST} note-ons at once at beat 5");
    Ok(())
}

/// `count` distinct notes from `on` to `off`, as `(tick, status, key)`: every
/// key of channel 1, then of channel 2, and so on.
fn chord(count: usize, on: u32, off: u32) -> Vec<(u32, u8, u8)> {
    (0..count)
        .flat_map(|n| {
            let channel = (n / 128) as u8 & 0x0F;
            let key = (n % 128) as u8;
            [(on, 0x90 | channel, key), (off, 0x80 | channel, key)]
        })
        .collect()
}

/// A MIDI variable-length quantity: seven bits a byte, most significant first,
/// the high bit set on every byte but the last.
fn variable_length(out: &mut Vec<u8>, mut value: u32) {
    let mut bytes = vec![(value & 0x7F) as u8];
    value >>= 7;
    while value > 0 {
        bytes.push((value & 0x7F) as u8 | 0x80);
        value >>= 7;
    }
    out.extend(bytes.iter().rev());
}
