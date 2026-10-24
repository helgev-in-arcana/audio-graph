# vst3-host-view

The window half of VST3 editor hosting: everything that happens between an
`IPlugView` and a real window on screen.

## Responsibilities

- `EditorWindow` — owns a container window and the plugin's view together, and
  is therefore the one place the teardown order is written: the view is removed
  and released before the controller terminates.
- `PlugFrame` — the `IPlugFrame` a plugin calls to request a resize, with the
  Linux `IRunLoop` used for timer and descriptor callbacks. Container close
  requests are observed by `EditorWindow` through `WindowState`.
- Naming the platform handle the way VST3 expects it.

## Not this crate's job

- **Creating the plugin instance or the view.** `vst3-host` does that and hands
  an owning `Vst3View` over. This crate depends on `vst3-host` for that lifetime contract.
- **The window itself.** `ContainerWindow` and key forwarding are format-agnostic
  and live in `host-window`, so the CLAP backend can use them without depending
  on VST3. This crate re-exports those window APIs. The caller owns deferred
  main-thread work.
- **Deciding when an editor opens.** That is the wrapper's decision, driven by a
  user action; this crate is asked, never asking.

## Invariants

- **Owning both halves is what makes the order enforceable.** A caller holding
  the view and the window separately could drop them in either order; here it
  cannot.
- **All access is on the UI thread**, which is where VST3 confines `IPlugFrame`
  calls. The frame and run loop are neither `Send` nor `Sync`.
- **Callbacks belong to a registration generation.** Replacing or retiming a
  registration invalidates callbacks collected for its previous generation.
  Linux native handlers are retained for their registration and additionally
  while a callback runs, so self-unregistration cannot release the object before
  the call returns.

Window support follows [host-window](../host-window/README.md#backends): Win32
and X11 containers are implemented; macOS container creation is a stub. Naming
`NSView` as the VST3 platform type alone does not provide a macOS editor.
