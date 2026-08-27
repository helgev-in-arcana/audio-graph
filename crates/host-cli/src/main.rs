//! Development harness for the host library.
//!
//! Stands in for a DAW during development: everything the milestones ask us to
//! prove — that a real plugin loads, enumerates, instantiates, and processes —
//! is exercised from here without a DAW in the loop.

mod fault;
mod host;
mod render;
mod wav;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use audio_graph_engine::{AudioIn, AudioOut, DelayRead, Mix, NodeKind, SlotIn};
use plugin_host::SubPluginMain;
use plugin_host::{Format, Plugin};
use subhost_adapter::SubHostConfig;

/// The wrapper's ceilings, as `audio-graph-plugin` builds them.
///
/// Repeated rather than imported: this crate checks the engine and the adapter
/// without linking the wrapper (and so without egui). Keep in step with
/// `audio_graph_plugin::SUB_HOST`.
const SUB_HOST: SubHostConfig = SubHostConfig {
    max_instances: 16,
    slot_count: 32,
    lanes: 32 + audio_graph_engine::MAX_GRAPH_PARAMS + audio_graph_engine::MAX_AUDIO_LANES,
};

fn main() -> ExitCode {
    fault::install_crash_handler();
    plugin_host::init_thread();

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
        "buses" => cmd_buses(rest),
        "outbus" => cmd_outbus(rest),
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
        "chain" => cmd_chain(rest),
        "instrument" => cmd_instrument(rest),
        "sidechain" => cmd_sidechain(rest),
        "aux" => cmd_aux(rest),
        "delay" => cmd_delay(rest),
        "gui" => cmd_gui(rest),
        "editor" => cmd_editor(rest),
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
  Every PLUGIN argument may be a .vst3 or a .clap; the extension picks the
  backend. ID is that format's own plugin id -- a VST3 class id in hex, or a
  CLAP reverse-DNS name -- as printed by `scan` and `info`.

  host-cli dirs                     list the directories a scan covers, and the
                                    config file they come from -- seeded on a
                                    first run from this OS's conventions, and
                                    editable from the wrapper's editor
  host-cli scan [DIR...]            load every module found and list its plugins
  host-cli info <PLUGIN> [ID]       detail one module, and list the CLAP
                                    extensions or VST3 interfaces one of its
                                    plugins implements
  host-cli churn <PLUGIN> [N]       load/unload N times (default 1000)
  host-cli params <PLUGIN> [ID]     instantiate and list parameters
  host-cli render <PLUGIN> <IN.wav> <OUT.wav> [ID]
                                    run audio through the plugin
  host-cli synth <PLUGIN> <OUT.wav> [ID]
                                    play a note into an instrument
  host-cli sweep [DIR...]           lifecycle-test every plugin, one child process each
  host-cli probe <PLUGIN>           lifecycle-test one module in this process
  host-cli nest <WRAPPER.vst3> [ID]
                                    check the wrapper reloads its sub-plugin from state
  host-cli graph <WRAPPER.vst3> <IN.wav> [RATE_HZ]
                                    check an LFO in the node graph reaches the sub-plugin
                                    (set AUDIO_GRAPH_SUB and AUDIO_GRAPH_SUB_BIND first)
  host-cli chain <WRAPPER.vst3> <IN.wav> <A> <B>
                                    check the graph routing A -> B matches A and B
                                    rendered one after the other
  host-cli buses <PLUGIN> [ID]      list the plugin's buses as the node graph
                                    will see them (§14.2)
  host-cli outbus <WRAPPER.vst3> <PLUGIN> [ID]
                                    render a plugin with more than one output
                                    bus once per output socket, and report what
                                    each socket carries
  host-cli instrument <WRAPPER.vst3> <SYNTH> <A> <B>
                                    check notes reach the instrument the graph
                                    points at, and only that one
  host-cli sidechain <WRAPPER.vst3> <COMP> <SYNTH> <SC_PARAM_ID>
                                    check a compressor inside the graph ducks
                                    against another node's audio
  host-cli aux <WRAPPER.vst3> <PLUGIN> <SYNTH> [ID=VALUE]
                                    weaker form of the above for a plugin with
                                    no sidechain-enable parameter: check that
                                    wiring the aux bus changes the output.
                                    ID=VALUE holds one parameter for the whole
                                    render, for a plugin that reads its aux bus
                                    only in some modes
  host-cli delay <WRAPPER.vst3>     check a feedback delay in the graph sounds the
                                    same at any block size, and that the mix's
                                    gains fade the repeats
  host-cli editor <WRAPPER.vst3> <PLUGIN> [SECONDS]
                                    open the wrapper's editor with a plugin
                                    node already in the patch. Without SECONDS
                                    it stays up until the window is closed
  host-cli gui <PLUGIN> [ID [SECONDS]] [--reverse]
                                    open a plugin's editor and tear it down.
                                    Without SECONDS it stays up until the
                                    window is closed; pass a number, or 0 to
                                    say so in a script
  host-cli twice <PLUGIN> [N]       instantiate N times in sequence
  host-cli state <PLUGIN> [ID]      save/restore a parameter across instances
  host-cli automate <PLUGIN> <IN.wav> [ID [PARAM_ID]]
                                    check a mid-block parameter change lands

Bundling moved to `cargo xtask bundle audio-graph-plugin --release`."
    );
}

/// The directories a scan actually covers, and the file they come from.
///
/// Running this on a machine with no config file writes one, seeded from the
/// conventions — which is the same thing opening the wrapper's editor would do,
/// and is why the list looks conventional the first time.
fn cmd_dirs() -> Result<(), String> {
    // One line per folder, not one per folder and format. Every folder is
    // searched for every format now, so printing the pairs would say the same
    // thing twice and imply a distinction that no longer exists.
    let dirs = plugin_host::config::directories();
    if dirs.is_empty() {
        println!("(no folders; nothing will be scanned)");
    }
    for d in &dirs {
        // A folder can be on a drive that is not plugged in. It stays on the
        // list — it is what the user asked for — and a scan passes over it.
        let note = if d.is_dir() { "" } else { "  (missing)" };
        println!("{}{note}", d.display());
    }
    match plugin_host::config::config_path() {
        Some(path) => println!("\nconfig {}", path.display()),
        None => println!("\nconfig (nowhere to keep one on this platform)"),
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
    if args.is_empty() {
        // Both formats, in the conventional places.
        return plugin_host::installed_modules()
            .into_iter()
            .map(|(_, path)| path)
            .collect();
    }

    let mut out = Vec::new();
    for d in args.iter().map(PathBuf::from) {
        // A path that is itself a module is taken as one; anything else is a
        // directory to search, and it is searched for every format.
        if Format::from_path(&d).is_some() {
            out.push(d);
            continue;
        }
        for format in plugin_host::FORMATS {
            out.extend(plugin_host::find_modules(format, &d));
        }
    }
    out
}

fn cmd_scan(args: &[String]) -> Result<(), String> {
    let modules = modules_from_args(args);
    if modules.is_empty() {
        return Err("no plugin modules found".into());
    }

    let mut loaded = 0usize;
    let mut classes = 0usize;
    let mut failed = 0usize;

    for path in &modules {
        // A scan must survive a broken or foreign-architecture plugin: report
        // and continue, never abort the sweep.
        match plugin_host::scan_module(path) {
            Ok(list) => {
                loaded += 1;
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                println!("\n{name}");
                for c in &list {
                    classes += 1;
                    let kind = if c.is_instrument { "instrument" } else { "fx" };
                    println!("  [{}] [{kind}] {} ({})", c.format, c.name, c.category);
                    println!("        {} v{}  {}", c.id, c.version, c.vendor);
                }
                if list.is_empty() {
                    println!("  (nothing instantiable)");
                }
            }
            Err(e) => {
                failed += 1;
                println!("\n{}\n  FAILED: {e}", path.display());
            }
        }
    }

    println!(
        "\n{} module(s): {loaded} loaded, {failed} failed, {classes} plugin(s)",
        modules.len()
    );
    Ok(())
}

/// What one plugin declares for buses and notes, which is what a plugin node's
/// sockets are built from (§14.2).
///
/// Read before activation, so this is the plugin's default shape rather than a
/// negotiated one. A sidechain socket has to exist before the graph can ask for
/// anything to be connected to it, so the default is the right thing to build
/// sockets from -- and §14.11 re-checks the negotiation at activate.
fn cmd_buses(args: &[String]) -> Result<(), String> {
    use std::sync::Arc;

    let path = args.first().ok_or("expected a plugin path")?;
    let (class, plugin) = render::load(
        Path::new(path),
        args.get(1).map(String::as_str),
        Arc::new(host::CliHost::new()),
    )
    .map_err(|e| e.to_string())?;

    let layout = plugin.io_layout();
    println!("{}", class.name);
    let show = |label: &str, buses: &[plugin_host::BusInfo]| {
        if buses.is_empty() {
            println!("  {label}: none");
            return;
        }
        for (i, bus) in buses.iter().enumerate() {
            let kind = if bus.is_aux { "aux" } else { "main" };
            println!(
                "  {label} {i}: {} ch, {kind}, \"{}\"",
                bus.channels, bus.name
            );
        }
    };
    show("audio in ", &layout.inputs);
    show("audio out", &layout.outputs);
    println!("  notes in:  {}", layout.accepts_notes);
    println!("  notes out: {}", layout.emits_notes);

    // The same thing again, as the graph will show it.
    println!("\nas a plugin node:");
    let ports = audio_graph_engine::PluginPorts::from_layout(&layout, 0);
    let node =
        audio_graph_engine::NodeKind::Plugin(audio_graph_engine::Plugin { instance: 0, ports });
    for port in node.input_ports() {
        println!("  in  {} ({})", port.name, port.ty.label());
    }
    for port in node.output_ports() {
        println!("  out {} ({})", port.name, port.ty.label());
    }
    Ok(())
}

fn cmd_info(args: &[String]) -> Result<(), String> {
    let path = Path::new(args.first().ok_or("expected a path")?);
    let wanted = args.get(1).map(String::as_str);
    let format = Format::from_path(path).ok_or("not a .vst3 or .clap path")?;
    let classes = plugin_host::scan_module(path).map_err(|e| e.to_string())?;

    println!("path:    {}", path.display());
    println!("format:  {format}");

    println!("\nplugins:");
    for c in &classes {
        println!("  {} — {}", c.name, c.vendor);
        println!("    id   {}", c.id);
        if !c.category.is_empty() {
            println!("    tags {}", c.category);
        }
        if !c.version.is_empty() {
            println!("    ver  {}", c.version);
        }
    }

    // Reading the factory says what a module offers; only an instance can say
    // what it implements, because both formats answer that question through the
    // instance (`get_extension` / `queryInterface`). So one plugin is loaded --
    // the one named, or the first -- rather than all of them: a module like
    // Airwindows Consolidated has hundreds of classes, and instantiating every
    // one to print the same list is not worth the wait.
    let (class, plugin) = render::load(path, wanted, std::sync::Arc::new(host::CliHost::new()))?;
    println!(
        "
implemented by {} ({}):",
        class.name, class.id
    );
    if classes.len() > 1 && wanted.is_none() {
        println!(
            "  (first of {} plugins; pass an ID for another)",
            classes.len()
        );
    }
    let names = plugin.format_interfaces();
    if names.is_empty() {
        println!("  (none of the names we know)");
    }
    for name in names {
        println!("  {name}");
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
        // One scan is one module load and unload: the module is opened, its
        // factory read, and the handle dropped before the next round.
        let classes = plugin_host::scan_module(path).map_err(|e| format!("iteration {i}: {e}"))?;
        if classes.is_empty() {
            return Err(format!("iteration {i}: no classes"));
        }
    }

    println!("{iterations} load/unload cycles completed");
    Ok(())
}

/// M2's parameter surface, seen from outside: the list, the ranges, and the
/// plugin's own formatting of each current value.
fn cmd_params(args: &[String]) -> Result<(), String> {
    use std::sync::Arc;

    let path = args.first().ok_or("expected a path")?;
    let class = render::choose_class(Path::new(path), args.get(1).map(String::as_str))?;

    let host = Arc::new(host::CliHost::new());
    let mut plugin =
        Plugin::load(Path::new(path), Some(&class.id), host).map_err(|e| e.to_string())?;
    // A plugin may only be able to answer some of the questions below once it
    // has had a main-thread turn — voice counts are the usual one. The real
    // host ticks every frame; this is the harness catching up with it.
    plugin.tick();

    let (ins, outs) = render::bus_widths(&plugin);
    println!("{} [{}]", class.name, class.category);
    println!("buses: {ins} in / {outs} out");
    println!("capabilities: {:?}", plugin.capabilities());
    // Only CLAP instruments answer this, so most plugins print nothing rather
    // than a line of zeroes.
    if let Some(voices) = SubPluginMain::voice_info(&plugin) {
        println!(
            "voices: {} of {}{}",
            voices.count,
            voices.capacity,
            if voices.overlapping_notes {
                ", overlapping notes"
            } else {
                ""
            }
        );
    }

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
    let twice = args.iter().any(|a| a == "--twice");
    let args: Vec<String> = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .collect();
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

    // `--twice` asks whether a second instance in the *same* process plays the
    // same notes the same way. Several synths randomise oscillator phase from a
    // process-global generator, which makes them perfectly repeatable run to
    // run and different instance to instance -- and therefore useless for any
    // check that compares a graph against a hand-made chain.
    if twice {
        let again = render::render(
            Path::new(path),
            args.get(2).map(String::as_str),
            &input,
            512,
            &events,
        )?;
        let mut worst = 0.0f32;
        for (a, b) in outcome.audio.samples.iter().zip(again.audio.samples.iter()) {
            worst = worst.max((a - b).abs());
        }
        println!("second instance in this process differs by {worst:.9}");
        if worst > 1e-6 {
            println!("=> not usable for a bit-exact graph comparison");
        }
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

    const CANDIDATES: usize = 8;

    let path = args.first().ok_or("expected a path")?;
    let class = render::choose_class(Path::new(path), args.get(1).map(String::as_str))?;
    let host = Arc::new(host::CliHost::new());

    let probe =
        Plugin::load(Path::new(path), Some(&class.id), host.clone()).map_err(|e| e.to_string())?;
    let candidates = writable_params(&probe);
    drop(probe);
    if candidates.is_empty() {
        return Err("plugin has no writable parameter".into());
    }

    let mut rejected = Vec::new();
    for target in candidates.iter().take(CANDIDATES) {
        match state_round_trip(Path::new(path), &class, host.clone(), target) {
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
    path: &Path,
    class: &plugin_host::ClassInfo,
    host: std::sync::Arc<host::CliHost>,
    target: &plugin_host::ParamInfo,
) -> Result<String, String> {
    let mut first = Plugin::load(path, Some(&class.id), host.clone()).map_err(|e| e.to_string())?;
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

    // And a plugin may only fold an edit into what it serialises on one of its
    // own timer callbacks, which run through `tick`. CHOWTapeModel does exactly
    // that: without this line it saves the value it had before the edit, and
    // the preset comes back wrong through no fault of the save/load path. The
    // real host ticks every frame, so this is the harness catching up with it.
    first.tick();

    let saved = first.snapshot().get(target.id).unwrap_or(f64::NAN);
    let span = (target.max - target.min).abs().max(1e-9);
    if (saved - wanted).abs() > span * 1e-3 {
        return Err(format!(
            "write did not stick (asked {wanted}, reads {saved})"
        ));
    }

    let blob = first.save_state().map_err(|e| e.to_string())?;
    drop(first);

    let mut second = Plugin::load(path, Some(&class.id), host).map_err(|e| e.to_string())?;
    let fresh = second.snapshot().get(target.id).unwrap_or(f64::NAN);
    second.load_state(&blob).map_err(|e| e.to_string())?;
    // A plugin may finish loading on a main-thread callback rather than inside
    // `load`, so give it the pump the real host runs every frame before asking
    // what it now believes. Without this, a plugin that defers reads back its
    // old values and looks like it lost the preset.
    second.tick();
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
fn run_one_block(plugin: &mut Plugin) -> Result<(), String> {
    use plugin_host::{AudioBuffers, AudioConfig, BufferLayout, EventSink, TimeContext};

    let (ins, outs) = render::bus_widths(plugin);
    let config = AudioConfig {
        sample_rate: 48_000.0,
        max_block_size: 512,
        input_channels: ins.min(2),
        output_channels: if outs == 0 { 2 } else { outs.min(2) },
        aux_inputs: Default::default(),
        aux_outputs: Default::default(),
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
fn writable_params(plugin: &Plugin) -> Vec<plugin_host::ParamInfo> {
    use plugin_host::ParamFlags;
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

fn pick_writable_param(plugin: &Plugin) -> Option<plugin_host::ParamInfo> {
    writable_params(plugin).into_iter().next()
}

/// M2's sample-accuracy DoD: a parameter change carrying a `sample_offset`
/// must take effect at that sample, not at the block boundary.
///
/// Rendering twice and diffing is the only way to see this from outside: the
/// plugin is a black box, so the evidence is *where* the two renders diverge.
fn cmd_automate(args: &[String]) -> Result<(), String> {
    use plugin_host::{Event, ParamEvent, Target};
    use std::sync::Arc;

    let path = args.first().ok_or("expected a plugin path")?;
    let input_path = args.get(1).ok_or("expected an input wav")?;
    let cid = args.get(2).map(String::as_str);
    // The auto-picked parameter is often a mode switch that does not alter a
    // steady tone at all, so an explicit id is worth having.
    let wanted_id: Option<u32> = args.get(3).and_then(|s| s.parse().ok());

    let input = wav::read(Path::new(input_path))?;
    let (_class, probe) = render::load(Path::new(path), cid, Arc::new(host::CliHost::new()))
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

    let path = args.first().ok_or("expected a path")?;
    let count: usize = args
        .get(1)
        .map_or(Ok(3), |s| s.parse())
        .map_err(|_| "bad count")?;
    let class = render::choose_class(Path::new(path), None)?;
    let host = Arc::new(host::CliHost::new());

    for i in 0..count {
        eprintln!("[instance {i}] creating");
        let plugin = Plugin::load(Path::new(path), Some(&class.id), host.clone())
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

    let path = args.first().ok_or("expected a path")?;

    // Enumerate, then let the module go before doing anything else. Holding a
    // second handle open across the editor probe is not something a host would
    // do, and TH3 faults when we do it.
    let classes = plugin_host::scan_module(Path::new(path)).map_err(|e| e.to_string())?;
    if classes.is_empty() {
        return Err("the module exports nothing instantiable".into());
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
            probe_editor(path, &class.id, &class.name, reverse)?;
        }
    }

    for class in classes {
        let host = Arc::new(host::CliHost::new());
        for round in 0..2 {
            let mut plugin = Plugin::load(Path::new(path), Some(&class.id), host.clone())
                .map_err(|e| format!("{}: round {round}: {e}", class.name))?;
            let params = SubPluginMain::params(&plugin).len();
            let (ins, outs) = render::bus_widths(&plugin);
            let config = plugin_host::AudioConfig {
                sample_rate: 48_000.0,
                max_block_size: 512,
                input_channels: ins.min(2),
                output_channels: if outs == 0 { 2 } else { outs.min(2) },
                aux_inputs: Default::default(),
                aux_outputs: Default::default(),
                offline: true,
            };
            let processor = plugin
                .activate(config)
                .map_err(|e| format!("{}: activate: {e}", class.name))?;
            plugin.deactivate(processor);
            if round == 1 {
                // Voices after activate, not before: an instrument that has no
                // sample rate yet often declines to answer.
                let voices = SubPluginMain::voice_info(&plugin)
                    .map(|v| format!(" | {} of {} voices", v.count, v.capacity))
                    .unwrap_or_default();
                println!("{} | {params} params | {ins}->{outs}{voices}", class.name);
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
fn probe_editor(path: &str, class_id: &str, name: &str, reverse: bool) -> Result<(), String> {
    use std::sync::Arc;
    use subhost_adapter::SubHost;

    let mut sub = SubHost::new(Arc::new(host::CliHost::new()), SUB_HOST);
    sub.load(0, Path::new(path), Some(class_id))?;

    let config = plugin_host::AudioConfig {
        sample_rate: 48_000.0,
        max_block_size: 512,
        input_channels: 2,
        output_channels: 2,
        aux_inputs: Default::default(),
        aux_outputs: Default::default(),
        offline: false,
    };
    let processor = sub.activate(config, &[], &[])?;

    match sub.open_editor(0, std::ptr::null_mut()) {
        Ok(()) => {}
        // A plugin with no editor is not a failure; plenty have none.
        Err(e) if e.contains("no editor") => {
            sub.deactivate(processor);
            return Ok(());
        }
        Err(e) => {
            sub.deactivate(processor);
            return Err(format!("{name}: open editor: {e}"));
        }
    }

    // Long enough for the plugin to finish its first paint, which is when a
    // badly attached editor tends to fault.
    for _ in 0..20 {
        plugin_host::pump_events();
        sub.tick_editors();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    if reverse {
        // The whole instance goes away with the editor still open, which is
        // what a DAW does when it terminates a plugin without a close notice.
        sub.deactivate(processor);
        drop(sub);
    } else {
        sub.close_editor(0);
        sub.deactivate(processor);
        drop(sub);
    }
    plugin_host::pump_events();

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
                let fault_summary = if stderr.contains("NATIVE FAULT CAUGHT") {
                    stderr
                        .lines()
                        .filter(|l| {
                            l.starts_with("Exception:")
                                || l.starts_with("Fault address:")
                                || l.starts_with("Access violation")
                        })
                        .collect::<Vec<_>>()
                        .join(" | ")
                } else {
                    String::new()
                };
                let detail = if !fault_summary.is_empty() {
                    fault_summary
                } else {
                    stderr
                        .lines()
                        .find(|l| l.starts_with("error:"))
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("crashed ({})", output.status))
                };
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

    let path = args.first().ok_or("expected the wrapper's path")?;
    let class = render::choose_class(Path::new(path), args.get(1).map(String::as_str))?;
    let host = Arc::new(host::CliHost::new());

    // The first instance picks up its sub-plugin however it can — in
    // development that is AUDIO_GRAPH_SUB, since there is no editor yet.
    let mut first =
        Plugin::load(Path::new(path), Some(&class.id), host.clone()).map_err(|e| e.to_string())?;
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

    let mut second =
        Plugin::load(Path::new(path), Some(&class.id), host).map_err(|e| e.to_string())?;
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

    let path = args.first().ok_or("expected the wrapper's path")?;
    let input_path = args.get(1).ok_or("expected an input wav")?;
    let rate: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4.0);

    let input = wav::read(Path::new(input_path))?;
    let class = render::choose_class(Path::new(path), None)?;

    // One instance, purely to get a state blob with a sub-plugin and a binding
    // in it. In a DAW the user does this with the editor; here it is
    // AUDIO_GRAPH_SUB and AUDIO_GRAPH_SUB_BIND.
    let mut probe = Plugin::load(
        Path::new(path),
        Some(&class.id),
        Arc::new(host::CliHost::new()),
    )
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
    println!(
        "graph: a {rate} Hz saw on the bound parameter, injected into the wrapper's saved state"
    );

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

/// Put an LFO on the parameter `AUDIO_GRAPH_SUB_BIND` bound.
///
/// The sub-plugin becomes a node in the graph and the LFO drives one of its
/// parameter sockets (§14.12). That used to be a slot binding with a `SlotOut`
/// on the other end; the slot is the DAW's lane, and the graph writing it was
/// the wrapper arguing with the host over who owns the automation.
fn inject_graph(state: &str, rate: f64) -> Result<String, String> {
    let mut value: serde_json::Value =
        serde_json::from_str(state).map_err(|e| format!("wrapper state is not JSON: {e}"))?;

    // The binding the wrapper saved says which parameter to wobble; the
    // reference says which plugin it belongs to.
    let binding = value["slots"]
        .as_array()
        .and_then(|slots| slots.iter().find_map(|s| s["binding"].as_object()))
        .ok_or("no slot is bound; set AUDIO_GRAPH_SUB_BIND to a parameter id")?
        .clone();
    let param_id = binding["param_id"]
        .as_u64()
        .ok_or("the saved binding has no parameter id")?;
    let param_name = binding["param_name"]
        .as_str()
        .unwrap_or("bound")
        .to_string();
    let reference = value["sub_plugin"].clone();
    let path_hint = reference["path_hint"]
        .as_str()
        .ok_or("the saved sub-plugin has no path")?
        .to_string();

    // Ports discovered from the plugin itself (§14.2), so the node has the
    // sockets it really has.
    let ports = {
        use std::sync::Arc;
        let (_, plugin) = render::load(Path::new(&path_hint), None, Arc::new(host::CliHost::new()))
            .map_err(|e| e.to_string())?;
        let mut ports = audio_graph_engine::PluginPorts::from_layout(&plugin.io_layout(), 0);
        ports.params = vec![audio_graph_engine::ParamPort {
            id: param_id as u32,
            name: param_name,
        }];
        serde_json::to_value(&ports).map_err(|e| e.to_string())?
    };
    // The parameter socket sits after the audio inputs and the notes port.
    let param_port = ports["audio_in"].as_array().map_or(0, |a| a.len())
        + usize::from(ports["accepts_notes"].as_bool().unwrap_or(false));

    value["sub_plugins"] = serde_json::json!([{ "instance": 0, "reference": reference }]);
    value["sub_plugin"] = serde_json::Value::Null;
    value["sub_state"] = serde_json::Value::Null;
    value["graph"] = serde_json::json!({
        "nodes": [
            { "id": 0, "pos": [40.0, 40.0],  "kind": { "AudioIn": { "bus": 0, "channels": 2 } } },
            { "id": 1, "pos": [260.0, 40.0], "kind": { "Plugin": { "instance": 0, "ports": ports } } },
            { "id": 2, "pos": [480.0, 40.0], "kind": { "AudioOut": { "bus": 0, "channels": 2 } } },
            { "id": 3, "pos": [40.0, 240.0], "kind": { "Lfo": {
                "waveform": "Saw", "rate": { "Hz": rate },
                "phase": 0.0, "depth": 0.5, "offset": 0.5 } } }
        ],
        "links": [
            { "from": 0, "from_port": 0, "to": 1, "to_port": 0 },
            { "from": 1, "from_port": 0, "to": 2, "to_port": 0 },
            { "from": 3, "from_port": 0, "to": 1, "to_port": param_port }
        ],
        "next_id": 4
    });
    // The finest rate on offer, so a fast LFO is not the thing being measured.
    value["sub_block"] = serde_json::json!(16);
    Ok(value.to_string())
}

/// Two sub-plugins in series through the node graph, against the same two
/// rendered one after the other (ROADMAP M8.2).
///
/// The whole point of M8 is that a graph of plugins sounds like the plugins.
/// Rendering the chain by hand and rendering it through the wrapper has to give
/// the same samples, and "the same" here means bit-for-bit up to the tolerance
/// two identical float paths deserve, not "close enough".
fn cmd_chain(args: &[String]) -> Result<(), String> {
    use std::sync::Arc;

    let wrapper = args.first().ok_or("expected the wrapper's path")?;
    let input_path = args.get(1).ok_or("expected an input wav")?;
    let first = args.get(2).ok_or("expected the first sub-plugin")?;
    let second = args.get(3).ok_or("expected the second sub-plugin")?;

    let input = wav::read(Path::new(input_path))?;
    const BLOCK: u32 = 512;

    // The reference: each plugin rendered on its own, the second fed the
    // first's output. This is what a DAW does with two plugins on a track.
    let stage_one = render::render(Path::new(first), None, &input, BLOCK, &[])?;
    let reference = render::render(Path::new(second), None, &stage_one.audio, BLOCK, &[])?;
    println!(
        "reference: {} -> {} rendered separately",
        Path::new(first)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy(),
        Path::new(second)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    );

    // The wrapper's own saved state, to graft a patch onto.
    let (_class, mut probe) =
        render::load(Path::new(wrapper), None, Arc::new(host::CliHost::new()))
            .map_err(|e| e.to_string())?;
    run_one_block(&mut probe)?;
    let baseline = probe.save_state().map_err(|e| e.to_string())?;
    drop(probe);

    let patched = inject_chain(&read_wrapper_state(&baseline)?, first, second)?;
    let with_chain = edit_wrapper_state(&baseline, &patched)?;
    println!("graph: audio in -> plugin 1 -> plugin 2 -> audio out");

    let through = render::render_with_state(
        Path::new(wrapper),
        None,
        Some(&with_chain),
        &input,
        BLOCK,
        &[],
    )?;

    let frames = reference.audio.frames.min(through.audio.frames);
    let channels = reference.audio.channels.min(through.audio.channels);
    if frames == 0 || channels == 0 {
        return Err("nothing was rendered".into());
    }
    let mut worst = 0.0f32;
    let mut loudest = 0.0f32;
    for ch in 0..channels {
        for i in 0..frames {
            let a = reference.audio.samples[(ch as usize * reference.audio.frames) + i];
            let b = through.audio.samples[(ch as usize * through.audio.frames) + i];
            worst = worst.max((a - b).abs());
            loudest = loudest.max(a.abs());
        }
    }

    println!("{frames} frames x {channels} ch compared");
    println!("reference peak: {loudest:.6}");
    // Two silences match perfectly, which would make everything below pass
    // while proving nothing. This is the check that gives the comparison teeth.
    if loudest < 1e-4 {
        return Err("the reference render is silent; this would compare equal to anything".into());
    }
    println!("largest difference: {worst:.9}");
    // Both paths run the same code on the same samples in the same order, so
    // anything above float noise means the graph is not doing what the chain
    // does.
    if worst > 1e-6 {
        return Err(format!(
            "the graph and the hand-made chain disagree by {worst:.9}"
        ));
    }
    println!("the graph routes audio through both plugins exactly as a chain does");
    Ok(())
}

/// Build a two-plugin series patch inside the wrapper's saved state.
fn inject_chain(state: &str, first: &str, second: &str) -> Result<String, String> {
    let mut value: serde_json::Value =
        serde_json::from_str(state).map_err(|e| format!("wrapper state is not JSON: {e}"))?;

    let reference = |path: &str| -> Result<serde_json::Value, String> {
        let class = render::choose_class(Path::new(path), None)?;
        Ok(serde_json::json!({
            "format": class.format.tag(),
            "plugin_id": class.id,
            "path_hint": path,
            "display_name": class.name,
        }))
    };

    value["sub_plugins"] = serde_json::json!([
        { "instance": 0, "reference": reference(first)? },
        { "instance": 1, "reference": reference(second)? },
    ]);
    // The pre-M8 fields would otherwise be read instead: `instances()` prefers
    // `sub_plugins`, but leaving a stale single sub-plugin behind would make
    // this test lie about which path it exercised.
    value["sub_plugin"] = serde_json::Value::Null;
    value["sub_state"] = serde_json::Value::Null;

    // Ports as they would be after discovery (§14.2). Stereo throughout, which
    // is what M8.2 covers.
    let ports = serde_json::json!({
        "audio_in": [2], "audio_out": [2],
        "accepts_notes": false, "params": [], "latency": 0
    });
    value["graph"] = serde_json::json!({
        "nodes": [
            { "id": 0, "pos": [40.0, 40.0],  "kind": { "AudioIn": { "bus": 0, "channels": 2 } } },
            { "id": 1, "pos": [220.0, 40.0], "kind": { "Plugin": { "instance": 0, "ports": ports } } },
            { "id": 2, "pos": [400.0, 40.0], "kind": { "Plugin": { "instance": 1, "ports": ports } } },
            { "id": 3, "pos": [580.0, 40.0], "kind": { "AudioOut": { "bus": 0, "channels": 2 } } }
        ],
        "links": [
            { "from": 0, "from_port": 0, "to": 1, "to_port": 0 },
            { "from": 1, "from_port": 0, "to": 2, "to_port": 0 },
            { "from": 2, "from_port": 0, "to": 3, "to_port": 0 }
        ],
        "next_id": 4
    });
    Ok(value.to_string())
}

/// An instrument into two effects, through the node graph (ROADMAP M8.3).
///
/// Deliberately *not* a comparison against a hand-made chain. M8.2's `chain`
/// already proves audio comes out sample-identical; what M8.3 adds is a rule
/// about where notes go, and that rule is testable directly:
///
/// - notes wired to an instrument: it plays;
/// - nothing wired to its notes port: it is silent. Before M8.3 every instance
///   was handed every event the DAW sent, so this one played anyway;
/// - notes wired to the *second* instrument node: that one plays. "Any
///   instrument node", not "instance 0".
///
/// Comparing samples would have been the stronger check and it is not
/// available: several synths randomise oscillator phase from a process-global
/// generator, so two instances in one process play the same notes differently.
/// `host-cli synth <PLUGIN> <OUT.wav> --twice` reports whether a given one
/// does.
fn cmd_instrument(args: &[String]) -> Result<(), String> {
    use std::sync::Arc;

    let wrapper = args.first().ok_or("expected the wrapper's path")?;
    let synth = args.get(1).ok_or("expected an instrument plugin")?;
    let first = args.get(2).ok_or("expected the first effect")?;
    let second = args.get(3).ok_or("expected the second effect")?;

    const BLOCK: u32 = 512;
    let sample_rate = 48_000.0;
    let frames = (sample_rate * 3.0) as usize;
    let note_at = (sample_rate * 0.1) as usize;
    let silence = wav::Audio::silence(sample_rate, 2, frames);
    let events = render::note(60, note_at, (sample_rate * 2.0) as usize);

    // What the instrument is worth on its own, so "the chain is silent" can be
    // told apart from "this synth needs a preset loaded first".
    let alone = render::render(Path::new(synth), None, &silence, BLOCK, &events)?;
    println!("{} alone: peak {:.6}", short(synth), alone.audio.peak());
    if alone.audio.peak() < 1e-4 {
        return Err(format!(
            "{} produced silence on its own, so nothing here would mean anything",
            short(synth)
        ));
    }

    let (_class, mut probe) =
        render::load(Path::new(wrapper), None, Arc::new(host::CliHost::new()))
            .map_err(|e| e.to_string())?;
    run_one_block(&mut probe)?;
    let baseline = probe.save_state().map_err(|e| e.to_string())?;
    drop(probe);
    let baseline_json = read_wrapper_state(&baseline)?;

    let run = |wired: Option<u32>| -> Result<render::RenderOutcome, String> {
        let patched = inject_instrument(&baseline_json, synth, first, second, wired)?;
        let state = edit_wrapper_state(&baseline, &patched)?;
        render::render_with_state(
            Path::new(wrapper),
            None,
            Some(&state),
            &silence,
            BLOCK,
            &events,
        )
    };

    // The DoD's chain: notes -> instrument -> effect -> effect.
    let played = run(Some(0))?;
    println!(
        "notes -> instrument 1 -> {} -> {}",
        short(first),
        short(second)
    );
    println!("  peak {:.6}", played.audio.peak());
    if played.audio.peak() < 1e-4 {
        return Err("the chain is silent; the notes are not reaching the instrument".into());
    }
    // Nothing before the note-on, or something other than the note is making
    // the sound and the check above proves nothing.
    let before = played
        .audio
        .samples
        .chunks(played.audio.frames)
        .flat_map(|ch| ch[..note_at.min(ch.len())].iter())
        .fold(0.0f32, |a, s| a.max(s.abs()));
    println!("  peak before the note: {before:.6}");

    // The M8.3 rule: an unwired notes port means no notes at all.
    let unwired = run(None)?;
    println!("same graph, nothing wired to the notes port");
    println!("  peak {:.6}", unwired.audio.peak());
    if unwired.audio.peak() > 1e-4 {
        return Err(format!(
            "an instrument with nothing wired to its notes port still played \
             (peak {:.6}); it is hearing the DAW's notes anyway",
            unwired.audio.peak()
        ));
    }

    // "Any instrument node", not "instance 0": the same graph with the notes
    // going to the second one instead.
    let other = run(Some(1))?;
    println!("notes wired to instrument 2 instead");
    println!("  peak {:.6}", other.audio.peak());
    if other.audio.peak() < 1e-4 {
        return Err("notes reach instrument 1 but not instrument 2".into());
    }

    println!("notes reach the instrument the graph points at, and only that one");
    Ok(())
}

/// Every output socket of a multi-output plugin, one render each (§14.2).
///
/// The node graph gives a plugin one output socket per declared bus. Until
/// 2026-08-23 nothing had ever wired one other than the first, and when this
/// check was written it turned out all three of Surge XT's sockets rendered
/// the main output — the compiler kept a plugin's audio by node rather than by
/// port, and handed the plugin one output bus at activate.
///
/// So this renders the same material once per socket. What it can assert
/// without knowing the plugin is narrow, because which of its buses carry
/// anything is the patch's business: socket 0 has to produce something, and
/// the sockets must not all be the same signal. Each render is its own
/// instance, and a synth with a free-running oscillator does not repeat
/// itself, so socket 0 is rendered twice to establish what "the same" is worth
/// here.
fn cmd_outbus(args: &[String]) -> Result<(), String> {
    use std::sync::Arc;

    use audio_graph_engine::{Graph, PluginPorts};

    let wrapper = args.first().ok_or("expected the wrapper's path")?;
    let plugin = args
        .get(1)
        .ok_or("expected a plugin with several outputs")?;
    let class_id = args.get(2).map(String::as_str).filter(|t| !t.is_empty());

    let (class, loaded) = render::load(Path::new(plugin), class_id, Arc::new(host::CliHost::new()))
        .map_err(|e| e.to_string())?;
    let layout = loaded.io_layout();
    let ports = PluginPorts::from_layout(&layout, 0);
    drop(loaded);

    let outputs = ports.audio_out.len();
    if outputs < 2 {
        return Err(format!(
            "{} declares {outputs} output bus(es); this check needs a plugin with more than one (Surge XT has three)",
            short(plugin)
        ));
    }
    // The graph can hand a plugin the main bus and `MAX_AUX_BUSES` more, in
    // either direction — every buffer in the engine's pool is as wide as the
    // widest region, so the ceiling is memory rather than taste (§14.7). A
    // 16-out drum machine has more outputs than that, and the ones past the
    // ceiling are refused at compile time; rendering them here would show
    // silence and read like the plugin's doing.
    let reachable = (1 + plugin_host::MAX_AUX_BUSES).min(outputs);
    println!(
        "{}: {} output sockets ({reachable} within the graph's reach), {} audio in, notes {}",
        short(plugin),
        outputs,
        ports.audio_in.len(),
        ports.accepts_notes
    );

    const BLOCK: u32 = 512;
    let sample_rate = 48_000.0f32;
    let frames = (sample_rate * 2.0) as usize;

    // Whatever the plugin will react to: a note if it takes them, a tone if it
    // does not. A socket reading silence has to mean "this bus is silent", not
    // "nothing was ever fed in".
    let mut input = wav::Audio::silence(f64::from(sample_rate), 2, frames);
    if !ports.accepts_notes {
        for ch in 0..2usize {
            for i in 0..frames {
                let phase = i as f32 / sample_rate * 220.0 * std::f32::consts::TAU;
                input.samples[ch * frames + i] = 0.3 * phase.sin();
            }
        }
    }
    let events = if ports.accepts_notes {
        render::note(
            60,
            (sample_rate * 0.1) as usize,
            (sample_rate * 1.5) as usize,
        )
    } else {
        Vec::new()
    };

    let (_wrapper_class, mut probe) =
        render::load(Path::new(wrapper), None, Arc::new(host::CliHost::new()))
            .map_err(|e| e.to_string())?;
    run_one_block(&mut probe)?;
    let baseline = probe.save_state().map_err(|e| e.to_string())?;
    drop(probe);
    let baseline_json = read_wrapper_state(&baseline)?;

    let run = |socket: usize| -> Result<render::RenderOutcome, String> {
        let mut value: serde_json::Value = serde_json::from_str(&baseline_json)
            .map_err(|e| format!("wrapper state is not JSON: {e}"))?;
        value["sub_plugins"] = serde_json::json!([
            {
                "instance": 0,
                "reference": {
                    "format": class.format.tag(),
                    "plugin_id": class.id,
                    "path_hint": plugin,
                    "display_name": class.name,
                }
            }
        ]);
        value["sub_plugin"] = serde_json::Value::Null;
        value["sub_state"] = serde_json::Value::Null;

        let mut graph = Graph::new();
        let source = graph.add(
            if ports.accepts_notes {
                NodeKind::NoteIn
            } else {
                NodeKind::AudioIn(AudioIn {
                    bus: 0,
                    channels: 2,
                })
            },
            [40.0, 60.0],
        );
        let node = graph.add(
            NodeKind::Plugin(audio_graph_engine::Plugin {
                instance: 0,
                ports: ports.clone(),
            }),
            [300.0, 60.0],
        );
        let out = graph.add(
            NodeKind::AudioOut(AudioOut {
                bus: 0,
                channels: 2,
            }),
            [560.0, 60.0],
        );
        // Input sockets are the audio buses first and the notes port after
        // them, which is what makes an instrument's notes port socket 0.
        let sink = if ports.accepts_notes {
            ports.audio_in.len() as u8
        } else {
            0
        };
        graph.connect(source, 0, node, sink);
        graph.connect(node, socket as u8, out, 0);
        value["graph"] = serde_json::to_value(&graph).map_err(|e| e.to_string())?;

        let state = edit_wrapper_state(&baseline, &value.to_string())?;
        render::render_with_state(
            Path::new(wrapper),
            None,
            Some(&state),
            &input,
            BLOCK,
            &events,
        )
    };

    let mut rendered = Vec::with_capacity(reachable);
    for socket in 0..reachable {
        let outcome = run(socket)?;
        println!(
            "  socket {socket} ({}): peak {:.6}  rms {:.6}",
            layout.outputs[socket].name,
            outcome.audio.peak(),
            outcome.audio.rms()
        );
        rendered.push(outcome.audio);
    }

    if rendered[0].peak() < 1e-4 {
        return Err(format!(
            "socket 0 is silent, so nothing here would mean anything; {} may need a preset loaded first",
            short(plugin)
        ));
    }

    // The floor: two renders of the *same* socket, compared by level rather
    // than sample by sample. Each render is its own instance, and a synth with
    // a free-running oscillator starts at a different phase every time, so two
    // renders of one bus differ by nearly the whole signal while their levels
    // agree to four decimal places.
    let control = run(0)?;
    let noise = (rendered[0].rms() - control.audio.rms()).abs();
    println!("  socket 0 rendered twice differs in level by {noise:.9} (the floor)");

    let mut distinct = 0;
    for (socket, audio) in rendered.iter().enumerate().skip(1) {
        let d = (audio.rms() - rendered[0].rms()).abs();
        let verdict = if d > noise * 4.0 + 1e-4 {
            distinct += 1;
            "its own bus"
        } else {
            "indistinguishable from socket 0"
        };
        println!("  socket {socket} vs socket 0: {d:.9} — {verdict}");
    }

    if distinct == 0 {
        return Err(format!(
            "no output socket carried anything socket 0 did not. Either the graph is reading one bus for all of them, or {} mirrors its buses in this patch — check the plugin's own metering before believing the first",
            short(plugin)
        ));
    }
    println!(
        "{distinct} of the {} reachable sockets past the first carry their own bus",
        reachable - 1
    );
    Ok(())
}

fn short(path: &str) -> String {
    Path::new(path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// Build the M8.3 patch: two instrument nodes mixed together, into two effects
/// in series, with the DAW's notes wired to `wired` (or to neither).
fn inject_instrument(
    state: &str,
    synth: &str,
    first: &str,
    second: &str,
    wired: Option<u32>,
) -> Result<String, String> {
    let mut value: serde_json::Value =
        serde_json::from_str(state).map_err(|e| format!("wrapper state is not JSON: {e}"))?;

    let reference = |path: &str| -> Result<serde_json::Value, String> {
        let class = render::choose_class(Path::new(path), None)?;
        Ok(serde_json::json!({
            "format": class.format.tag(),
            "plugin_id": class.id,
            "path_hint": path,
            "display_name": class.name,
        }))
    };

    value["sub_plugins"] = serde_json::json!([
        { "instance": 0, "reference": reference(synth)? },
        { "instance": 1, "reference": reference(synth)? },
        { "instance": 2, "reference": reference(first)? },
        { "instance": 3, "reference": reference(second)? },
    ]);
    // The pre-M8 fields would otherwise be read instead, and the test would be
    // lying about which path it exercised.
    value["sub_plugin"] = serde_json::Value::Null;
    value["sub_state"] = serde_json::Value::Null;

    // Ports as discovery will report them (§14.2). An instrument has no audio
    // input and takes notes, so its port 0 *is* the notes port.
    let instrument = serde_json::json!({
        "audio_in": [], "audio_out": [2],
        "accepts_notes": true, "params": [], "latency": 0
    });
    let effect = serde_json::json!({
        "audio_in": [2], "audio_out": [2],
        "accepts_notes": false, "params": [], "latency": 0
    });

    let mut links = vec![
        serde_json::json!({ "from": 1, "from_port": 0, "to": 3, "to_port": 0 }),
        // A mix's sockets go signal, gain, signal, gain: the second audio
        // input is port 2.
        serde_json::json!({ "from": 2, "from_port": 0, "to": 3, "to_port": 2 }),
        serde_json::json!({ "from": 3, "from_port": 0, "to": 4, "to_port": 0 }),
        serde_json::json!({ "from": 4, "from_port": 0, "to": 5, "to_port": 0 }),
        serde_json::json!({ "from": 5, "from_port": 0, "to": 6, "to_port": 0 }),
    ];
    // Node 1 holds instance 0 and node 2 holds instance 1.
    if let Some(instance) = wired {
        links.push(serde_json::json!({
            "from": 0, "from_port": 0, "to": instance + 1, "to_port": 0
        }));
    }

    value["graph"] = serde_json::json!({
        "nodes": [
            { "id": 0, "pos": [40.0, 40.0],   "kind": "NoteIn" },
            { "id": 1, "pos": [220.0, 40.0],  "kind": { "Plugin": { "instance": 0, "ports": instrument } } },
            { "id": 2, "pos": [220.0, 200.0], "kind": { "Plugin": { "instance": 1, "ports": instrument } } },
            { "id": 3, "pos": [400.0, 40.0],  "kind": { "Mix": { "channels": 2, "inputs": 2 } } },
            { "id": 4, "pos": [580.0, 40.0],  "kind": { "Plugin": { "instance": 2, "ports": effect } } },
            { "id": 5, "pos": [760.0, 40.0],  "kind": { "Plugin": { "instance": 3, "ports": effect } } },
            { "id": 6, "pos": [940.0, 40.0],  "kind": { "AudioOut": { "bus": 0, "channels": 2 } } }
        ],
        "links": links,
        "next_id": 7
    });
    Ok(value.to_string())
}

/// A compressor inside the graph, keyed off another node (ROADMAP M8.4).
///
/// The signal being compressed is a steady tone, so anything that moves in the
/// output moved because of the sidechain. The key is an instrument playing one
/// note in the middle of it. Run twice — once with the key wired, once
/// without — and the difference is the ducking.
///
/// The compressor's own "SC Active" switch is turned on the way a user would:
/// a `Constant` node driving a slot that is bound to that parameter. So this
/// exercises the parameter path and the audio path at once.
fn cmd_sidechain(args: &[String]) -> Result<(), String> {
    use std::sync::Arc;

    let wrapper = args.first().ok_or("expected the wrapper's path")?;
    let comp = args.get(1).ok_or("expected a compressor plugin")?;
    let synth = args.get(2).ok_or("expected an instrument to key off")?;
    let switch: u32 = args
        .get(3)
        .ok_or("expected the compressor's sidechain-enable parameter id")?
        .parse()
        .map_err(|_| "the parameter id must be a number")?;

    const BLOCK: u32 = 512;
    let sample_rate = 48_000.0f32;
    let frames = (sample_rate * 3.0) as usize;
    let note_at = (sample_rate * 1.0) as usize;

    // A steady tone: flat by construction, so any movement in the output is
    // the compressor's doing and not the material's.
    let mut tone = wav::Audio::silence(f64::from(sample_rate), 2, frames);
    for ch in 0..2usize {
        for i in 0..frames {
            let phase = i as f32 / sample_rate * 220.0 * std::f32::consts::TAU;
            tone.samples[ch * frames + i] = 0.3 * phase.sin();
        }
    }
    let events = render::note(48, note_at, (sample_rate * 1.0) as usize);

    let (_class, mut probe) =
        render::load(Path::new(wrapper), None, Arc::new(host::CliHost::new()))
            .map_err(|e| e.to_string())?;
    run_one_block(&mut probe)?;
    let baseline = probe.save_state().map_err(|e| e.to_string())?;
    drop(probe);
    let baseline_json = read_wrapper_state(&baseline)?;

    let run = |keyed: bool| -> Result<render::RenderOutcome, String> {
        let patched = inject_sidechain(&baseline_json, comp, synth, Some((switch, 1.0)), keyed)?;
        let state = edit_wrapper_state(&baseline, &patched)?;
        render::render_with_state(
            Path::new(wrapper),
            None,
            Some(&state),
            &tone,
            BLOCK,
            &events,
        )
    };

    // How much the level moves between the quiet second and the keyed one.
    let ducking = |out: &wav::Audio| -> (f32, f32) {
        let window = |from: usize, to: usize| -> f32 {
            let mut sum = 0.0f64;
            let mut n = 0usize;
            for ch in 0..out.channels as usize {
                for i in from..to.min(out.frames) {
                    let s = out.samples[ch * out.frames + i] as f64;
                    sum += s * s;
                    n += 1;
                }
            }
            if n == 0 {
                0.0
            } else {
                (sum / n as f64).sqrt() as f32
            }
        };
        // Start half a second in: the compressor's release is a few hundred
        // milliseconds, and its settling from the first sample is not ducking.
        let quiet = window((sample_rate * 0.5) as usize, note_at);
        let keyed = window(note_at + 4800, note_at + (sample_rate * 0.9) as usize);
        (quiet, keyed)
    };

    let unkeyed = run(false)?;
    let (a, b) = ducking(&unkeyed.audio);
    println!("sidechain not wired: {a:.6} before the note, {b:.6} during");
    if a < 1e-4 {
        return Err("the compressor produced nothing; the tone is not reaching it".into());
    }
    let drift = (b / a - 1.0).abs();
    println!("  level moved by {:.1}%", drift * 100.0);
    if drift > 0.02 {
        return Err(format!(
            "the level moves by {:.1}% with nothing wired to the sidechain, \
             so this measurement cannot tell ducking from the material",
            drift * 100.0
        ));
    }

    let keyed = run(true)?;
    let (c, d) = ducking(&keyed.audio);
    println!("sidechain wired:     {c:.6} before the note, {d:.6} during");
    let reduction = 1.0 - d / c;
    println!("  ducked by {:.1}%", reduction * 100.0);
    if reduction < 0.2 {
        return Err(format!(
            "the sidechain is wired but the compressor did not duck \
             ({:.1}%); either nothing is reaching the aux bus or the \
             sidechain-enable parameter is not being driven",
            reduction * 100.0
        ));
    }
    println!("another node's audio reaches the sidechain, and the compressor acts on it");
    Ok(())
}

/// The same wiring as `sidechain`, for a plugin that has no switch to throw.
///
/// `sidechain` asks a compressor to duck, which is a strong check but needs a
/// plugin whose sidechain is (a) optional and (b) reached through one
/// parameter. Plenty of plugins fail one of those and still read their aux bus
/// -- the CLAP fixture always mixes it, Surge XT Effects hides the switch
/// inside whichever effect is loaded -- so this asks the weaker question that
/// applies to all of them: does wiring the aux bus change the output at all?
///
/// Direction is not asserted. A compressor ducks, a mixer adds, a vocoder does
/// something else entirely; what is under test is the graph's aux plumbing, not
/// the plugin's taste.
fn cmd_aux(args: &[String]) -> Result<(), String> {
    use std::sync::Arc;

    let wrapper = args.first().ok_or("expected the wrapper's path")?;
    let plugin = args.get(1).ok_or("expected a plugin with an aux input")?;
    let synth = args
        .get(2)
        .ok_or("expected an instrument to feed the aux")?;
    // Optional `ID=VALUE`: a parameter held at a value through slot 0 for the
    // whole render, for a plugin that reads its aux bus only in some modes.
    let preset = match args.get(3) {
        None => None,
        Some(text) => {
            let (id, value) = text
                .split_once('=')
                .ok_or("the fourth argument looks like PARAM_ID=VALUE")?;
            Some((
                id.parse::<u32>()
                    .map_err(|_| "the parameter id must be a number")?,
                value
                    .parse::<f64>()
                    .map_err(|_| "the value must be a number")?,
            ))
        }
    };

    const BLOCK: u32 = 512;
    let sample_rate = 48_000.0f32;
    let frames = (sample_rate * 3.0) as usize;
    let note_at = (sample_rate * 1.0) as usize;

    let mut tone = wav::Audio::silence(f64::from(sample_rate), 2, frames);
    for ch in 0..2usize {
        for i in 0..frames {
            let phase = i as f32 / sample_rate * 220.0 * std::f32::consts::TAU;
            // Quieter than `sidechain`'s tone on purpose: what the aux bus
            // contributes is measured against this, and a loud carrier buries
            // it.
            tone.samples[ch * frames + i] = 0.05 * phase.sin();
        }
    }
    let events = render::note(48, note_at, (sample_rate * 1.0) as usize);

    let (_class, mut probe) =
        render::load(Path::new(wrapper), None, Arc::new(host::CliHost::new()))
            .map_err(|e| e.to_string())?;
    run_one_block(&mut probe)?;
    let baseline = probe.save_state().map_err(|e| e.to_string())?;
    drop(probe);
    let baseline_json = read_wrapper_state(&baseline)?;

    let run = |keyed: bool| -> Result<render::RenderOutcome, String> {
        let patched = inject_sidechain(&baseline_json, plugin, synth, preset, keyed)?;
        let state = edit_wrapper_state(&baseline, &patched)?;
        render::render_with_state(
            Path::new(wrapper),
            None,
            Some(&state),
            &tone,
            BLOCK,
            &events,
        )
    };

    // Only the keyed second is compared: before the note the aux bus is silent
    // either way, so any difference there would be the plugin drifting rather
    // than the wiring doing anything.
    let rms = |out: &wav::Audio| -> f32 {
        let from = note_at + 4800;
        let to = (note_at + (sample_rate * 0.9) as usize).min(out.frames);
        let mut sum = 0.0f64;
        let mut n = 0usize;
        for ch in 0..out.channels as usize {
            for i in from..to {
                let s = out.samples[ch * out.frames + i] as f64;
                sum += s * s;
                n += 1;
            }
        }
        if n == 0 {
            0.0
        } else {
            (sum / n as f64).sqrt() as f32
        }
    };

    let unkeyed = run(false)?;
    let keyed = run(true)?;
    let (a, b) = (rms(&unkeyed.audio), rms(&keyed.audio));
    println!("aux not wired: {a:.6}");
    println!("aux wired:     {b:.6}");
    if a < 1e-4 && b < 1e-4 {
        return Err("the plugin produced nothing either way; the tone is not reaching it".into());
    }
    let change = (b - a).abs() / a.max(b);
    println!("  level moved by {:.1}%", change * 100.0);
    if change < 0.05 {
        return Err(format!(
            "wiring the aux bus changed the output by {:.1}%; either nothing              reaches the aux port or this plugin ignores it",
            change * 100.0
        ));
    }
    println!("another node's audio reaches the plugin's aux input");
    Ok(())
}

/// Tone -> compressor -> out, with an instrument keying the compressor's aux
/// bus, and a `Constant` driving a slot bound to the sidechain-enable switch.
/// M8.5's DoD, through the real wrapper: a feedback delay sounds the same at
/// any block size, and the echo lands where the delay time says it does.
///
/// A DAW check by nature — the DAW is what chooses the block size — so it is
/// done here instead (ROADMAP). No sub-plugin is involved: the patch is an
/// input, a mix, a delay line and an output, which is the smallest thing that
/// puts §14.4's cut in the middle of a cycle.
/// How much of the loop goes back round, on the `Mix` node's second input.
const FEEDBACK: f64 = 0.7;

fn cmd_delay(args: &[String]) -> Result<(), String> {
    use std::sync::Arc;

    let wrapper = args.first().ok_or("expected the wrapper's path")?;
    let sample_rate = 48_000.0f32;
    let frames = (sample_rate * 2.0) as usize;
    // Well above the 32-sample floor of §14.4, and short enough that several
    // repeats fit in two seconds.
    let delay = 0.25f64;

    // One click, then silence. Everything after it in the output came out of
    // the delay line.
    let mut input = wav::Audio::silence(f64::from(sample_rate), 2, frames);
    for ch in 0..2usize {
        input.samples[ch * frames] = 1.0;
    }

    let (_class, mut probe) =
        render::load(Path::new(wrapper), None, Arc::new(host::CliHost::new()))
            .map_err(|e| e.to_string())?;
    run_one_block(&mut probe)?;
    let baseline = probe.save_state().map_err(|e| e.to_string())?;
    drop(probe);
    let patched = inject_delay(&read_wrapper_state(&baseline)?, delay)?;
    let state = edit_wrapper_state(&baseline, &patched)?;

    let render = |block: u32| -> Result<wav::Audio, String> {
        render::render_with_state(Path::new(wrapper), None, Some(&state), &input, block, &[])
            .map(|o| o.audio)
    };

    let big = render(512)?;
    let small = render(64)?;

    // Where the repeats are. A peak every `delay` seconds is the line running
    // at the time it was told, and the count is the feedback going round.
    let expected = (delay * f64::from(sample_rate)).round() as usize;
    let mut repeats: Vec<usize> = Vec::new();
    let mut i = 1;
    while i < big.frames {
        if big.samples[i].abs() > 0.1 {
            repeats.push(i);
            i += expected / 2;
        }
        i += 1;
    }
    println!(
        "delay {delay} s = {expected} samples; {} repeats at {:?}",
        repeats.len(),
        &repeats[..repeats.len().min(4)]
    );
    if repeats.len() < 3 {
        return Err(format!(
            "expected the feedback to repeat; found {} peaks",
            repeats.len()
        ));
    }
    for (n, &at) in repeats.iter().enumerate() {
        let want = expected * (n + 1);
        // One sample of slack: the read pointer is fractional, so a peak can
        // straddle two samples.
        if at.abs_diff(want) > 1 {
            return Err(format!("repeat {} landed at {at}, expected {want}", n + 1));
        }
        // And each one is the last one times the mix's gain, which is the only
        // thing in the patch that could be making them fade.
        let height = big.samples[at].abs();
        let want = FEEDBACK.powi(n as i32 + 1) as f32;
        if (height - want).abs() > 0.02 {
            return Err(format!(
                "repeat {} came back at {height:.4}, expected {want:.4}",
                n + 1
            ));
        }
    }
    println!(
        "  repeats fade by the mix's gain: {:?}",
        repeats
            .iter()
            .take(4)
            .map(|&at| format!("{:.3}", big.samples[at].abs()))
            .collect::<Vec<_>>()
    );

    let worst = big
        .samples
        .iter()
        .zip(&small.samples)
        .map(|(a, b)| (a - b).abs())
        .fold(0.0f32, f32::max);
    println!("  512 vs 64 samples per block: largest difference {worst:.9}");
    if worst > 1e-6 {
        return Err(format!(
            "the loop sounds different at a different block size ({worst})"
        ));
    }
    println!("  ok");
    Ok(())
}

/// A feedback delay patch, written straight into the wrapper's saved state.
///
/// ```text
///   AudioIn ─┐                ┌─> AudioOut
///            ├─> Mix ─────────┤
///   DelayRead┘                └─> DelayWrite
/// ```
///
/// The read and the write are joined by the line number and by nothing else,
/// which is what keeps this out of the cycle check (§14.4).
fn inject_delay(state: &str, time: f64) -> Result<String, String> {
    let mut value: serde_json::Value =
        serde_json::from_str(state).map_err(|e| format!("wrapper state is not JSON: {e}"))?;

    value["sub_plugins"] = serde_json::json!([]);
    value["sub_plugin"] = serde_json::Value::Null;
    value["sub_state"] = serde_json::Value::Null;

    let mut graph = audio_graph_engine::Graph::new();
    let input = graph.add(
        audio_graph_engine::NodeKind::AudioIn(AudioIn {
            bus: 0,
            channels: 2,
        }),
        [40.0, 40.0],
    );
    let output = graph.add(
        audio_graph_engine::NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [600.0, 40.0],
    );
    let mix = graph.add(
        audio_graph_engine::NodeKind::Mix(Mix {
            channels: 2,
            inputs: 2,
            // The dry signal at unity (0 dB), the loop below it, so the repeats fade
            // rather than running for ever. Checking that they fade *by this
            // much* is what puts the mix's gains under test too.
            gains: vec![0.0, audio_graph_engine::linear_to_db(FEEDBACK)],
        }),
        [320.0, 40.0],
    );
    let (write, read) = graph.add_delay(audio_graph_engine::PortType::STEREO, [320.0, 240.0]);
    if let Some(audio_graph_engine::NodeKind::DelayRead(DelayRead {
        time: t, max_time, ..
    })) = graph.node_mut(read).map(|n| &mut n.kind)
    {
        *t = time;
        *max_time = time * 2.0;
    }
    graph.connect(input, 0, mix, 0);
    graph.connect(read, 0, mix, 2);
    graph.connect(mix, 0, output, 0);
    graph.connect(mix, 0, write, 0);

    value["graph"] = serde_json::to_value(&graph).map_err(|e| e.to_string())?;
    Ok(value.to_string())
}

fn inject_sidechain(
    state: &str,
    comp: &str,
    synth: &str,
    // A parameter to hold at a value through slot 0, when the plugin needs one
    // set before it will read its aux bus at all -- a sidechain-enable switch,
    // or Surge XT Effects' choice of which effect is loaded. `None` wires the
    // aux bus and leaves the parameters alone.
    //
    // Driven through a parameter socket (§14.12) rather than written directly
    // because that is the path a real patch uses, and because some plugins act
    // on a parameter only when it arrives as an event during `process`.
    switch: Option<(u32, f64)>,
    keyed: bool,
) -> Result<String, String> {
    let mut value: serde_json::Value =
        serde_json::from_str(state).map_err(|e| format!("wrapper state is not JSON: {e}"))?;

    let describe = |path: &str| -> Result<(serde_json::Value, String, Vec<u16>, bool), String> {
        use std::sync::Arc;
        let (class, plugin) = render::load(Path::new(path), None, Arc::new(host::CliHost::new()))
            .map_err(|e| e.to_string())?;
        let layout = plugin.io_layout();
        let ports = audio_graph_engine::PluginPorts::from_layout(&layout, 0);
        Ok((
            serde_json::json!({
                "format": class.format.tag(),
                "plugin_id": class.id,
                "path_hint": path,
                "display_name": class.name,
            }),
            class.id,
            ports.audio_in.clone(),
            ports.accepts_notes,
        ))
    };

    // Ports are discovered rather than written down here (§14.2): the whole
    // point is that the sidechain socket is the one the plugin really has.
    let (comp_ref, _, comp_in, _) = describe(comp)?;
    let (synth_ref, _, synth_in, synth_notes) = describe(synth)?;
    if comp_in.len() < 2 {
        return Err(format!(
            "{} declares no aux input bus, so it has no sidechain to wire",
            short(comp)
        ));
    }
    println!(
        "{}: main {} ch, sidechain {} ch",
        short(comp),
        comp_in[0],
        comp_in[1]
    );

    value["sub_plugins"] = serde_json::json!([
        { "instance": 0, "reference": comp_ref },
        { "instance": 1, "reference": synth_ref },
    ]);
    value["sub_plugin"] = serde_json::Value::Null;
    value["sub_state"] = serde_json::Value::Null;

    // Nothing is bound to a slot: the graph drives the compressor's
    // sidechain-enable switch through a parameter socket of its own, which is
    // the only route from the graph to a parameter now.
    value["slots"] = serde_json::Value::Array(
        (0..32)
            .map(|_| serde_json::json!({ "name": null, "binding": null }))
            .collect(),
    );

    let comp_params = match switch {
        Some((switch, _)) => serde_json::json!([{ "id": switch, "name": "SC Active" }]),
        None => serde_json::json!([]),
    };
    let comp_ports = serde_json::json!({
        "audio_in": comp_in, "audio_out": [2],
        "accepts_notes": false, "params": comp_params, "latency": 0
    });
    let synth_ports = serde_json::json!({
        "audio_in": synth_in, "audio_out": [2],
        "accepts_notes": synth_notes, "params": [], "latency": 0
    });
    // The synth's notes port sits after its audio inputs.
    let synth_notes_port = synth_in.len() as u32;

    let mut links = vec![
        serde_json::json!({ "from": 0, "from_port": 0, "to": 1, "to_port": 0 }),
        serde_json::json!({ "from": 1, "from_port": 0, "to": 4, "to_port": 0 }),
    ];
    if switch.is_some() {
        // The switch's socket sits after the compressor's audio inputs; it
        // takes no notes, so there is nothing else in between.
        links.push(serde_json::json!({
            "from": 5, "from_port": 0, "to": 1, "to_port": comp_in.len() as u32
        }));
    }
    if synth_notes {
        links.push(serde_json::json!({
            "from": 3, "from_port": 0, "to": 2, "to_port": synth_notes_port
        }));
    }
    if keyed {
        // Port 1 of the compressor is its sidechain -- see `plugin_input_ports`.
        links.push(serde_json::json!({ "from": 2, "from_port": 0, "to": 1, "to_port": 1 }));
    }

    value["graph"] = serde_json::json!({
        "nodes": [
            { "id": 0, "pos": [40.0, 40.0],   "kind": { "AudioIn": { "bus": 0, "channels": 2 } } },
            { "id": 1, "pos": [400.0, 40.0],  "kind": { "Plugin": { "instance": 0, "ports": comp_ports } } },
            { "id": 2, "pos": [220.0, 220.0], "kind": { "Plugin": { "instance": 1, "ports": synth_ports } } },
            { "id": 3, "pos": [40.0, 220.0],  "kind": "NoteIn" },
            { "id": 4, "pos": [600.0, 40.0],  "kind": { "AudioOut": { "bus": 0, "channels": 2 } } },
            { "id": 5, "pos": [40.0, 400.0],  "kind": { "Constant": { "value": switch.map_or(1.0, |(_, v)| v) } } }
        ],
        "links": links,
        "next_id": 6
    });
    Ok(value.to_string())
}

/// Open the wrapper's own editor with a patch already in it (ROADMAP M8.4).
///
/// `host-cli gui` opens an empty one, which never draws a plugin node — and a
/// plugin node is where the new drawing is. This builds a patch that has one,
/// with its sockets discovered from the plugin and one parameter socket
/// already added, so opening it exercises every path the canvas grew.
fn cmd_editor(args: &[String]) -> Result<(), String> {
    use std::sync::Arc;

    let wrapper = args.first().ok_or("expected the wrapper's path")?;
    let plugin = args.get(1).ok_or("expected a plugin to put in a node")?;
    let hold = hold_for(args.get(2))?;

    let (class, mut probe) = render::load(Path::new(wrapper), None, Arc::new(host::CliHost::new()))
        .map_err(|e| e.to_string())?;
    run_one_block(&mut probe)?;
    let baseline = probe.save_state().map_err(|e| e.to_string())?;
    drop(probe);

    let patched = inject_one_plugin(&read_wrapper_state(&baseline)?, plugin)?;
    let state = edit_wrapper_state(&baseline, &patched)?;

    let mut sub = subhost_adapter::SubHost::new(Arc::new(host::CliHost::new()), SUB_HOST);
    sub.load(0, Path::new(wrapper), Some(&class.id))?;
    sub.load_sub_state(0, &state)?;
    // The wrapper reads its own state at activate, not when the blob arrives:
    // nice-plug restores persisted fields first, and the sub-plugins have to be
    // loaded at the sample rate they are about to be given. Without this the
    // editor opens on the default patch and the whole point of the injection —
    // one node of every kind on screen — is lost.
    let processors = sub.activate(
        plugin_host::AudioConfig {
            sample_rate: 48_000.0,
            max_block_size: 512,
            input_channels: 2,
            output_channels: 2,
            aux_inputs: Default::default(),
            aux_outputs: Default::default(),
            offline: false,
        },
        &[],
        &[],
    )?;
    sub.open_editor(0, std::ptr::null_mut())?;
    println!("opened the wrapper's editor with a {} node", short(plugin));
    match hold {
        Some(limit) => println!("close the window, or wait {:.0}s", limit.as_secs_f64()),
        None => println!("close the window when you are done looking"),
    }

    let (open, elapsed) = hold_editor_open(&mut sub, hold);
    sub.close_editor(0);
    sub.deactivate(processors);
    sub.unload_all();
    println!(
        "{} after {:.1}s",
        if open { "still open" } else { "closed" },
        elapsed.as_secs_f64()
    );
    println!("teardown completed cleanly");
    Ok(())
}

/// Audio in -> one plugin node -> audio out, with the plugin's real sockets.
fn inject_one_plugin(state: &str, plugin: &str) -> Result<String, String> {
    use std::sync::Arc;

    let mut value: serde_json::Value =
        serde_json::from_str(state).map_err(|e| format!("wrapper state is not JSON: {e}"))?;

    let (class, loaded) = render::load(Path::new(plugin), None, Arc::new(host::CliHost::new()))
        .map_err(|e| e.to_string())?;
    let layout = loaded.io_layout();
    let mut ports = audio_graph_engine::PluginPorts::from_layout(&layout, 0);
    // One parameter socket, as if the user had pressed "+ param" once.
    if let Some(first) = loaded.params().first() {
        ports.params.push(audio_graph_engine::ParamPort {
            id: first.id.0,
            name: first.name.clone(),
        });
    }
    println!(
        "{}: {} audio in, {} audio out, notes {}, {} parameters",
        short(plugin),
        ports.audio_in.len(),
        ports.audio_out.len(),
        ports.accepts_notes,
        loaded.params().len()
    );
    drop(loaded);

    value["sub_plugins"] = serde_json::json!([
        {
            "instance": 0,
            "reference": {
                "format": class.format.tag(),
                "plugin_id": class.id,
                "path_hint": plugin,
                "display_name": class.name,
            }
        }
    ]);
    value["sub_plugin"] = serde_json::Value::Null;
    value["sub_state"] = serde_json::Value::Null;
    // Every kind of socket at once: audio, an aux input, a param and (if the
    // plugin takes them) notes. The canvas colours sockets by type, and a patch
    // with only one type in it is a patch that cannot show whether it works.
    let mut graph = audio_graph_engine::Graph::new();
    let input = graph.add(
        audio_graph_engine::NodeKind::AudioIn(AudioIn {
            bus: 0,
            channels: 2,
        }),
        [40.0, 60.0],
    );
    let sidechain = graph.add(
        audio_graph_engine::NodeKind::AudioIn(AudioIn {
            bus: 1,
            channels: 2,
        }),
        [40.0, 200.0],
    );
    let node = graph.add(
        audio_graph_engine::NodeKind::Plugin(audio_graph_engine::Plugin {
            instance: 0,
            ports: ports.clone(),
        }),
        [300.0, 60.0],
    );
    let mix = graph.add(
        audio_graph_engine::NodeKind::Mix(Mix {
            channels: 2,
            inputs: 2,
            gains: vec![0.0, audio_graph_engine::linear_to_db(0.5)],
        }),
        [560.0, 60.0],
    );
    let output = graph.add(
        audio_graph_engine::NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [800.0, 60.0],
    );
    let (write, read) = graph.add_delay(audio_graph_engine::PortType::STEREO, [300.0, 340.0]);
    let slot = graph.add(
        audio_graph_engine::NodeKind::SlotIn(SlotIn { slot: 0 }),
        [40.0, 400.0],
    );

    graph.connect(input, 0, node, 0);
    if ports.audio_in.len() > 1 {
        graph.connect(sidechain, 0, node, 1);
    }
    graph.connect(node, 0, mix, 0);
    graph.connect(read, 0, mix, 2);
    graph.connect(mix, 0, output, 0);
    graph.connect(mix, 0, write, 0);
    // A slot driving the delay time, so a param link is on screen too.
    graph.connect(slot, 0, read, 0);

    value["graph"] = serde_json::to_value(&graph).map_err(|e| e.to_string())?;
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

/// Pump the message loop until the editor closes, or until `deadline` passes.
///
/// A plugin would never do this — the DAW is already pumping — but a harness
/// has to, or the window comes up and never repaints.
///
/// `None` means "until the window is closed", which is what an unattended run
/// must never ask for and what looking at the thing always wants. Whichever of
/// the two ended it is worth reporting: a window that closes itself well before
/// its deadline is the plugin giving up, not the user.
fn hold_editor_open(
    sub: &mut subhost_adapter::SubHost,
    deadline: Option<std::time::Duration>,
) -> (bool, std::time::Duration) {
    use std::time::{Duration, Instant};

    let started = Instant::now();
    while sub.editor_is_open(0) {
        if deadline.is_some_and(|limit| started.elapsed() >= limit) {
            break;
        }
        plugin_host::pump_events();
        sub.tick_editors();
        std::thread::sleep(Duration::from_millis(16));
    }
    (sub.editor_is_open(0), started.elapsed())
}

/// How long a harness should hold an editor open, from an optional argument.
///
/// Absent means "until the window is closed"; `0` says the same thing out loud,
/// for a script that would rather not rely on an argument's absence.
fn hold_for(arg: Option<&String>) -> Result<Option<std::time::Duration>, String> {
    let Some(text) = arg else { return Ok(None) };
    let seconds: f64 = text.parse().map_err(|_| "bad duration")?;
    if seconds <= 0.0 {
        return Ok(None);
    }
    Ok(Some(std::time::Duration::from_secs_f64(seconds)))
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
    use subhost_adapter::SubHost;

    let path = args.first().ok_or("expected a plugin path")?;
    let class_id = args.get(1).map(String::as_str).filter(|t| !t.is_empty());
    let hold = hold_for(args.get(2).filter(|s| !s.starts_with("--")))?;
    let reverse = args.iter().any(|a| a == "--reverse");

    let mut sub = SubHost::new(Arc::new(host::CliHost::new()), SUB_HOST);
    sub.load(0, Path::new(path), class_id)?;
    let name = sub.class(0).map(|c| c.name.clone()).unwrap_or_default();
    sub.open_editor(0, std::ptr::null_mut())?;
    println!("opened {name}");
    match hold {
        Some(limit) => println!("close the window, or wait {:.0}s", limit.as_secs_f64()),
        None => println!("close the window when you are done looking"),
    }

    let (open, elapsed) = hold_editor_open(&mut sub, hold);
    if open {
        println!("held open for {:.1}s", elapsed.as_secs_f64());
    } else {
        println!("the editor closed after {:.1}s", elapsed.as_secs_f64());
    }

    if reverse {
        println!("destroying the whole instance with the editor still open");
        drop(sub);
    } else {
        println!("closing the editor, then unloading");
        sub.close_editor(0);
        drop(sub);
    }

    // Dispatch anything the plugin posted while tearing down. A bad ordering
    // usually surfaces here rather than at the moment of the mistake.
    plugin_host::pump_events();

    println!("teardown completed cleanly");
    Ok(())
}
