# clap-host

A CLAP host backend in pure Rust. The sibling of `vst3-host`, with the same
boundaries drawn in the same places.

## Responsibilities

- Loading a `.clap` module, reading its factory, and enumerating descriptors.
- Instantiating a plugin and driving its lifecycle: `init`, `activate`,
  `start_processing`, `process`, and the reverse on the way out.
- The extensions this host uses: params, audio-ports, note-ports, state,
  latency, voice-info, render, timer-support, gui.
- Translating between CLAP's vocabulary and the shared one in `plugin-host-api`
  — mostly a matter of shape, since the shared model is CLAP-shaped by design.
- Embedding the plugin's GUI, which in CLAP is an extension on the instance
  rather than a separate object.

## Not this crate's job

- **Choosing between formats.** That is `plugin-host`.
- **Anything about nesting** — transport forwarding, latency arithmetic, slot
  tables. This crate does not know a DAW is above it.
- **Providing host services.** They arrive through
  `plugin_host_api::HostContext`.
- **The container window.** `host-window` owns it; this crate uses it.

## Invariants

- **`ClapProcessor` is the only half that crosses to the audio thread.** CLAP
  designates `process` as the audio-thread call, and the two-trait split is what
  guarantees the main-thread half stays behind.
- **`HostShim` is reachable from both threads on purpose.** CLAP marks
  `request_restart` and its neighbours thread-safe, so every field is either
  immutable after construction or an atomic or a mutex.
- **`tick` runs whether or not an editor is open.** `on_main_thread` and the
  timer extension exist regardless of what is on screen, and a plugin starved of
  them stalls its own worker.
- **A `Sleep` process status means silent and staying silent.** Every other
  status means the tail is still running; collapsing the two would cut reverbs
  off.
