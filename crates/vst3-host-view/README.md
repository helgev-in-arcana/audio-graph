# vst3-host-view

The window half of VST3 editor hosting: everything that happens between an
`IPlugView` and a real window on screen.

## Responsibilities

- `EditorWindow` — owns a container window and the plugin's view together, and
  is therefore the one place the teardown order is written: the view is removed
  and released before the controller terminates.
- `PlugFrame` — the `IPlugFrame` a plugin calls back into, so `resizeView` and
  the plugin's own close request reach the host.
- Naming the platform handle the way VST3 expects it.

## Not this crate's job

- **Creating the plugin instance or the view.** `vst3-host` does that and hands
  the `IPlugView` over; that handover is the whole seam between the two crates.
- **The window itself.** `ContainerWindow`, the deferred queue and key
  forwarding are format-agnostic and live in `host-window`, so the CLAP backend
  can reach them without depending on VST3. They are re-exported here only so
  callers that already speak in this crate's names keep compiling.
- **Deciding when an editor opens.** That is the wrapper's decision, driven by a
  user action; this crate is asked, never asking.

## Invariants

- **Owning both halves is what makes the order enforceable.** A caller holding
  the view and the window separately could drop them in either order; here it
  cannot.
- **All access is on the UI thread**, which is where VST3 confines `IPlugFrame`
  calls. The `unsafe impl Send`/`Sync` on the frame rests on that, plus the
  frame outliving the pointer to it.
