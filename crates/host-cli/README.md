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
- The standing regression sweep over every plugin installed on the machine.
- Building patches directly, so graph behaviour can be checked against a
  hand-rendered equivalent.

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
