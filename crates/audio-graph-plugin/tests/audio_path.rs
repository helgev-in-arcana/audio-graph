//! The wrapper's own `process`, driven the way a DAW drives it.
//!
//! Everything else about the audio path is checked through the bundled binary
//! by `host-cli`. These are the questions that are about the wrapper itself
//! rather than about the graph: what comes out of a block, and what the wrapper
//! says to the host while blocks are going through it.

mod harness;

use harness::{BOUNCE, Block, Daw, LIVE, fixture_as_clap, fixture_state, fx_layout};

use audio_graph_plugin::{Wrapper, WrapperKind};

/// The latency the fixture is asked to claim. Any number a plugin might
/// plausibly want; nothing here depends on which.
const LATENCY: u32 = 128;

/// What the wrapper puts on the input, and the least it may leave on the
/// output for the block to count as having come through.
const LEVEL: f32 = 0.5;

/// A wrapper with the fixture wired between input and output, running.
fn playing(name: &str) -> Wrapper {
    plugin_host::init_thread();
    let mut wrapper = Wrapper::default();
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .expect("the first activation");
    wrapper
        .shared()
        .load(&fixture_as_clap(name))
        .expect("the fixture loads");
    wrapper.shared().adopt_default_patch();
    wrapper
}

/// A bounce renders the audio, and leaves the wrapper able to go on rendering
/// it.
///
/// A render mode change is a deactivate and an activate, and every block of the
/// export goes through the configuration that pair leaves behind. A wrapper
/// that only sets its audio path up on the first of those writes a silent file
/// and stays silent on the desk afterwards.
#[test]
fn a_bounce_renders_the_audio_and_gives_it_back_afterwards() {
    let mut wrapper = playing("audio-path-bounce");
    let mut daw = Daw::playing();

    let mut audible = |wrapper: &mut Wrapper, what: &str| {
        let mut block = Block::silent(256);
        block.fill(LEVEL);
        block.process(wrapper, &mut daw);
        assert!(
            block.peak() > 0.0,
            "{what}: the block came back silent, with a peak of {}",
            block.peak()
        );
    };

    audible(&mut wrapper, "on the configuration it was loaded with");

    for (what, config) in [
        ("after a plain re-activation", LIVE),
        ("during the bounce", BOUNCE),
        ("back at the desk", LIVE),
    ] {
        wrapper.deactivate();
        wrapper
            .activate(WrapperKind::Effect, &fx_layout(), &config)
            .unwrap_or_else(|| panic!("{what} activates"));
        audible(&mut wrapper, what);
    }

    wrapper.deactivate();
}

/// A latency that appears while the project is playing reaches the DAW.
///
/// Dropping a plugin with lookahead onto the canvas mid-take moves the whole
/// track, and the host has to hear about it before the next block is used.
/// `process` is the only place the wrapper is handed anything that can say so
/// between activations, so a wrapper that only answers at activate leaves the
/// track running late until the DAW next restarts it.
#[test]
fn a_latency_that_appears_mid_session_reaches_the_daw() {
    plugin_host::init_thread();
    let mut wrapper = Wrapper::default();
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .expect("the first activation");

    let mut daw = Daw::playing();
    let mut block = Block::silent(256);
    block.process(&mut wrapper, &mut daw);
    assert_eq!(
        daw.latency.get(),
        None,
        "an empty canvas costs nothing, and a block with nothing to report \
         must not restart the DAW's processing to say so"
    );

    // The user picks a plugin, and it comes with lookahead.
    wrapper
        .shared()
        .load(&fixture_as_clap("audio-path-latency"))
        .expect("the fixture loads");
    wrapper
        .shared()
        .main()
        .host
        .load_sub_state(0, &fixture_state(f64::from(LATENCY)))
        .expect("the fixture takes its state");
    // A plugin answers for its latency when it starts, so the preset only
    // becomes a number once it has been restarted around it.
    wrapper.shared().rebind().expect("the fixture restarts");
    wrapper.shared().adopt_default_patch();

    block.process(&mut wrapper, &mut daw);
    assert_eq!(
        daw.latency.get(),
        Some(LATENCY),
        "the track is still being aligned by a latency the patch no longer has"
    );

    wrapper.deactivate();
}
