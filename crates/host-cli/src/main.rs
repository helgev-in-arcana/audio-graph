//! Development harness for the host library.
//!
//! Stands in for a DAW during development: everything the milestones ask us to
//! prove — that a real plugin loads, enumerates, instantiates, and processes —
//! is exercised from here without a DAW in the loop.

mod host;
mod render;
mod wav;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use plugin_host_api::SubPluginMain;
use vst3_host::{Module, default_plugin_directories, find_modules};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, rest) = match args.split_first() {
        Some((c, r)) => (c.as_str(), r),
        None => {
            usage();
            return ExitCode::FAILURE;
        }
    };

    let result = match cmd {
        "scan" => cmd_scan(rest),
        "info" => cmd_info(rest),
        "dirs" => cmd_dirs(),
        "churn" => cmd_churn(rest),
        "params" => cmd_params(rest),
        "render" => cmd_render(rest),
        "synth" => cmd_synth(rest),
        "state" => cmd_state(rest),
        "twice" => cmd_twice(rest),
        "sweep" => cmd_sweep(rest),
        "probe" => cmd_probe(rest),
        "nest" => cmd_nest(rest),
        "graph" => cmd_graph(rest),
        "gui" => cmd_gui(rest),
        "automate" => cmd_automate(rest),
        "bundle" => cmd_bundle(rest),
        _ => {
            usage();
            return ExitCode::FAILURE;
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    eprintln!(
        "usage:
  host-cli dirs                     list the conventional VST3 directories
  host-cli scan [DIR...]            load every module found and list its classes
  host-cli info <PATH.vst3>         detail one module
  host-cli churn <PATH.vst3> [N]    load/unload N times (default 1000)
  host-cli params <PATH.vst3> [CID] instantiate and list parameters
  host-cli render <PATH.vst3> <IN.wav> <OUT.wav> [CID]
                                    run audio through the plugin
  host-cli synth <PATH.vst3> <OUT.wav> [CID]
                                    play a note into an instrument
  host-cli sweep [DIR...]           lifecycle-test every plugin, one child process each
  host-cli probe <PATH.vst3>        lifecycle-test one module in this process
  host-cli nest <WRAPPER.vst3> [CID]
                                    check the wrapper reloads its sub-plugin from state
  host-cli graph <WRAPPER.vst3> <IN.wav> [RATE_HZ]
                                    check an LFO in the node graph reaches the sub-plugin
                                    (set AUDIO_GRAPH_SUB and AUDIO_GRAPH_SUB_BIND first)
  host-cli gui <PATH.vst3> [CID [SECONDS]] [--reverse]
                                    open a plugin's editor and tear it down
  host-cli twice <PATH.vst3> [N]    instantiate N times in sequence
  host-cli state <PATH.vst3> [CID]  save/restore a parameter across instances
  host-cli automate <PATH.vst3> <IN.wav> [CID [PARAM_ID]]
                                    check a mid-block parameter change lands
  host-cli bundle <DLL> <OUT.vst3>  wrap a built cdylib as a VST3 bundle"
    );
}

/// Package a built cdylib as a VST3 bundle.
///
/// A bare `.dll` renamed to `.vst3` loads in some hosts and not others; the
/// bundle layout is what the format actually specifies, so the wrapper is
/// tested in the shape a DAW will meet it in. This exists here rather than in a
/// build script because it is only ever wanted deliberately.
fn cmd_bundle(args: &[String]) -> Result<(), String> {
    let dll = args.first().ok_or("usage: bundle <DLL> <OUT.vst3>")?;
    let out = args.get(1).ok_or("usage: bundle <DLL> <OUT.vst3>")?;
    let out = Path::new(out);

    let stem = out
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("the output needs a name ending in .vst3")?;
    let arch = if cfg!(target_arch = "x86_64") {
        "x86_64-win"
    } else {
        "arm64-win"
    };
    let contents = out.join("Contents").join(arch);

    // Replaced wholesale: a stale binary next to a fresh one is the kind of
    // thing that costs an hour to notice.
    if out.exists() {
        std::fs::remove_dir_all(out).map_err(|e| format!("clearing {}: {e}", out.display()))?;
    }
    std::fs::create_dir_all(&contents).map_err(|e| e.to_string())?;
    let target = contents.join(format!("{stem}.vst3"));
    std::fs::copy(dll, &target).map_err(|e| format!("copying {dll}: {e}"))?;

    println!("{}", out.display());
    Ok(())
}

fn cmd_dirs() -> Result<(), String> {
    for d in default_plugin_directories() {
        println!("{}", d.display());
    }
    Ok(())
}

/// Resolve the paths to scan: explicit arguments, or the OS-conventional
/// directories when none are given.
fn modules_from_args(args: &[String]) -> Vec<PathBuf> {
    // Flags are not paths.
    let args: Vec<String> = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .collect();
    let dirs: Vec<PathBuf> = if args.is_empty() {
        default_plugin_directories()
    } else {
        args.iter().map(PathBuf::from).collect()
    };

    let mut out = Vec::new();
    for d in dirs {
        if d.extension().is_some_and(|e| e == "vst3") {
            out.push(d);
        } else {
            out.extend(find_modules(&d));
        }
    }
    out
}

fn cmd_scan(args: &[String]) -> Result<(), String> {
    let modules = modules_from_args(args);
    if modules.is_empty() {
        return Err("no .vst3 modules found".into());
    }

    let mut loaded = 0usize;
    let mut classes = 0usize;
    let mut failed = 0usize;

    for path in &modules {
        // A scan must survive a broken or foreign-architecture plugin: report
        // and continue, never abort the sweep.
        match Module::open(path) {
            Ok(module) => {
                loaded += 1;
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                println!("\n{name}");
                if let Ok(info) = module.factory_info() {
                    println!("  vendor: {}", info.vendor);
                }
                match module.audio_modules() {
                    Ok(list) => {
                        for c in &list {
                            classes += 1;
                            let kind = if c.is_instrument() {
                                "instrument"
                            } else {
                                "fx"
                            };
                            println!("  [{kind}] {} ({})", c.name, c.subcategories);
                            println!("        cid {} v{}", c.cid, c.version);
                        }
                        if list.is_empty() {
                            println!("  (no audio module classes)");
                        }
                    }
                    Err(e) => println!("  class enumeration failed: {e}"),
                }
            }
            Err(e) => {
                failed += 1;
                println!("\n{}\n  FAILED: {e}", path.display());
            }
        }
    }

    println!(
        "\n{} module(s): {loaded} loaded, {failed} failed, {classes} audio module class(es)",
        modules.len()
    );
    Ok(())
}

fn cmd_info(args: &[String]) -> Result<(), String> {
    let path = args.first().ok_or("expected a path")?;
    let module = Module::open(path).map_err(|e| e.to_string())?;

    let info = module.factory_info().map_err(|e| e.to_string())?;
    println!("path:    {}", module.path().display());
    println!("vendor:  {}", info.vendor);
    println!("url:     {}", info.url);
    println!("email:   {}", info.email);
    println!("unicode: {}", info.unicode);

    match module.module_info() {
        Some(mi) => println!("moduleinfo.json: {} classes declared", mi.classes.len()),
        None => println!("moduleinfo.json: absent"),
    }

    println!("\nclasses:");
    for c in module.classes().map_err(|e| e.to_string())? {
        println!("  {} — {}", c.name, c.category);
        println!("    cid  {}", c.cid);
        if !c.subcategories.is_empty() {
            println!("    tags {}", c.subcategories);
        }
        if !c.version.is_empty() {
            println!("    ver  {} (sdk {})", c.version, c.sdk_version);
        }
    }
    Ok(())
}

/// M0's leak check: repeated load/unload must not grow or crash.
fn cmd_churn(args: &[String]) -> Result<(), String> {
    let path = args.first().ok_or("expected a path")?;
    let iterations: usize = args
        .get(1)
        .map_or(Ok(1000), |s| s.parse())
        .map_err(|_| "bad count")?;
    let path = Path::new(path);

    for i in 0..iterations {
        let module = Module::open(path).map_err(|e| format!("iteration {i}: {e}"))?;
        let classes = module
            .classes()
            .map_err(|e| format!("iteration {i}: {e}"))?;
        if classes.is_empty() {
            return Err(format!("iteration {i}: no classes"));
        }
        drop(module);
    }

    println!("{iterations} load/unload cycles completed");
    Ok(())
}

/// M2's parameter surface, seen from outside: the list, the ranges, and the
/// plugin's own formatting of each current value.
fn cmd_params(args: &[String]) -> Result<(), String> {
    use std::sync::Arc;
    use vst3_host::Vst3Plugin;

    let path = args.first().ok_or("expected a path")?;
    let module = Module::open(path).map_err(|e| e.to_string())?;
    let class = render::choose_class(&module, args.get(1).map(String::as_str))?;

    let host = Arc::new(host::CliHost::new());
    let plugin = Vst3Plugin::create(&module, class.cid, host).map_err(|e| e.to_string())?;

    let (ins, outs) = plugin.bus_channel_counts();
    println!("{} [{}]", class.name, class.subcategories);
    println!("buses: {ins} in / {outs} out");
    println!("capabilities: {:?}", plugin.capabilities());

    let snapshot = plugin.snapshot();
    let params = SubPluginMain::params(&plugin);
    println!("{} parameters", params.len());
    for p in params.iter().take(40) {
        let current = snapshot.get(p.id).unwrap_or(p.default);
        let text = plugin.param_to_text(p.id, current).unwrap_or_default();
        // The module is the VST3 unit the parameter sits in — the tree a DAW
        // shows when picking an automation lane. Printed because an accidental
        // extra level there is invisible from the parameter list alone (§8.1).
        let module = if p.module.is_empty() {
            String::new()
        } else {
            format!("  <{}>", p.module)
        };
        println!(
            "  {:>8}  {:<28} {:>12.4} [{:.4} .. {:.4}]  {}{}",
            p.id.0, p.name, current, p.min, p.max, text, module
        );
    }
    if params.len() > 40 {
        println!("  ... {} more", params.len() - 40);
    }
    Ok(())
}

fn cmd_render(args: &[String]) -> Result<(), String> {
    let path = args.first().ok_or("expected a plugin path")?;
    let input_path = args.get(1).ok_or("expected an input wav")?;
    let output_path = args.get(2).ok_or("expected an output wav")?;

    let input = wav::read(Path::new(input_path))?;
    let outcome = render::render(
        Path::new(path),
        args.get(3).map(String::as_str),
        &input,
        512,
        &[],
    )?;

    wav::write(Path::new(output_path), &outcome.audio)?;
    report(&outcome, &input);
    Ok(())
}

fn cmd_synth(args: &[String]) -> Result<(), String> {
    let path = args.first().ok_or("expected a plugin path")?;
    let output_path = args.get(1).ok_or("expected an output wav")?;

    // Three seconds at 48 kHz: long enough that a slow attack or a preset that
    // needs loading does not read as silence.
    let sample_rate = 48_000.0;
    let frames = (sample_rate * 3.0) as usize;
    let input = wav::Audio::silence(sample_rate, 2, frames);
    let events = render::note(
        60,
        (sample_rate * 0.1) as usize,
        (sample_rate * 2.0) as usize,
    );

    let outcome = render::render(
        Path::new(path),
        args.get(2).map(String::as_str),
        &input,
        512,
        &events,
    )?;

    wav::write(Path::new(output_path), &outcome.audio)?;
    report(&outcome, &input);
    if outcome.audio.peak() == 0.0 {
        return Err("instrument produced silence".into());
    }
    Ok(())
}

fn report(outcome: &render::RenderOutcome, input: &wav::Audio) {
    println!("blocks:  {}", outcome.blocks);
    println!("latency: {} samples", outcome.latency);
    println!("input:   peak {:.4}  rms {:.4}", input.peak(), input.rms());
    println!(
        "output:  peak {:.4}  rms {:.4}",
        outcome.audio.peak(),
        outcome.audio.rms()
    );
    for line in &outcome.host_log {
        println!("host:    {line}");
    }
}

/// M2's state DoD: change a parameter, save, restore into a *fresh* instance,
/// and confirm the value survived.
///
/// Several parameters are tried before giving up. A single failure says
/// nothing: plugins refuse writes to some parameters, and recompute others
/// from the transport on every block.
fn cmd_state(args: &[String]) -> Result<(), String> {
    use std::sync::Arc;
    use vst3_host::Vst3Plugin;

    const CANDIDATES: usize = 8;

    let path = args.first().ok_or("expected a path")?;
    let module = Module::open(path).map_err(|e| e.to_string())?;
    let class = render::choose_class(&module, args.get(1).map(String::as_str))?;
    let host = Arc::new(host::CliHost::new());

    let probe = Vst3Plugin::create(&module, class.cid, host.clone()).map_err(|e| e.to_string())?;
    let candidates = writable_params(&probe);
    drop(probe);
    if candidates.is_empty() {
        return Err("plugin has no writable parameter".into());
    }

    let mut rejected = Vec::new();
    for target in candidates.iter().take(CANDIDATES) {
        match state_round_trip(&module, &class, host.clone(), target) {
            Ok(report) => {
                println!("parameter: {} ({})", target.name, target.id.0);
                println!("{report}");
                println!("state round-trip OK");
                return Ok(());
            }
            Err(why) => rejected.push(format!("{} ({}): {why}", target.name, target.id.0)),
        }
    }

    Err(format!(
        "no parameter round-tripped; tried:
  {}",
        rejected.join(
            "
  "
        )
    ))
}

/// Set a parameter, run a block so the processor hears it, save, and restore
/// into a fresh instance.
fn state_round_trip(
    module: &Module,
    class: &vst3_host::ClassInfo,
    host: std::sync::Arc<host::CliHost>,
    target: &plugin_host_api::ParamInfo,
) -> Result<String, String> {
    use vst3_host::Vst3Plugin;

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

    // VST3 keeps the processor and the controller apart, and an edit only
    // reaches the processor through the change list in `process`. Saving
    // without a block captures what the processor still believed, which is how
    // a preset silently comes back wrong.
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

    let mut second = Vst3Plugin::create(module, class.cid, host).map_err(|e| e.to_string())?;
    let fresh = second.snapshot().get(target.id).unwrap_or(f64::NAN);
    second.load_state(&blob).map_err(|e| e.to_string())?;
    let restored = second.snapshot().get(target.id).unwrap_or(f64::NAN);

    if (restored - saved).abs() > span * 1e-6 {
        return Err(format!("restored {restored}, expected {saved}"));
    }

    Ok(format!(
        "  initial   {before}
  set to    {wanted}
  state     {} bytes
  fresh     {fresh}
  restored  {restored}",
        blob.len()
    ))
}

/// Activate, process one silent block, deactivate.
fn run_one_block(plugin: &mut vst3_host::Vst3Plugin) -> Result<(), String> {
    use plugin_host_api::{AudioBuffers, AudioConfig, BufferLayout, EventSink, TimeContext};

    let (ins, outs) = plugin.bus_channel_counts();
    let config = AudioConfig {
        sample_rate: 48_000.0,
        max_block_size: 512,
        input_channels: ins.min(2),
        output_channels: if outs == 0 { 2 } else { outs.min(2) },
        offline: true,
    };
    let frames = 64u32;
    let mut processor = plugin.activate(config).map_err(|e| e.to_string())?;

    let input = vec![0.0f32; (config.input_channels * frames).max(1) as usize];
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

/// Parameters that are safe to move: automatable, not bypass, not read-only,
/// and with a range wide enough for the change to be observable.
///
/// Returns a list rather than one pick, because the first candidate is often a
/// poor probe — a plugin may refuse the write outright, or the parameter may be
/// one the plugin recomputes from the transport (a host-synced tempo, say) and
/// therefore overwrites on the next block. Neither is a host fault, so the
/// caller works down the list.
fn writable_params(plugin: &vst3_host::Vst3Plugin) -> Vec<plugin_host_api::ParamInfo> {
    use plugin_host_api::ParamFlags;
    SubPluginMain::params(plugin)
        .iter()
        .filter(|p| {
            p.flags.contains(ParamFlags::AUTOMATABLE)
                && !p.flags.contains(ParamFlags::BYPASS)
                && !p.flags.contains(ParamFlags::READONLY)
                && (p.max - p.min).abs() > 0.0
        })
        .cloned()
        .collect()
}

fn pick_writable_param(plugin: &vst3_host::Vst3Plugin) -> Option<plugin_host_api::ParamInfo> {
    writable_params(plugin).into_iter().next()
}

/// M2's sample-accuracy DoD: a parameter change carrying a `sample_offset`
/// must take effect at that sample, not at the block boundary.
///
/// Rendering twice and diffing is the only way to see this from outside: the
/// plugin is a black box, so the evidence is *where* the two renders diverge.
fn cmd_automate(args: &[String]) -> Result<(), String> {
    use plugin_host_api::{Event, ParamEvent, Target};
    use std::sync::Arc;
    use vst3_host::Vst3Plugin;

    let path = args.first().ok_or("expected a plugin path")?;
    let input_path = args.get(1).ok_or("expected an input wav")?;
    let cid = args.get(2).map(String::as_str);
    // The auto-picked parameter is often a mode switch that does not alter a
    // steady tone at all, so an explicit id is worth having.
    let wanted_id: Option<u32> = args.get(3).and_then(|s| s.parse().ok());

    let input = wav::read(Path::new(input_path))?;
    let module = Module::open(path).map_err(|e| e.to_string())?;
    let class = render::choose_class(&module, cid)?;
    let probe = Vst3Plugin::create(&module, class.cid, Arc::new(host::CliHost::new()))
        .map_err(|e| e.to_string())?;
    let target = match wanted_id {
        Some(id) => SubPluginMain::params(&probe)
            .iter()
            .find(|p| p.id.0 == id)
            .cloned()
            .ok_or_else(|| format!("no parameter with id {id}"))?,
        None => pick_writable_param(&probe).ok_or("plugin has no writable parameter")?,
    };
    drop(probe);

    // Deliberately not on a block boundary: with 512-sample blocks this lands
    // 488 samples into the second block, so a host that ignored sample_offset
    // would diverge at 512 instead.
    const BLOCK: u32 = 512;
    const SWITCH_AT: usize = 1000;

    let hold = |value: f64| {
        vec![(
            0usize,
            Event::Param(ParamEvent::SetValue {
                id: target.id,
                target: Target::Global,
                value,
                sample_offset: 0,
            }),
        )]
    };

    let baseline = render::render(Path::new(path), cid, &input, BLOCK, &hold(target.min))?;

    let mut stepped = hold(target.min);
    stepped.push((
        SWITCH_AT,
        Event::Param(ParamEvent::SetValue {
            id: target.id,
            target: Target::Global,
            value: target.max,
            sample_offset: 0,
        }),
    ));
    let changed = render::render(Path::new(path), cid, &input, BLOCK, &stepped)?;

    let divergence = (0..baseline.audio.frames).find(|&i| {
        (0..baseline.audio.channels)
            .any(|ch| baseline.audio.channel(ch)[i] != changed.audio.channel(ch)[i])
    });

    let block_start = SWITCH_AT / BLOCK as usize * BLOCK as usize;

    println!("parameter: {} ({})", target.name, target.id.0);
    println!(
        "  swept {} -> {} at sample {SWITCH_AT} (block starts at {block_start})",
        target.min, target.max
    );
    match divergence {
        Some(at) => {
            println!("  renders first differ at sample {at}");
            // What this can and cannot show: the offset we *submit* is checked
            // by unit tests, because from out here a plugin that ramps from the
            // block start and one that ignored the offset look identical.
            if at == SWITCH_AT {
                println!("the plugin acted exactly on the requested sample");
            } else if at == block_start {
                println!(
                    "the plugin acted at the block boundary — normal for a plugin that 
                     applies its parameter queue at block start; it does not indicate 
                     that the offset was dropped"
                );
            } else if at < SWITCH_AT {
                println!("the plugin acted early (look-ahead or internal latency)");
            } else {
                println!(
                    "the plugin acted {} samples late (parameter smoothing)",
                    at - SWITCH_AT
                );
            }
            Ok(())
        }
        None => Err(format!(
            "the parameter change had no audible effect; \
             either {} does not alter this signal or sample_offset was ignored",
            target.name
        )),
    }
}

/// Instantiate the same class N times from one module, sequentially.
///
/// A DAW does this constantly (add a plug-in, remove it, add it again), and it
/// is where per-instance/per-module lifetime confusion shows up.
fn cmd_twice(args: &[String]) -> Result<(), String> {
    use std::sync::Arc;
    use vst3_host::Vst3Plugin;

    let path = args.first().ok_or("expected a path")?;
    let count: usize = args
        .get(1)
        .map_or(Ok(3), |s| s.parse())
        .map_err(|_| "bad count")?;
    let module = Module::open(path).map_err(|e| e.to_string())?;
    let class = render::choose_class(&module, None)?;
    let host = Arc::new(host::CliHost::new());

    for i in 0..count {
        eprintln!("[instance {i}] creating");
        let plugin = Vst3Plugin::create(&module, class.cid, host.clone())
            .map_err(|e| format!("instance {i}: {e}"))?;
        eprintln!(
            "[instance {i}] {} params",
            SubPluginMain::params(&plugin).len()
        );
        drop(plugin);
        eprintln!("[instance {i}] dropped");
    }
    println!("{count} sequential instantiations OK");
    Ok(())
}

/// Lifecycle-test one module: instantiate, activate, deactivate, twice.
///
/// Creating *after* destroying is the case that catches module-scoped state
/// mistaken for per-instance state, so it is the shape the probe uses.
fn cmd_probe(args: &[String]) -> Result<(), String> {
    use std::sync::Arc;
    use vst3_host::Vst3Plugin;

    let path = args.first().ok_or("expected a path")?;

    // Enumerate, then let the module go before doing anything else. Holding a
    // second handle open across the editor probe is not something a host would
    // do, and TH3 faults when we do it.
    let classes = {
        let module = Module::open(path).map_err(|e| e.to_string())?;
        module.audio_modules().map_err(|e| e.to_string())?
    };
    if classes.is_empty() {
        return Err("no audio module classes".into());
    }

    // Editors are opened only on request: it takes seconds per plugin, and
    // the windows steal focus. One teardown order per run, so a plugin that
    // survives one and not the other says so precisely.
    let gui_order = if args.iter().any(|a| a == "--gui-reverse") {
        Some(true)
    } else if args.iter().any(|a| a == "--gui") {
        Some(false)
    } else {
        None
    };
    if let Some(reverse) = gui_order {
        for class in &classes {
            probe_editor(path, class.cid, &class.name, reverse)?;
        }
    }

    let module = Module::open(path).map_err(|e| e.to_string())?;
    for class in classes {
        let host = Arc::new(host::CliHost::new());
        for round in 0..2 {
            let mut plugin = Vst3Plugin::create(&module, class.cid, host.clone())
                .map_err(|e| format!("{}: round {round}: {e}", class.name))?;
            let params = SubPluginMain::params(&plugin).len();
            let (ins, outs) = plugin.bus_channel_counts();
            let config = plugin_host_api::AudioConfig {
                sample_rate: 48_000.0,
                max_block_size: 512,
                input_channels: ins.min(2),
                output_channels: if outs == 0 { 2 } else { outs.min(2) },
                offline: true,
            };
            let processor = plugin
                .activate(config)
                .map_err(|e| format!("{}: activate: {e}", class.name))?;
            plugin.deactivate(processor);
            if round == 1 {
                println!("{} | {params} params | {ins}->{outs}", class.name);
            }
        }
    }
    Ok(())
}

/// Open and tear down one plugin's editor, both ways round (§5.3).
///
/// The second order is the one that matters: some DAWs terminate a plugin
/// without ever sending a close notification, so correctness cannot depend on
/// a caller remembering to close the editor first.
fn probe_editor(path: &str, cid: vst3_host::Cid, name: &str, reverse: bool) -> Result<(), String> {
    use std::sync::Arc;
    use subhost_adapter::SubHost;

    let mut sub = SubHost::new(Arc::new(host::CliHost::new()));
    sub.load(Path::new(path), Some(cid))?;
    match sub.open_editor(std::ptr::null_mut()) {
        Ok(()) => {}
        // A plugin with no editor is not a failure; plenty have none.
        Err(e) if e.contains("no editor") => return Ok(()),
        Err(e) => return Err(format!("{name}: open editor: {e}")),
    }

    // Long enough for the plugin to finish its first paint, which is when a
    // badly attached editor tends to fault.
    for _ in 0..20 {
        vst3_host_view::pump_events();
        sub.tick_editor();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    if reverse {
        // The whole instance goes away with the editor still open, which is
        // what a DAW does when it terminates a plugin without a close notice.
        drop(sub);
    } else {
        sub.close_editor();
        drop(sub);
    }
    vst3_host_view::pump_events();

    let order = if reverse {
        "instance dropped with editor open"
    } else {
        "editor closed first"
    };
    println!("{name} | editor ok ({order})");
    Ok(())
}

/// The standing regression sweep: every installed plugin through the lifecycle
/// a DAW would put it through.
///
/// Each module is probed in a *child process*. A third-party plugin that
/// corrupts its own heap on teardown would otherwise take the whole sweep with
/// it, and losing the results for the other fifty is not an acceptable way to
/// learn that one of them is broken. This is also a small preview of why
/// ADR-6's out-of-process backend exists.
fn cmd_sweep(args: &[String]) -> Result<(), String> {
    use std::process::Command;

    let modules = modules_from_args(args);
    if modules.is_empty() {
        return Err("no .vst3 modules found".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;

    let mut ok = 0usize;
    let mut problems = Vec::new();

    for path in &modules {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        // Each requested mode is its own child process, so a plugin that
        // crashes in one still reports its result for the others.
        let mut modes: Vec<Vec<String>> = vec![Vec::new()];
        if args.iter().any(|a| a == "--gui") {
            modes = vec![
                vec!["--gui".into()],
                vec!["--gui-reverse".into()],
                Vec::new(),
            ];
        }

        let mut all_ok = true;
        for mode in &modes {
            let output = Command::new(&exe)
                .arg("probe")
                .arg(path)
                .args(mode)
                .output()
                .map_err(|e| format!("could not spawn probe: {e}"))?;
            if output.status.success() {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    println!("  ok   {name} | {line}");
                }
            } else {
                all_ok = false;
                let stderr = String::from_utf8_lossy(&output.stderr);
                let detail = stderr
                    .lines()
                    .find(|l| l.starts_with("error:"))
                    .map(str::to_string)
                    // No error line means it did not get far enough to print
                    // one: the process died, which is the interesting case.
                    .unwrap_or_else(|| format!("crashed ({})", output.status));
                let label = mode.first().map_or("lifecycle", |m| m.as_str());
                problems.push(format!("{name} [{label}]: {detail}"));
            }
        }
        if all_ok {
            ok += 1;
        }
    }

    println!(
        "
{ok} module(s) passed, {} problem(s)",
        problems.len()
    );
    for p in &problems {
        println!("  !! {p}");
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err("sweep found problems".into())
    }
}

/// M3's project-reopen check: does the wrapper's state carry its sub-plugin?
///
/// Saves state from an instance that has a sub-plugin loaded, then restores it
/// into a fresh one and confirms the sub-plugin came back. This is what a DAW
/// does when a project is closed and opened again, and it is the case section
/// 8.3 exists for.
fn cmd_nest(args: &[String]) -> Result<(), String> {
    use std::sync::Arc;
    use vst3_host::Vst3Plugin;

    let path = args.first().ok_or("expected the wrapper's path")?;
    let module = Module::open(path).map_err(|e| e.to_string())?;
    let class = render::choose_class(&module, args.get(1).map(String::as_str))?;
    let host = Arc::new(host::CliHost::new());

    // The first instance picks up its sub-plugin however it can — in
    // development that is AUDIO_GRAPH_SUB, since there is no editor yet.
    let mut first =
        Vst3Plugin::create(&module, class.cid, host.clone()).map_err(|e| e.to_string())?;
    run_one_block(&mut first)?;
    let blob = first.save_state().map_err(|e| e.to_string())?;
    drop(first);
    println!("saved {} bytes of wrapper state", blob.len());

    let text = String::from_utf8_lossy(&blob);
    let names: Vec<&str> = text
        .match_indices("display_name")
        .filter_map(|(i, _)| text.get(i..i + 96))
        .collect();
    if names.is_empty() {
        return Err("no sub-plugin reference in the saved state;                     set AUDIO_GRAPH_SUB so the first instance has one to save".into());
    }
    println!("state references: {}", names[0].escape_debug());

    let mut second = Vst3Plugin::create(&module, class.cid, host).map_err(|e| e.to_string())?;
    second.load_state(&blob).map_err(|e| e.to_string())?;
    run_one_block(&mut second)?;
    let restored = second.save_state().map_err(|e| e.to_string())?;

    let restored_text = String::from_utf8_lossy(&restored);
    if !restored_text.contains("display_name") {
        return Err("the restored instance has no sub-plugin; state did not carry it".into());
    }
    println!("sub-plugin reference survived a state round-trip");
    Ok(())
}

/// M5's acceptance check, run without a DAW.
///
/// The claim to demonstrate is "an LFO can wobble a sub-plugin's parameter",
/// and it is not one the unit tests can make: they stop at the compiled program
/// and the events it produces. What they cannot show is that those events, sent
/// through the wrapper, through the nesting layer, into a real commercial
/// plugin, come out the other side as a change in the audio.
///
/// So: render the same input twice through the same wrapper and the same
/// sub-plugin, once with a graph driving the bound slot and once without, and
/// compare. The graph is injected into the wrapper's own saved state, which is
/// exactly the route a project file takes.
///
///   AUDIO_GRAPH_SUB=...\RoughRider3.vst3 AUDIO_GRAPH_SUB_BIND=56 \
///     cargo run -p host-cli -- graph target/AudioGraph.vst3 tone.wav
fn cmd_graph(args: &[String]) -> Result<(), String> {
    use std::sync::Arc;
    use vst3_host::Vst3Plugin;

    let path = args.first().ok_or("expected the wrapper's path")?;
    let input_path = args.get(1).ok_or("expected an input wav")?;
    let rate: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4.0);

    let input = wav::read(Path::new(input_path))?;
    let module = Module::open(path).map_err(|e| e.to_string())?;
    let class = render::choose_class(&module, None)?;

    // One instance, purely to get a state blob with a sub-plugin and a binding
    // in it. In a DAW the user does this with the editor; here it is
    // AUDIO_GRAPH_SUB and AUDIO_GRAPH_SUB_BIND.
    let mut probe = Vst3Plugin::create(&module, class.cid, Arc::new(host::CliHost::new()))
        .map_err(|e| e.to_string())?;
    run_one_block(&mut probe)?;
    let baseline_state = probe.save_state().map_err(|e| e.to_string())?;
    drop(probe);

    let wrapper_state = read_wrapper_state(&baseline_state)?;
    if !wrapper_state.contains("display_name") {
        return Err("the wrapper saved no sub-plugin; set AUDIO_GRAPH_SUB".into());
    }
    if !wrapper_state.contains("param_id") {
        return Err("no slot is bound; set AUDIO_GRAPH_SUB_BIND to a parameter id".into());
    }

    let with_graph = edit_wrapper_state(&baseline_state, &inject_graph(&wrapper_state, rate)?)?;
    println!("graph: a {rate} Hz saw on slot 1, injected into the wrapper's saved state");

    const BLOCK: u32 = 512;
    let plain = render::render_with_state(
        Path::new(path),
        None,
        Some(&baseline_state),
        &input,
        BLOCK,
        &[],
    )?;
    let modulated =
        render::render_with_state(Path::new(path), None, Some(&with_graph), &input, BLOCK, &[])?;

    // Per-block RMS, so a slow modulation shows up as an envelope rather than
    // being averaged away over the whole file.
    let window = (input.sample_rate / 20.0) as usize;
    let plain_envelope = envelope(&plain.audio, window);
    let modulated_envelope = envelope(&modulated.audio, window);

    let deviation = plain_envelope
        .iter()
        .zip(&modulated_envelope)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    let wobble = spread(&modulated_envelope) - spread(&plain_envelope);

    println!("{} windows of {window} samples", plain_envelope.len());
    println!("largest difference from the unmodulated render: {deviation:.6}");
    println!("extra envelope movement introduced by the graph: {wobble:.6}");

    if deviation < 1e-6 {
        return Err(
            "the graph changed nothing. Either the bound parameter does not affect this \
             plugin's output, or the modulation is not reaching it"
                .into(),
        );
    }
    if wobble <= 0.0 {
        println!(
            "note: the output differs but is not more varied. That is what a parameter \
             that shifts the sound without shaping its level looks like."
        );
    }
    println!("the graph reached the sub-plugin");
    Ok(())
}

/// Read the wrapper's own JSON out of a saved state blob.
///
/// Three layers have to be peeled, and none of them belong to us: `vst3-host`
/// frames the component and controller halves with their lengths, the plugin
/// framework wraps each half in its own JSON document, and the wrapper's state
/// is a persisted string field inside that. Searching rather than assuming, so
/// this check does not break the next time one of them changes shape.
fn read_wrapper_state(blob: &[u8]) -> Result<String, String> {
    for half in halves(blob)? {
        if let Some((_, inner)) = locate(half) {
            return Ok(inner);
        }
    }
    Err("no wrapper state found inside the saved blob".into())
}

/// Rewrite the wrapper's JSON wherever it appears, and re-frame the result.
fn edit_wrapper_state(blob: &[u8], edited: &str) -> Result<Vec<u8>, String> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    for half in halves(blob)? {
        out.push(match locate(half) {
            Some((mut document, _)) => {
                document.replace(edited);
                document.emit()
            }
            None => half.to_vec(),
        });
    }

    // The framing `vst3-host` expects: two lengths, then the two halves.
    let mut framed = Vec::with_capacity(8 + out.iter().map(Vec::len).sum::<usize>());
    framed.extend_from_slice(&(out[0].len() as u32).to_le_bytes());
    framed.extend_from_slice(&(out.get(1).map_or(0, Vec::len) as u32).to_le_bytes());
    for half in &out {
        framed.extend_from_slice(half);
    }
    Ok(framed)
}

fn halves(blob: &[u8]) -> Result<Vec<&[u8]>, String> {
    if blob.len() < 8 {
        return Err("the saved state is too short to be framed".into());
    }
    let component = u32::from_le_bytes(blob[0..4].try_into().unwrap()) as usize;
    let controller = u32::from_le_bytes(blob[4..8].try_into().unwrap()) as usize;
    if blob.len() < 8 + component + controller {
        return Err("the saved state is truncated".into());
    }
    Ok(vec![
        &blob[8..8 + component],
        &blob[8 + component..8 + component + controller],
    ])
}

/// One half of the blob, with the wrapper's state found inside it.
struct Document {
    outer: serde_json::Value,
    path: Vec<String>,
    /// Whether the framework stored the string by serialising it again, so what
    /// comes back is a JSON string *containing* a JSON string.
    doubled: bool,
}

impl Document {
    fn replace(&mut self, edited: &str) {
        let stored = if self.doubled {
            serde_json::to_string(edited).expect("a string always serialises")
        } else {
            edited.to_string()
        };
        let mut cursor = &mut self.outer;
        for key in &self.path {
            cursor = &mut cursor[key];
        }
        *cursor = serde_json::Value::String(stored);
    }

    fn emit(&self) -> Vec<u8> {
        self.outer.to_string().into_bytes()
    }
}

fn locate(half: &[u8]) -> Option<(Document, String)> {
    let outer: serde_json::Value = serde_json::from_slice(half).ok()?;
    let mut found = None;
    walk(&outer, &mut Vec::new(), &mut found);
    let (path, stored) = found?;

    let doubled = stored.trim_start().starts_with('"');
    let inner = if doubled {
        serde_json::from_str::<String>(&stored).ok()?
    } else {
        stored
    };
    Some((
        Document {
            outer,
            path,
            doubled,
        },
        inner,
    ))
}

/// Depth-first search for a string that is itself the wrapper's state.
fn walk(
    value: &serde_json::Value,
    path: &mut Vec<String>,
    found: &mut Option<(Vec<String>, String)>,
) {
    if found.is_some() {
        return;
    }
    match value {
        serde_json::Value::String(text) => {
            // Matched on the field names rather than on exact JSON, because how
            // many times the framework has escaped this on the way in is its
            // business and not something worth depending on.
            if text.contains("sub_block") && text.contains("slots") {
                *found = Some((path.clone(), text.clone()));
            }
        }
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                path.push(key.clone());
                walk(child, path, found);
                path.pop();
            }
        }
        _ => {}
    }
}

/// Put an LFO on slot 1, the one `AUDIO_GRAPH_SUB_BIND` bound.
fn inject_graph(state: &str, rate: f64) -> Result<String, String> {
    let mut value: serde_json::Value =
        serde_json::from_str(state).map_err(|e| format!("wrapper state is not JSON: {e}"))?;
    value["graph"] = serde_json::json!({
        "nodes": [
            { "id": 0, "pos": [40.0, 40.0], "kind": { "Lfo": {
                "waveform": "Saw", "rate": { "Hz": rate },
                "phase": 0.0, "depth": 0.5, "offset": 0.5 } } },
            { "id": 1, "pos": [260.0, 40.0], "kind": { "SlotOut": { "slot": 0 } } }
        ],
        "links": [{ "from": 0, "to": 1, "input": 0 }],
        "next_id": 2
    });
    // The finest rate on offer, so a fast LFO is not the thing being measured.
    value["sub_block"] = serde_json::json!(16);
    Ok(value.to_string())
}

/// RMS over consecutive windows, summed across channels.
fn envelope(audio: &wav::Audio, window: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(audio.frames / window.max(1) + 1);
    let mut start = 0;
    while start < audio.frames {
        let end = (start + window).min(audio.frames);
        let mut sum = 0.0f64;
        for ch in 0..audio.channels {
            for &sample in &audio.channel(ch)[start..end] {
                sum += f64::from(sample) * f64::from(sample);
            }
        }
        out.push((sum / ((end - start) * audio.channels as usize) as f64).sqrt() as f32);
        start = end;
    }
    out
}

fn spread(envelope: &[f32]) -> f32 {
    let highest = envelope.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let lowest = envelope.iter().copied().fold(f32::INFINITY, f32::min);
    highest - lowest
}

/// M4's acceptance check, run without a DAW.
///
/// Opens a sub-plugin's editor and tears it down in each of the two orders
/// section 5.3 cares about:
///
///   normal   — the editor is closed, then the instance goes away
///   reverse  — the whole instance is destroyed with the editor still open
///
/// The second is the one that actually breaks hosts. Some DAWs terminate a
/// plugin without ever sending a close notification, so the ordering cannot
/// live in a close path that a caller has to remember to call. It is enforced
/// instead by field order inside `SubHost`, and this command is what proves it:
/// `--reverse` drops the whole thing at once and the editor still tears down
/// first.
fn cmd_gui(args: &[String]) -> Result<(), String> {
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use subhost_adapter::SubHost;

    let path = args.first().ok_or("expected a plugin path")?;
    let cid = args
        .get(1)
        .map(String::as_str)
        .filter(|t| !t.is_empty())
        .and_then(vst3_host::Cid::from_hex);
    let seconds: f64 = args
        .get(2)
        .filter(|s| !s.starts_with("--"))
        .map_or(Ok(1.5), |s| s.parse())
        .map_err(|_| "bad duration")?;
    let reverse = args.iter().any(|a| a == "--reverse");

    let mut sub = SubHost::new(Arc::new(host::CliHost::new()));
    sub.load(Path::new(path), cid)?;
    let name = sub.class().map(|c| c.name.clone()).unwrap_or_default();
    sub.open_editor(std::ptr::null_mut())?;
    println!("opened {name}");

    // Stand in for the DAW's message pump. A plugin would never do this — the
    // DAW is already pumping — but a harness has to.
    let started = Instant::now();
    let deadline = started + Duration::from_secs_f64(seconds);
    while Instant::now() < deadline && sub.editor_is_open() {
        vst3_host_view::pump_events();
        sub.tick_editor();
        std::thread::sleep(Duration::from_millis(16));
    }
    // Which of the two ended it matters: a window that closes itself long
    // before the deadline is the plugin giving up, not the user.
    if sub.editor_is_open() {
        println!("held open for {:.1}s", started.elapsed().as_secs_f64());
    } else {
        println!(
            "the editor closed itself after {:.1}s",
            started.elapsed().as_secs_f64()
        );
    }

    if reverse {
        println!("destroying the whole instance with the editor still open");
        drop(sub);
    } else {
        println!("closing the editor, then unloading");
        sub.close_editor();
        drop(sub);
    }

    // Dispatch anything the plugin posted while tearing down. A bad ordering
    // usually surfaces here rather than at the moment of the mistake.
    vst3_host_view::pump_events();

    println!("teardown completed cleanly");
    Ok(())
}
