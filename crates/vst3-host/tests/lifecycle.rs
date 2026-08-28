//! Lifecycle and state checks against real installed plugins.
//!
//! Skips itself when no plugins are present, so this stays green on a bare CI
//! box while doing real work on a developer machine.
//!
//! **Everything here runs inside one `#[test]` on purpose.** VST3 pins these
//! calls to the thread that created the instance, and the test harness runs
//! separate `#[test]` functions on separate threads in parallel; splitting
//! these up deadlocks against plugins that expect to own the main thread. One
//! sequential test is the shape the format actually permits.

use std::path::PathBuf;
use std::sync::Arc;

use plugin_host_api::{
    AudioConfig, HostContext, ParamFlags, ParamId, RestartReason, SubPluginMain,
};
use vst3_host::{Cid, ClassInfo, Module, Vst3Plugin, default_plugin_directories, find_modules};

#[derive(Default)]
struct TestHost;

impl HostContext for TestHost {
    fn host_name(&self) -> &str {
        "vst3-host tests"
    }
    fn request_restart(&self, _reason: RestartReason) {}
    fn param_edited(&self, _id: ParamId, _plain: f64) {}
}

/// Plugins excluded from in-process lifecycle tests due to known teardown issues.
const EXCLUDED: &[&str] = &["OTT.vst3"];

/// Cap the search: the point is to find *a* usable effect, and some sampler
/// hosts take seconds to instantiate.
const SEARCH_LIMIT: usize = 12;

/// Skip plugins that expose an implausible number of parameters.
///
/// Amp and sampler suites publish thousands of generic `Param N` slots that map
/// onto whatever is loaded at the time and do not persist through
/// `IComponent::getState` at all. They are legitimate plugins but useless as a
/// probe: a state test against them measures their preset system, not ours.
const MAX_PARAMS_FOR_PROBE: usize = 200;

/// How many parameters to try before concluding state does not round-trip.
///
/// One failure proves nothing — plugins refuse writes to some parameters and
/// recompute others from the transport on every block.
const CANDIDATES: usize = 8;

fn candidates() -> Vec<PathBuf> {
    // Initialize COM STA apartment on test runner thread because plugins assume one exists.
    vst3_host::init_apartment();
    default_plugin_directories()
        .iter()
        .flat_map(|d| find_modules(d))
        .filter(|p| {
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            !EXCLUDED.contains(&name.as_str())
        })
        .collect()
}

/// Find an effect that takes stereo in and out.
fn find_stereo_effect() -> Option<(PathBuf, Cid)> {
    for path in candidates().into_iter().take(SEARCH_LIMIT) {
        let Ok(module) = Module::open(&path) else {
            continue;
        };
        let Ok(classes) = module.audio_modules() else {
            continue;
        };
        for class in classes {
            if class.is_instrument() {
                continue;
            }
            let Ok(plugin) = Vst3Plugin::create(&module, class.cid, Arc::new(TestHost)) else {
                continue;
            };
            let (ins, outs) = plugin.bus_channel_counts();
            let param_count = SubPluginMain::params(&plugin).len();
            drop(plugin);
            if ins == 2 && outs == 2 && param_count <= MAX_PARAMS_FOR_PROBE {
                return Some((path, class.cid));
            }
        }
    }
    None
}

#[test]
fn vst3_lifecycle_against_installed_plugins() {
    let Some((path, cid)) = find_stereo_effect() else {
        eprintln!("no stereo VST3 effect installed; skipping");
        return;
    };
    let module = Module::open(&path).expect("reopen module");
    let class = module
        .audio_modules()
        .expect("enumerate")
        .into_iter()
        .find(|c| c.cid == cid)
        .expect("class still present");
    eprintln!("testing against {}", class.name);

    repeated_activation(&module, &class);
    state_round_trips_into_a_fresh_instance(&module, &class);
    truncated_state_is_rejected(&module, &class);
    parameters_report_usable_ranges(&module, &class);
}

/// Repeated activation verifies that module-level and instance-level state
/// are properly separated and can be created, activated, and dropped repeatedly.
fn repeated_activation(module: &Module, class: &ClassInfo) {
    let host = Arc::new(TestHost);
    for round in 0..3 {
        let mut plugin = Vst3Plugin::create(module, class.cid, host.clone())
            .unwrap_or_else(|e| panic!("round {round}: {e}"));
        let processor = plugin
            .activate(AudioConfig {
                offline: true,
                ..Default::default()
            })
            .unwrap_or_else(|e| panic!("round {round}: activate: {e}"));
        plugin.deactivate(processor);
    }
}

fn state_round_trips_into_a_fresh_instance(module: &Module, class: &ClassInfo) {
    let host = Arc::new(TestHost);
    let probe = Vst3Plugin::create(module, class.cid, host.clone()).expect("create");
    let candidates: Vec<_> = SubPluginMain::params(&probe)
        .iter()
        .filter(|p| {
            p.flags.contains(ParamFlags::AUTOMATABLE)
                && !p.flags.contains(ParamFlags::BYPASS)
                && !p.flags.contains(ParamFlags::READONLY)
                && p.max != p.min
        })
        .cloned()
        .collect();
    drop(probe);

    if candidates.is_empty() {
        eprintln!(
            "{} exposes no writable parameter; skipping state check",
            class.name
        );
        return;
    }

    let mut rejected = Vec::new();
    for target in candidates.iter().take(CANDIDATES) {
        match try_round_trip(module, class, host.clone(), target) {
            Ok(()) => return,
            Err(why) => rejected.push(format!("{}: {why}", target.name)),
        }
    }

    panic!(
        "{}: no parameter round-tripped:
  {}",
        class.name,
        rejected.join(
            "
  "
        )
    );
}

fn try_round_trip(
    module: &Module,
    class: &ClassInfo,
    host: Arc<TestHost>,
    target: &plugin_host_api::ParamInfo,
) -> Result<(), String> {
    let mut first =
        Vst3Plugin::create(module, class.cid, host.clone()).map_err(|e| e.to_string())?;

    let before = first.snapshot().get(target.id).unwrap_or(target.default);
    // Move to whichever end is further away, so the check cannot pass because
    // the value happened to be there already.
    let wanted = if (before - target.min).abs() > (before - target.max).abs() {
        target.min
    } else {
        target.max
    };
    first
        .set_param(target.id, wanted)
        .map_err(|e| e.to_string())?;

    // Process a block before saving to ensure pending parameter edits in the
    // controller are propagated to the processor before state serialization.
    run_one_block(&mut first)?;

    let saved = first.snapshot().get(target.id).unwrap_or(f64::NAN);
    let span = (target.max - target.min).abs().max(1e-9);
    if (saved - wanted).abs() > span * 1e-3 {
        return Err(format!(
            "write did not stick (asked {wanted}, reads {saved})"
        ));
    }

    let blob = first.save_state().map_err(|e| e.to_string())?;
    drop(first);
    if blob.is_empty() {
        return Err("state blob was empty".into());
    }

    let mut second = Vst3Plugin::create(module, class.cid, host).map_err(|e| e.to_string())?;
    second.load_state(&blob).map_err(|e| e.to_string())?;
    let restored = second.snapshot().get(target.id).unwrap_or(f64::NAN);

    if (restored - saved).abs() > span * 1e-6 {
        return Err(format!("restored {restored}, expected {saved}"));
    }
    Ok(())
}

/// Handing a plugin a half-read chunk is how a corrupted project turns into a
/// crash, so the length prefixes are validated before anything is passed on.
fn truncated_state_is_rejected(module: &Module, class: &ClassInfo) {
    let mut plugin = Vst3Plugin::create(module, class.cid, Arc::new(TestHost)).expect("create");
    assert!(plugin.load_state(&[]).is_err());
    assert!(plugin.load_state(&[1, 2, 3]).is_err());
    assert!(plugin.load_state(&[255, 255, 0, 0, 0, 0, 0, 0]).is_err());
}

/// Activate, process a single silent block, deactivate.
fn run_one_block(plugin: &mut Vst3Plugin) -> Result<(), String> {
    use plugin_host_api::{AudioBuffers, BufferLayout, EventSink, TimeContext};

    let config = AudioConfig {
        offline: true,
        ..Default::default()
    };
    let frames = 64u32;
    let mut processor = plugin.activate(config).map_err(|e| e.to_string())?;

    let input = vec![0.0f32; (config.input_channels * frames) as usize];
    let mut output = vec![0.0f32; (config.output_channels * frames) as usize];
    let mut buffers = AudioBuffers::new(
        &input,
        &mut output,
        config.input_channels,
        config.output_channels,
        frames,
        BufferLayout::Planar,
    );
    let mut sink = EventSink::new();
    processor.process(&mut buffers, &[], &TimeContext::default(), &mut sink);

    plugin.deactivate(processor);
    Ok(())
}

fn parameters_report_usable_ranges(module: &Module, class: &ClassInfo) {
    let plugin = Vst3Plugin::create(module, class.cid, Arc::new(TestHost)).expect("create");
    for p in SubPluginMain::params(&plugin) {
        assert!(
            p.min.is_finite() && p.max.is_finite(),
            "{}: non-finite range",
            p.name
        );
        assert!(p.default.is_finite(), "{}: non-finite default", p.name);
        // A stepped parameter with an empty range cannot be addressed at all.
        if p.flags.contains(ParamFlags::STEPPED) {
            assert!(
                p.max > p.min,
                "{}: stepped parameter with empty range",
                p.name
            );
        }
    }
}
