//! Everything the editor's buttons do, without the editor.
//!
//! The egui layer is a thin shell over [`Shared`]: each control resolves to one
//! call here. Driving those calls directly against a real installed plugin
//! covers the part that can actually be wrong — the load/bind/activate ordering
//! — and leaves only the drawing to be checked by eye.
//!
//! One `#[test]`, deliberately. VST3 pins its objects to the thread that created
//! them, and the test harness runs `#[test]` functions on a pool of threads;
//! splitting this would deadlock rather than fail.

use std::sync::Arc;

use plugin_host_api::{AudioConfig, HostContext, RestartReason};
use subhost_adapter::SubHost;
use wrapper_plugin::{Shared, WrapperParams};

struct SilentHost;

impl HostContext for SilentHost {
    fn host_name(&self) -> &str {
        "editor-actions test"
    }
    fn request_restart(&self, _reason: RestartReason) {}
    fn latency_changed(&self, _samples: u32) {}
    fn param_edited(&self, _id: plugin_host_api::ParamId, _value: f64) {}
}

/// Plugins this test will not instantiate.
///
/// Mostly not a judgement on them — they are simply the wrong shape for an
/// unattended test. Sampler and amp-sim hosts scan multi-gigabyte content
/// libraries or put up an authorisation dialog on their first instantiation,
/// and either one stops the run dead with no output.
///
/// Chroma is here for a sharper reason: it puts a **top-level window** on screen
/// during instantiation, before any host has asked for an editor, and that
/// window stops responding unless somebody pumps the message loop. A host that
/// merely instantiates it — a plugin scanner, this test — hangs. It is the same
/// plugin whose editor faults on first paint (ARCHITECTURE §13), and the two are
/// probably the same underlying assumption: that it is running inside a DAW that
/// is already pumping.
///
/// `AUDIO_GRAPH_TEST_SUB` overrides the search entirely when a specific plugin
/// is wanted.
const AVOID: &[&str] = &[
    "AmpliTube",
    "BBC Symphony",
    "Chroma",
    "Kontakt",
    "MODO",
    "Vienna",
    "Sine",
    "OTT",
];

/// How many modules to try before giving up.
const CANDIDATES: usize = 6;

/// A plugin to test against, chosen from whatever is installed.
///
/// Not a fixed name: this has to run on a machine with a different set of
/// plugins from the one it was written on. Anything with parameters will do.
fn a_plugin_with_parameters() -> Option<(std::path::PathBuf, Arc<Shared>)> {
    let mut tried = 0;
    for path in candidate_paths() {
        if tried >= CANDIDATES {
            break;
        }
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        if AVOID.iter().any(|a| name.contains(a)) {
            continue;
        }
        tried += 1;
        eprintln!("trying {name}");

        let params = WrapperParams::new();
        let shared = Shared::new(SubHost::new(Arc::new(SilentHost)), params);
        if shared.lock().load(&path).is_err() {
            continue;
        }
        if !shared.lock().host.params().is_empty() {
            return Some((path, shared));
        }
    }
    None
}

fn candidate_paths() -> Vec<std::path::PathBuf> {
    if let Ok(explicit) = std::env::var("AUDIO_GRAPH_TEST_SUB") {
        return vec![std::path::PathBuf::from(explicit)];
    }
    vst3_host::default_plugin_directories()
        .iter()
        .flat_map(|d| vst3_host::find_modules(d))
        .collect()
}

#[test]
fn the_editors_actions_work_against_an_installed_plugin() {
    let Some((path, shared)) = a_plugin_with_parameters() else {
        eprintln!("no installed VST3 with parameters; skipping");
        return;
    };
    eprintln!("driving the editor's actions against {}", path.display());

    // The DAW has activated us, so the editor's later loads have a
    // configuration to activate against.
    shared.lock().config = Some(AudioConfig {
        sample_rate: 48_000.0,
        max_block_size: 512,
        input_channels: 2,
        output_channels: 2,
        offline: true,
    });

    // "Rescan" — the list the editor draws.
    let installed: usize = vst3_host::default_plugin_directories()
        .iter()
        .map(|d| vst3_host::find_modules(d).len())
        .sum();
    assert!(installed > 0, "the plugin list would be empty");

    // Clicking an entry in that list.
    shared.lock().load(&path).expect("reload from the list");
    assert!(shared.lock().host.is_loaded());
    assert!(
        shared.lock().processor.is_some(),
        "a load while the DAW is running has to leave the sub-plugin processing; \
         otherwise picking a plugin mid-session silently mutes the track"
    );

    // Clicking a parameter row: bind, then re-activate so the processor picks
    // the new target up.
    let first = shared.lock().host.params()[0].clone();
    {
        let mut state = shared.lock();
        state.host.bind_slot(0, first.id).expect("bind");
        state.rebind().expect("rebind");
    }
    assert!(
        shared.lock().host.slots().resolved(0).is_some(),
        "the binding has to resolve against the plugin it was just made from"
    );
    assert!(shared.lock().processor.is_some(), "still processing after a rebind");

    // Every edit writes the state back, so the DAW always has something current
    // to save.
    shared.store_state();
    let saved = shared.params().state.0.read().unwrap().clone();
    assert!(saved.contains(&first.name), "the binding is missing from the saved state");

    // The "Open plugin GUI" button is deliberately *not* exercised here.
    // Opening a real editor needs somebody pumping the Win32 message loop, and
    // a test binary is not pumping one; several plugins simply block. That path
    // already has a harness built for it — `host-cli sweep --gui` runs it across
    // every installed plugin, in both teardown orders, one child process each.

    // "x" on a slot row.
    {
        let mut state = shared.lock();
        state.host.slots_mut().clear(0);
        state.rebind().expect("rebind after clearing");
    }
    assert!(shared.lock().host.slots().resolved(0).is_none());

    // "Unload".
    shared.lock().unload();
    assert!(!shared.lock().host.is_loaded());
    assert!(
        shared.lock().processor.is_none(),
        "unloading has to take the processor with it, or the audio thread keeps \
         a processor whose plugin is gone"
    );
}
