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
                                    play a note into an instrument"
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
