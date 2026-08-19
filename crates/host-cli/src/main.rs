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
        "automate" => cmd_automate(rest),
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
  host-cli twice <PATH.vst3> [N]    instantiate N times in sequence
  host-cli state <PATH.vst3> [CID]  save/restore a parameter across instances
  host-cli automate <PATH.vst3> <IN.wav> [CID [PARAM_ID]]
                                    check a mid-block parameter change lands"
    );
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
                            let kind = if c.is_instrument() { "instrument" } else { "fx" };
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
    let iterations: usize = args.get(1).map_or(Ok(1000), |s| s.parse()).map_err(|_| "bad count")?;
    let path = Path::new(path);

    for i in 0..iterations {
        let module = Module::open(path).map_err(|e| format!("iteration {i}: {e}"))?;
        let classes = module.classes().map_err(|e| format!("iteration {i}: {e}"))?;
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
        println!(
            "  {:>8}  {:<28} {:>12.4} [{:.4} .. {:.4}]  {}",
            p.id.0, p.name, current, p.min, p.max, text
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
    let events = render::note(60, (sample_rate * 0.1) as usize, (sample_rate * 2.0) as usize);

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
        rejected.join("
  ")
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

    let mut first = Vst3Plugin::create(module, class.cid, host.clone()).map_err(|e| e.to_string())?;
    let before = first.snapshot().get(target.id).unwrap_or(target.default);

    // Move to whichever end is further away, so the check cannot pass because
    // the value happened to be there already.
    let wanted = if (before - target.min).abs() > (before - target.max).abs() {
        target.min
    } else {
        target.max
    };
    first.set_param(target.id, wanted).map_err(|e| e.to_string())?;

    // VST3 keeps the processor and the controller apart, and an edit only
    // reaches the processor through the change list in `process`. Saving
    // without a block captures what the processor still believed, which is how
    // a preset silently comes back wrong.
    run_one_block(&mut first)?;

    let saved = first.snapshot().get(target.id).unwrap_or(f64::NAN);
    let span = (target.max - target.min).abs().max(1e-9);
    if (saved - wanted).abs() > span * 1e-3 {
        return Err(format!("write did not stick (asked {wanted}, reads {saved})"));
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

fn pick_writable_param(
    plugin: &vst3_host::Vst3Plugin,
) -> Option<plugin_host_api::ParamInfo> {
    writable_params(plugin).into_iter().next()
}

/// M2's sample-accuracy DoD: a parameter change carrying a `sample_offset`
/// must take effect at that sample, not at the block boundary.
///
/// Rendering twice and diffing is the only way to see this from outside: the
/// plugin is a black box, so the evidence is *where* the two renders diverge.
fn cmd_automate(args: &[String]) -> Result<(), String> {
    use std::sync::Arc;
    use plugin_host_api::{Event, ParamEvent, Target};
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

    let hold = |value: f64| vec![(0usize, Event::Param(ParamEvent::SetValue {
        id: target.id,
        target: Target::Global,
        value,
        sample_offset: 0,
    }))];

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
    let count: usize = args.get(1).map_or(Ok(3), |s| s.parse()).map_err(|_| "bad count")?;
    let module = Module::open(path).map_err(|e| e.to_string())?;
    let class = render::choose_class(&module, None)?;
    let host = Arc::new(host::CliHost::new());

    for i in 0..count {
        eprintln!("[instance {i}] creating");
        let plugin = Vst3Plugin::create(&module, class.cid, host.clone())
            .map_err(|e| format!("instance {i}: {e}"))?;
        eprintln!("[instance {i}] {} params", SubPluginMain::params(&plugin).len());
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
    let module = Module::open(path).map_err(|e| e.to_string())?;
    let classes = module.audio_modules().map_err(|e| e.to_string())?;
    if classes.is_empty() {
        return Err("no audio module classes".into());
    }

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
        let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let output = Command::new(&exe)
            .arg("probe")
            .arg(path)
            .output()
            .map_err(|e| format!("could not spawn probe: {e}"))?;

        if output.status.success() {
            ok += 1;
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                println!("  ok   {name} | {line}");
            }
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr
                .lines()
                .find(|l| l.starts_with("error:"))
                .map(str::to_string)
                // No error line means it did not get far enough to print one:
                // the process died, which is the interesting case.
                .unwrap_or_else(|| format!("crashed ({})", output.status));
            problems.push(format!("{name}: {detail}"));
        }
    }

    println!("
{ok} module(s) passed, {} problem(s)", problems.len());
    for p in &problems {
        println!("  !! {p}");
    }
    if problems.is_empty() { Ok(()) } else { Err("sweep found problems".into()) }
}
