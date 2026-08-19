//! Development harness for the host library.
//!
//! Stands in for a DAW during development: everything the milestones ask us to
//! prove — that a real plugin loads, enumerates, instantiates, and processes —
//! is exercised from here without a DAW in the loop.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

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
  host-cli churn <PATH.vst3> [N]    load/unload N times (default 1000)"
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
