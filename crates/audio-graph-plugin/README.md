# audio-graph-plugin

The wrapping plugin itself: one plugin to the DAW, a host on the inside. This is
the only crate in the workspace that knows AudioGraph is a product.

## Responsibilities

- Being a plugin: two classes exported from one binary — an effect with a stereo
  input, an instrument without one — because plugin categories are static while
  the sub-plugin's kind is not.
- Choosing AudioGraph's numbers: how many slots to publish, how many instances a
  patch may hold, how many lanes a sub-block carries. They are handed to
  `subhost-adapter` as configuration, which never names one itself.
- The editor: the node canvas, the slot table, the plugin browser.
- Owning the state split between the main thread and the audio thread, and
  publishing each newly compiled program across it.
- The wrapper's saved document, with the adapter's and the engine's states
  nested inside it.
- The main-thread tick that keeps sub-plugins alive whether or not any window is
  open.

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
- **Allocation happens on the main thread and rides over inside the program.**
  Delay rings are sized here, and an unchanged line is handed nothing rather
  than a fresh copy of what it already has.
- **Nothing that opens or resizes a window happens inside `ui()`.** Every such
  button pushes a command and returns; the commands run on the next turn of the
  DAW's message loop, once the frame is over.
- **A missing sub-plugin must not stop a project from opening.** It is reported,
  the bindings are kept, and reinstalling the plugin brings them back.
