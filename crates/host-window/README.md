# host-window

Window plumbing for hosting somebody else's editor, with no plugin format in
sight.

## Responsibilities

- `ContainerWindow` — a bare, titled, resizable top-level frame for a plugin's
  own editor to be attached to.
- `Deferred` — running work on the next turn of the host's message loop.
- `forward_key` — posting a key the plugin's window swallowed on to the DAW, so
  the space bar still reaches the transport.
- `pump_events` / `root_window` — the small amount of platform glue the above
  needs.

## Not this crate's job

- **Drawing anything.** This window has no renderer, no layout and no widgets;
  wgpu, Vello and a windowing crate are all beside the point.
- **Owning the event loop.** Inside a plugin the DAW owns it. `winit` does not
  fit for exactly that reason, and on Windows there is no loop to run at all — a
  window created on the DAW's UI thread has its messages delivered by the DAW's
  own pump.
- **Knowing a plugin format.** The formats disagree only about the *name* they
  give a platform handle, and that name belongs to the backend that speaks it.
  Both `vst3-host-view` and `clap-host` build on what is here.
- **Teardown order.** This window deliberately owns no plugin object; whichever
  crate owns both the window and the view writes the sequence.

## Invariants

- **A draw callback may only record what the user asked for.** Creating,
  showing or destroying a window dispatches messages synchronously, and the
  message lands back inside the GUI toolkit mid-frame — with egui-baseview that
  is a `RefCell` borrow violation inside a callback that cannot unwind, so the
  process dies rather than panicking. `Deferred` is where the recorded work
  goes.
- **`Deferred` carries one-shot work only.** A periodic tick built on a Win32
  timer silently meant no tick at all on the platforms whose backend is still a
  stub; that job belongs to whatever owns the plugin instance.
- **`WM_CLOSE` is recorded, not obeyed.** Destroying the window there would take
  the plugin's child window with it without the plugin ever being told.
- **The non-Windows backend is an honest stub.** It returns an error rather than
  half-working.
