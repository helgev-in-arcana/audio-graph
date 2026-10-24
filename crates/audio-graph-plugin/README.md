# audio-graph-plugin

The product integration layer: one plugin to the DAW, a host on the inside.
It exports `Audio Graph FX` and `Audio Graph Instrument` from one VST3/CLAP
binary, using nice-plug for the wrapper and nice-plug-egui for its editor.

## Responsibilities

- Being a plugin: two classes exported from one binary — an effect with a stereo
  input, an instrument without one — because plugin categories are static while
  the sub-plugin's kind is not.
- Choosing AudioGraph's numbers: how many slots to publish, how many instances a
  patch may hold, how many lanes a parameter row carries. They are handed to
  `subhost-adapter` as configuration, which never names one itself.
- The editor: the node canvas, slot binding controls, and the plugin browser,
  including parameter learning from a hosted plugin's own GUI and live socket
  values.
- Connecting `audio-graph-settings` to the host scanner and catalogue, and
  choosing the browser's display categories from the scanned class metadata.
- Owning the state split between the main thread and the audio thread, and
  handing each compiled program to the engine's `ProgramPublisher` for
  preparation and publication.
- The wrapper's saved document, with the adapter's and the engine's states
  nested inside it.
- The main-thread tick that keeps sub-plugins alive whether or not any window is
  open.

## Product configuration

[`config.rs`](src/config.rs) sets the product's ceilings: 16 plugin instances
and 32 DAW automation slots. Graph parameter lanes take precedence over bound
slots when both target the same parameter. Values sent to a plugin are
normalized to `0..1`; the backend maps them to its native representation.

Parameter resolution and audio Sub-block are independent patch settings.
`WrapperState` saves them with the graph, slot bindings, child state blobs, and
the writing release's version. Plugin folders and pins belong to
[`audio-graph-settings`](../audio-graph-settings/README.md) and are shared across
instances, rather than saved in each DAW project.

The settings bar's `Reset` clears running notes, delays, phases, and key latches
and resets child processing without replacing the patch. The graph toolbar has
a separate `Reset` that restores the default graph, while `Clear` makes it empty.
Graph replacement and loading a saved document are main-thread operations.

## Not this crate's job

- **Hosting mechanics.** Loading, binding, latency, nesting: `subhost-adapter`.
- **Graph semantics.** Nodes, compilation, evaluation: `audio-graph-engine`.
- **Anything format-specific.** No VST3 or CLAP type appears here.

## Invariants

- **What you hear is what is drawn.** There is no implicit pass-through and no
  implicit "through the loaded plugin" route. A new instance gets a real patch —
  input wired to output — and an empty canvas is silence. A patch saved when
  those implicit routes existed is migrated to the graph it was really running.
- **The audio thread is never made to wait for the editor.** State is split by
  how often it is touched: main-thread-only state takes no lock, the live
  processors sit behind a mutex the audio thread only ever *tries*, and the
  compiled program crosses through a lock-free handoff — which is the path every
  graph edit takes, so it is the one that had to be free.
- **Allocation happens on the main thread and rides over inside a prepared
  program.** The engine's publisher sizes delay rings on this thread, and an unchanged line
  is handed nothing rather than a fresh copy of what it already has. Pending
  replacements transfer matching rings before they are published.
- **Nothing that opens or resizes a window happens inside `ui()`.** Every such
  button pushes a command and returns; the commands run on the next turn of the
  DAW's message loop, once the frame is over.
- **A missing sub-plugin must not stop a project from opening.** It is reported,
  its state, bindings, and recorded ports are retained, and a completed scan can
  retry it using matching catalogue entries. Connections whose ports are
  temporarily unavailable remain pending until the ports return or the user
  explicitly discards them.
- **An unreadable document remains intact.** A document that cannot be decoded
  is retained, playback is silent, and the editor offers an explicit new-graph
  action. Loading a readable preset also resumes normal operation.
- **An unknown node does not make the whole document unreadable.** Its saved
  representation and connections remain in the graph, but it does not execute.
  Readable routes can still run. A warning identifies unreadable nodes and
  documents written by a newer release; unknown fields on otherwise readable
  nodes are not guaranteed to survive saving through an older build.
- **Capacity loss is visible.** Dropped note events and voices cut off at the
  live-note limit are counted and reported through the editor's notifications.

## Build and verification

From the repository root:

```sh
cargo build --workspace --locked
cargo test -p audio-graph-plugin --locked
cargo xtask bundle audio-graph-plugin --release --locked
```

The workspace build provides native fixtures for the integration tests. Bundling
produces `target/bundled/AudioGraph.vst3` and `AudioGraph.clap`. Optional tests
against installed plugins use [`AUDIO_GRAPH_TEST_PLUGINS`](../../.env.example).

Integration tests drive `Wrapper` with the DAW context in
[`tests/harness`](tests/harness/mod.rs), covering audio, saved-state restoration,
reset, latency, publication, and child lifetime. The
[`host-cli`](../host-cli/README.md) commands exercise the exported bundles and
editor with real plugins. Hosted editor support follows
[`host-window`](../host-window/README.md#backends); a macOS build does not imply
a working native child editor.
