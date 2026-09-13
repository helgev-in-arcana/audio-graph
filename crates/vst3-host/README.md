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
- **The processing containers are sized before any audio runs.** Input batches
  that exceed capacity or violate event timing are rejected before native processing.
  Main-thread edits remain queued until a complete batch can be delivered.
- **Successful activation matches the requested buses.** The backend verifies
  native arrangements and channel counts; it does not silently substitute stereo.
- **The host context is module-scoped.** The factory keeps the pointer it is
  given for the module's whole lifetime. Each binary has one owner thread within
  this backend; another thread receives `HostError::ModuleBusy` until it is released.
- **Thread initialization has an owner.** Hold the `ApartmentGuard` returned by
  `init_apartment()` until every plugin, view and returned processor is released.
  An incompatible Windows apartment is reported as an error.
- **Component and controller are connected only when they are distinct
  objects.** A single object implementing both would be connected to itself,
  which plugins do not expect and at least one corrupts its heap over.
- **The editor seam owns native lifetime.** `vst3-host-view` receives `Vst3View`.
  Its raw pointer is borrowed; derived native references must not outlive the handle.
