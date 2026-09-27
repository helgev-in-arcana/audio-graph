//! A control moved in a sub-plugin's own window is recorded for the canvas
//! to learn a parameter socket from.

mod harness;

use harness::{Block, Daw, LIVE, fixture_as_clap, fx_layout};

use audio_graph_plugin::{Wrapper, WrapperKind};
use plugin_host::ParamId;

/// The fixture's `ask` parameter, and the request that makes it behave as if
/// its gain knob had been turned in its window.
const PARAM_ASK: ParamId = ParamId(5);
const ASK_REQUEST_FLUSH: f64 = 12.0;
const PARAM_GAIN: u32 = 0;

/// An edit a running CLAP plugin reports from its own window reaches the
/// record the canvas learns from, naming the parameter that moved.
#[test]
fn an_edit_in_the_plugins_window_is_recorded() {
    let _thread = plugin_host::init_thread().unwrap();
    let mut wrapper = Wrapper::default();
    wrapper
        .activate(WrapperKind::Effect, &fx_layout(), &LIVE)
        .expect("the first activation");
    wrapper
        .shared()
        .load(&fixture_as_clap("learn"))
        .expect("the fixture loads");
    wrapper.shared().adopt_default_patch();
    let mut daw = Daw::playing();
    Block::silent(64).process(&mut wrapper, &mut daw);
    assert_eq!(wrapper.shared().touched().snapshot()[0].edit, 0);

    wrapper
        .shared()
        .main()
        .host
        .set_sub_param(0, PARAM_ASK, ASK_REQUEST_FLUSH)
        .expect("the fixture takes the request");
    for _ in 0..4 {
        wrapper.tick();
        Block::silent(64).process(&mut wrapper, &mut daw);
    }

    let touch = wrapper.shared().touched().snapshot()[0];
    assert_ne!(touch.edit, 0, "the edit was recorded");
    assert_eq!(touch.param, PARAM_GAIN);
    wrapper.deactivate();
}
