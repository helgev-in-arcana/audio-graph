# host-window

Window plumbing for hosting somebody else's editor, with no plugin format in
sight.

## Responsibilities

- `ContainerWindow` — a bare, titled, resizable top-level frame for a plugin's
  own editor to be attached to.
- `forward_key` — posting a key the plugin's window swallowed on to the DAW, so
  the space bar still reaches the transport.
- `watch` — running a plugin's timers, and on Linux its file descriptors,
  because a plugin has no event loop of its own to wait on. Both formats ask
  for this; only the words differ.
- `poll` / `pump_events` / `root_window` — the small amount of platform glue the
  above needs.

## Backends

One module each, under `src/`: `win32`, `x11`, and `stub` for what is left.
X11 rather than Wayland because that is what the plugin formats speak — VST3's
`X11EmbedWindowID` and CLAP's `x11` both name an X window id, and neither format
has a Wayland handle to hand over. Under a Wayland session the DAW and the
plugins are on XWayland for the same reason.

## Not this crate's job

- **Drawing anything.** This window has no renderer, no layout and no widgets;
  wgpu, Vello and a windowing crate are all beside the point.
- **Owning the event loop.** Inside a plugin the DAW owns it. `winit` does not
  fit for exactly that reason, and on Windows there is no loop to run at all — a
  window created on the DAW's UI thread has its messages delivered by the DAW's
  own pump. X11 is the exception: a connection is per-client and the DAW's is not
  ours, so nothing it pumps will ever deliver our events. `poll` is what turns
  that one, and it does nothing where the host is already doing the work.
- **Knowing a plugin format.** The formats disagree only about the *name* they
  give a platform handle, and that name belongs to the backend that speaks it.
  Both `vst3-host-view` and `clap-host` build on what is here.
- **Teardown order.** This window deliberately owns no plugin object; whichever
  crate owns both the window and the view writes the sequence.

## Invariants

- **`poll` runs no caller code.** An event only ever writes to a `WindowState`,
  so there is no turn of it that is unsafe to be inside of. Deferring work out
  of a draw callback is a real rule, but it belongs to whoever owns the plugin
  instance, not here — this crate used to carry a `Deferred` for it and the
  queue's owner turned out to be the wrong place for it to live.
- **`watch` never calls a plugin back with its list locked, and re-checks
  liveness before every callback.** A plugin is entitled to unregister from
  inside its own callback, including somebody else's registration. Both rules
  live here precisely so that the two backends cannot drift on them.
- **`WM_CLOSE` is recorded, not obeyed.** Destroying the window there would take
  the plugin's child window with it without the plugin ever being told.
- **The backend that is missing is an honest stub.** macOS returns an error
  rather than half-working.
- **The tests open real windows.** `cargo test` on X11 therefore needs a display;
  without one it fails saying so rather than passing vacuously. CI runs the Linux
  job under `xvfb-run`.
