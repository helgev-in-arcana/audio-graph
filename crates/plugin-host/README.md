# plugin-host

`plugin-host` is the format-neutral hosting facade for one module the caller
names. Finding modules — folders, the scan catalogue, resolving saved
references — is `plugin-scan`, above this crate; AudioGraph's persistent folders,
pins, and `config.json` live in `audio-graph-settings`.

One facade over both plugin backends. A caller says "load this path", "give me
its parameters", "open its editor" and never learns which format answered.

## Responsibilities

- **Anything whose answer differs by format, for one module.** What classes it
  exports (`scan_module`), how an instance is created, how an editor is attached.
- **`Plugin`** — one loaded sub-plugin, whichever format it came in, including
  module lifetime and the order things must be torn down in.
- **`Format`** — the closed set of formats, and the stable tags a saved project
  holds.
- **`MainThread` / `Processor` / `reclaim_main_thread`** — the common lifetime and
  owner-thread destruction contract, re-exported from `plugin-host-api` so
  backends and their callers use the same return path.
- Re-exporting `plugin-host-api` wholesale, so a caller needs one dependency
  instead of two.

## Not this crate's job

- **Hosting a plugin inside another plugin** — forwarding the DAW's transport,
  combining latency, publishing automatable slots, nesting state. That is
  `subhost-adapter`.
- **Defining the data model.** `plugin-host-api` owns it; nothing is added to it
  here.
- **Looking at more than one module.** Listing folders, the catalogue, and
  resolving saved references are `plugin-scan`'s.
- **Talking to a specific format.** `vst3-host`, `vst3-host-view` and `clap-host`
  do that; this crate is the arm that chooses between them.
- **Windows.** `host-window` owns the container window and key forwarding.
  The embedding application owns deferred main-thread work.

The test for whether something belongs here: **would a standalone offline
renderer or a plugin scanner still need it?** If yes, it belongs here or below.
If it only makes sense because a DAW is above us, it belongs in
`subhost-adapter`.

## Invariants

- **A saved reference is `(format, plugin_id, path_hint)`, and the id is the
  authority.** Plugin folders differ between machines; where a reference went is
  found through `plugin-scan`'s catalogue, never by opening modules to ask.
- **Enumerating a module means loading third-party code.** Anything that opens a
  module says so and expects to be called off the UI thread.

## Host lifecycle

Create `let _thread = plugin_host::init_thread()?;` on the owning thread before
loading plugins. Keep this guard alive until all plugins, editors, and processors
have been released and returned resources reclaimed.

The main loop services three separate responsibilities:

- `poll()` advances this library's window events inside a DAW. Standalone hosts
  can use `pump_events()` to dispatch their own message queue. A plugin must not
  call `pump_events()` on behalf of its DAW.
- Call each `Plugin::tick()` even without an open editor, to deliver main-thread
  callbacks and service plugin timers and editor changes.
- Call `reclaim_main_thread()` to destroy resources returned from other threads,
  including during shutdown before releasing the thread guard.

Processing, metadata refresh, and processor return follow the
[shared API contracts](../plugin-host-api/README.md#processing-and-metadata-contracts).

## Processing flow

1. Initialize the owner thread, then call `Plugin::load(path, class_id, context)`.
   `None` selects the first class; restoring a saved reference should pass its ID.
2. Inspect `io_layout()` and parameters. Request main bus widths while inactive,
   then build an `AudioConfig` matching the supported layout and input-event bound.
3. Allocate flat audio buffers and an `EventSink::with_capacity` before processing.
   `activate(config)` returns the owning `Processor`.
4. Import `SubPluginProcessor` to call `process`. Clear the output sink at the
   start of its collection interval, inspect the returned `ProcessStatus`, and
   check `overflowed()` before treating the output event stream as complete.
5. Service `tick()` and metadata requests on the owner thread. Stop and return
   the processor before applying a change that needs deactivation, then rebuild
   buffers and activate with the new descriptors.
6. Call `processor.deactivate()` or drop it. Return resources from the audio
   thread, reclaim them on the owner thread, close editors and release plugins
   before dropping the thread guard.

The [CLI host](../host-cli/src/host.rs) provides a concrete `HostContext`;
[CLI commands](../host-cli/src/main.rs) show complete loading and processing
flows. These APIs are intended for workspace use and are not yet stable.

## Tests

From the repository root, build the native fixtures before running the tests:

```sh
cargo build --workspace --locked
cargo test -p plugin-host --locked
```

Tests of installed plugins use the explicit path list described in
[`.env.example`](../../.env.example); absent suitable paths, those cases skip.
Fixture-based tests do not require an installed commercial plugin.
