//! Notes the wrapper has to lose, and the notice that says it did.
//!
//! Both limits are fixed at activation because the audio thread may not grow
//! anything, so a DAW sending past them loses notes. That much is by design;
//! losing them without a word is not. These drive the wrapper past each limit
//! the way a DAW would and read the notice the editor shows.

mod harness;

use harness::{Block, Daw, LIVE, fx_layout};

use audio_graph_plugin::{ErrorSource, Wrapper, WrapperKind};
use nice_plug::prelude::NoteEvent;

const FRAMES: usize = 64;

fn running() -> Wrapper {
    let mut wrapper = Wrapper::default();
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .expect("the first activation");
    wrapper.shared().adopt_default_patch();
    wrapper
}

/// `count` distinct held notes, walking the keys of one channel before moving
/// to the next, so no two share an address.
fn note_ons(from: usize, count: usize) -> Vec<NoteEvent<()>> {
    (from..from + count)
        .map(|n| NoteEvent::NoteOn {
            timing: 0,
            voice_id: None,
            channel: (n / 128) as u8,
            note: (n % 128) as u8,
            velocity: 0.8,
        })
        .collect()
}

fn notice(wrapper: &mut Wrapper) -> Option<String> {
    wrapper.tick();
    wrapper.shared().error_message(ErrorSource::Notes)
}

/// A block carrying more events than the wrapper holds is reported as
/// dropped, even though the engine never saw the ones that did not fit.
#[test]
fn a_block_with_more_events_than_the_wrapper_holds_is_reported() {
    let _thread = plugin_host::init_thread().unwrap();
    let mut wrapper = running();
    let mut daw = Daw::playing();
    assert_eq!(notice(&mut wrapper), None, "nothing lost yet");

    daw.incoming = note_ons(0, 1100);
    Block::silent(FRAMES).process(&mut wrapper, &mut daw);

    let shown = notice(&mut wrapper).expect("the loss is reported");
    assert!(shown.contains("were dropped"), "{shown}");
}

/// More notes held at once than the ledger tracks are reported as cut off.
#[test]
fn more_held_notes_than_the_ledger_tracks_are_reported() {
    let _thread = plugin_host::init_thread().unwrap();
    let mut wrapper = running();
    let mut daw = Daw::playing();

    // Spread over blocks so no single block overflows anything: the only
    // limit reached is the number of notes alive at once.
    for block in 0..3 {
        daw.incoming = note_ons(block * 100, 100);
        Block::silent(FRAMES).process(&mut wrapper, &mut daw);
    }

    let shown = notice(&mut wrapper).expect("the loss is reported");
    assert!(
        shown.contains(&format!(
            "{} note(s) were cut off",
            300 - audio_graph_engine::MAX_LIVE_NOTES
        )),
        "{shown}"
    );
    assert!(!shown.contains("dropped"), "{shown}");
}
