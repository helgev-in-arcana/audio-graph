# clap-host

A CLAP host backend in pure Rust. The sibling of `vst3-host`, with the same
boundaries drawn in the same places.

## Responsibilities

- Loading a `.clap` module, reading its factory, and enumerating descriptors.
- Instantiating a plugin and driving its lifecycle: `init`, `activate`,
  `start_processing`, `process`, and the reverse on the way out.
- Parameter, audio-port and note-port metadata; state, latency, voice info,
  render mode, GUI, timers, thread checks, logging, and Linux POSIX descriptor
  callbacks.
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

- **Activation returns an owning `plugin_host_api::Processor`.** It retains the
  `ClapProcessor` and native resources needed on the audio thread. The
  `ClapPlugin` main-thread surface stays with its owner; returning the processor
  uses the shared owner-thread reclamation contract.
- **`HostShim` is reachable from both threads on purpose.** CLAP marks
  `request_restart` and its neighbours thread-safe, so every field is either
  immutable after construction or an atomic or a mutex.
- **`tick` runs whether or not an editor is open.** `on_main_thread` and the
  timer extension exist regardless of what is on screen, and a plugin starved of
  them stalls its own worker.
- **CLAP sleep maps to `ProcessStatus::Silent`.** Continuation and tail statuses
  keep processing, while errors return `ProcessStatus::Error`. A silent block
  alone does not imply that a plugin will stay silent.

Input batches must fit `AudioConfig::max_input_events` and be ordered within
the current block. Invalid batches are rejected without partial delivery.
Output events append to the caller's bounded `EventSink`, with overflow reported
through its sticky flag. `reset()` clears processing state without rewinding
CLAP's continuously advancing `steady_time`.

Hosted editor support follows [host-window](../host-window/README.md#backends).
The [deterministic CLAP fixture](../clap-test-plugin/README.md) covers audio,
events, state, callbacks, and GUI lifetime. Build it before running the tests:

```sh
cargo build --workspace --locked
cargo test -p clap-host --locked
```

Run Linux window tests under an X server, for example with `xvfb-run -a`.
