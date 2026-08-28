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
- Handing out the raw `IPlugView` for an editor — and nothing further.

## Not this crate's job

- **Windows.** Attaching a view to a frame, resizing it, tearing it down in
  order: `vst3-host-view`.
- **Choosing between formats.** A caller that has to ask "VST3 or CLAP?" is in
  `plugin-host`, not here.
- **Anything about nesting.** No transport forwarding, no latency arithmetic, no
  slot tables. This crate does not know a DAW is above it.
- **Providing host services.** `IHostApplication` and friends are *injected*
  through `plugin_host_api::HostContext`; this crate never builds its own.

## Invariants

- **The two-trait split is the activation gate.** `Vst3Plugin` is the
  main-thread half and `activate` yields `Vst3Processor` by value, so a processor
  cannot exist before the sequence that makes one valid has run.
- **The processing containers are sized before any audio runs.** The sub-block
  quantiser bounds how many parameter points a block can carry, so the ceilings
  are fixed rather than derived and `process` never allocates.
- **The host context is module-scoped.** The factory keeps the pointer it is
  given for the module's whole lifetime, and on Linux it is also where a plugin
  picks up its run loop — so it must outlive any editor.
- **Component and controller are connected only when they are distinct
  objects.** A single object implementing both would be connected to itself,
  which plugins do not expect and at least one corrupts its heap over.
- **No VST3 type reaches a public signature outside this crate**, with the single
  deliberate exception of the `IPlugView` handed to `vst3-host-view`.
