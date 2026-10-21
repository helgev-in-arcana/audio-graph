# vst3-host

A VST3 host backend in pure Rust. Everything that is true of VST3 and of nothing
else lives here.

## Responsibilities

- Loading a module, reading its factory, and enumerating the classes it exports.
- Instantiating a class and driving the long, order-sensitive VST3 lifecycle:
  `initialize`, connect the component and controller, negotiate buses, `setupProcessing`,
  `setActive`, `IAudioProcessor::process`, and the reverse on the way out.
- Translating between VST3's vocabulary and the shared one in `plugin-host-api`:
  parameter ids and normalised values, note expressions, bus layouts, process
  contexts, state streams.
- Creating `Vst3View`, an owning view handle that keeps its instance and module alive.
- Synchronizing controller edits, processor automation, and native parameter output.

## Not this crate's job

- **Windows.** Attaching a view to a frame, resizing it, tearing it down in
  order: `vst3-host-view`.
- **Choosing between formats.** A caller that has to ask "VST3 or CLAP?" is in
  `plugin-host`, not here.
- **Anything about nesting.** No transport forwarding, no latency arithmetic, no
  slot tables. This crate does not know a DAW is above it.
- **Choosing host policy.** The caller supplies `plugin_host_api::HostContext`;
  this backend implements the native `IHostApplication` and callback shims.

## Invariants

- **The two-trait split is the activation gate.** `Vst3Plugin` is the
  main-thread half and `activate` yields an owning `Processor`, so a processor
  cannot exist before the sequence that makes one valid has run.
- **The processing containers are sized before any audio runs.** Input
  parameter points share one pool sized from `AudioConfig::max_input_events`,
  sorted into one run per parameter before `process`, so a caller's points may
  fall on any parameters in any proportion. Input batches that exceed capacity
  or violate event timing are rejected before native processing.
  Main-thread edits remain queued until a complete batch can be delivered.
- **Successful activation matches the requested buses.** The backend verifies
  native arrangements and channel counts; it does not silently substitute stereo.
- **The native host application is module-scoped.** It retains only the host
  name, not one instance's callback context. Per-instance component handlers
  retain their own `HostContext`. Each binary has one owner thread; another
  thread receives `HostError::ModuleBusy` until it is released.
- **Thread initialization has an owner.** Hold the `ApartmentGuard` returned by
  `init_apartment()` until every plugin, view and returned processor is released.
  An incompatible Windows apartment is reported as an error.
- **Component and controller are connected only when they are distinct
  objects.** A single object implementing both would be connected to itself,
  which plugins do not expect and at least one corrupts its heap over.
- **The editor seam owns native lifetime.** `vst3-host-view` receives `Vst3View`.
  Its raw pointer is borrowed; derived native references must not outlive the handle.

Plain parameter values use the controller's conversion methods, including
stepped parameters. `SetNormalized` already carries the value the VST3 processor
expects. Parameter mappings are prepared outside processing and searched by ID
without asking the controller again for every audio block.

VST3 silence flags describe the current block, not whether a plugin will remain
silent. They do not cause the host to return `ProcessStatus::Sleep`. Latency
change requests are propagated only when the reported latency actually changes.

## Tests

From the repository root:

```sh
cargo build --workspace --locked
cargo test -p vst3-host --locked
```

The [VST3 fixture](../vst3-test-plugin/README.md) exercises native ownership,
parameter synchronization, event capacity, and activation contracts. Optional
installed-plugin tests use [`AUDIO_GRAPH_TEST_PLUGINS`](../../.env.example).
