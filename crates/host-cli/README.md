# host-cli

A development harness that stands in for a DAW. Everything worth proving about
the host — that a real plugin loads, enumerates, instantiates, processes, saves
and restores — is exercised from here without a DAW in the loop.

## Responsibilities

- Driving the whole stack from the outside: scan, load, inspect parameters,
  render offline, open and tear down an editor, save and restore state.
- Demonstrating claims the unit tests structurally cannot. A unit test can show
  the compiled program and the events it produces; only this can show those
  events reaching a real commercial plugin and coming back out as a change in
  the audio.
- A regression sweep over the modules found in selected plugin directories.
- Building patches directly, so graph behaviour can be checked against a
  hand-rendered equivalent.
- Sharing AudioGraph's persisted folder list through `audio-graph-settings`,
  and passing the chosen paths to the generic host's scanner.

## Not this crate's job

- **Being a product.** No stability promises, no pretty output, no packaging.
- **Being the unit tests.** Anything that can be asserted in-process belongs in
  a crate's own `tests/`.
- **Linking the wrapper.** This crate checks the engine and the adapter without
  pulling in `audio-graph-plugin` or egui — `cargo tree -p host-cli` is the
  check. That is why the wrapper's ceilings are repeated here rather than
  imported, and they have to be kept in step with `audio_graph_plugin::SUB_HOST`.

## Invariants

- **The sweep probes each module in a child process.** A third-party plugin that
  corrupts its own heap on teardown would otherwise take the whole sweep with
  it, and losing the results for the other fifty is not an acceptable way to
  learn that one of them is broken.
- **Evidence over assertion.** Where a plugin is a black box, the check renders
  twice and compares: the interesting fact is *where* two renders diverge.
- **A single failure proves nothing.** Plugins refuse writes to some parameters
  and recompute others every block, so checks that poke a parameter try several
  before giving up.

## Running commands

Run from the repository root:

```sh
cargo run -p host-cli -- <COMMAND> <ARGS...>
```

Running without a command prints the complete usage list and exits with a
failure status. There is no separate successful `--help` command. Most `PLUGIN`
arguments accept either `.vst3` or `.clap`; the usage text identifies the wrapper
format required by a particular check. Optional `ID` arguments select a VST3
class ID or a CLAP plugin ID printed by `scan` and `info`.

| Commands | Purpose |
| --- | --- |
| `dirs`, `scan`, `info`, `params`, `buses` | Inspect the selected directories, modules, classes, parameters, and bus layouts. |
| `render`, `synth`, `automate` | Render audio or an instrument and check parameter timing. |
| `churn`, `twice`, `state`, `probe`, `sweep` | Exercise repeated loading, activation, state, and teardown. |
| `nest`, `graph`, `chain`, `instrument` | Drive exported wrapper bundles and compare routing and state behavior. |
| `sustain`, `pedal`, `wheel`, `ends`, `tremolo` | Check MIDI controllers, voice completion, and MIDI/audio tremolo. |
| `sidechain`, `aux`, `outbus`, `delay`, `follow` | Check sidechains, extra outputs, feedback delay, and audio-derived control. |
| `resolution` | Check parameter event capacity at every supported resolution. |
| `gui`, `editor`, `live` | Open a native editor, a prepared wrapper patch, or LFO-driven live controls. |

`scan [DIR...]` and `sweep [DIR...]` use the supplied folders, or the shared
AudioGraph settings if no folders are given. Discovery examines each folder
and one level below it. `dirs` can initialize the settings file on first use;
set `AUDIO_GRAPH_CONFIG` to use a separate profile.

Commands taking a wrapper bundle need a build first:

```sh
cargo xtask bundle audio-graph-plugin --release --locked
cargo run -p host-cli -- live target/bundled/AudioGraph.vst3 10
```

`live` runs audio blocks while the editor displays moving socket values. Other
GUI commands can remain open until their window is closed; their optional
duration arguments are useful for scripted checks. Linux editor commands need
an X server, and macOS hosted-editor containers are not implemented.

The CLI takes plugin paths explicitly or discovers them through its directory
arguments/settings. The workspace tests' `AUDIO_GRAPH_TEST_PLUGINS` list is a
separate mechanism, documented in [`.env.example`](../../.env.example).
