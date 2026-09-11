# plugin-host-api

The format-agnostic data model and traits both plugin backends implement. This
crate is dependency-free, and every other crate in the workspace depends on it.

## Responsibilities

- The vocabulary both backends and every caller share: parameters, events, note
  expressions, audio buffers, bus layouts, transport, capabilities, errors.
- The traits a backend implements (`SubPluginMain`, `SubPluginProcessor`) and
  the one a host implements (`HostContext`).
- Staying dependency-free, because every other crate in the workspace depends on
  this one.

## Not this crate's job

- **Loading anything.** There is no I/O here, no dynamic library handling, no
  filesystem.
- **Knowing which format is in play.** No VST3 or CLAP type appears in this
  crate; `Format` itself lives in `plugin-host`.
- **Nesting.** No transport forwarding, no latency arithmetic, no slot tables.
- **Windows.** `host-window` owns those.

## Invariants

### The model is shaped after the richer format, not the intersection

CLAP is the richer of the two formats, and the vocabulary here is deliberately
shaped after it. VST3 backends *degrade* to this model; the model is never
narrowed to what both formats can express.

Concretely: `SetValue` and `Modulate` stay separate variants because CLAP keeps
`PARAM_VALUE` and `PARAM_MOD` apart and modulation is non-destructive.
Collapsing them here would delete that capability from every backend, including
the one that has it. The VST3 backend flattens them back together in its own
layer, where the loss belongs.

Parameters are likewise carried as plain values with an explicit range rather
than normalised to `0..1`. Normalising in the core would bake VST3's poverty in:
CLAP's stepped and enum semantics do not survive that round trip. Backends
normalise on the way out instead.

Where a format genuinely has nothing to offer, the answer is `None`, never a
guess — `VoiceInfo` comes from CLAP's `voice-info` and a VST3 sub-plugin reports
`None`.

### Nothing that cannot cross a process boundary appears in a public signature

No `ComPtr`, no raw pointers, no references or `Arc` in payloads, no callbacks.
`HostError` is a flat owned enum for the same reason. This is what keeps an
out-of-process backend a drop-in substitution rather than a rewrite.

Two consequences worth stating outright:

- **Audio buffers are flat, not slice-of-slices.** A nested slice cannot live in
  shared memory. One region per direction, main bus first and each aux bus
  packed after it.
- **`AudioConfig` and `AuxBuses` are `Copy` and carry no pointer**, which is why
  `MAX_AUX_BUSES` is a fixed ceiling rather than a `Vec`.

### There are no single-shot getters

Reads are batched by construction: `params()`, `snapshot()`, `io_layout()` each
return everything in one round trip, and there is no `param(id)` or per-bus
accessor anywhere in the API. This is not a convenience — it is what stops the
boundary from becoming chatty enough that IPC stops being viable.

### Main-thread and audio-thread surfaces are different traits

`SubPluginMain` and `SubPluginProcessor` are separate. `activate` returns an owned
`Processor` that retains the native instance, module, and callbacks needed for
processing. `Processor::deactivate` consumes that activation directly; there is
no separate destination instance to confuse with the instance that created it.
An active instance rejects another activation and state restoration.

`SubPluginMain` is deliberately not `Send`: both formats pin these calls to the
thread that created the instance.

`MainThread<T>` restricts both access and destruction to its creation thread.
Releasing it, or a `Processor`, from another thread only marks a preallocated
return record. Hosts call `reclaim_main_thread()` from their main-thread pump
and during shutdown. Returning a processor on its owner thread reclaims it
immediately. Processing uses its retained trait pointer directly, without
reference-count updates or registry lookups.

The owner thread drains returned resources when it exits. Resources still held
by other threads are retained if their owner exits first: destroying them on an
arbitrary thread would violate their contract. A normal shutdown returns active
processors and reclaims them before the owner thread or its host module exits.

### The host's services are injected, never assumed

A backend never builds its own host object — `vst3-host` does not construct an
`IHostApplication`, it receives a `HostContext`. That keeps "forwarded from the
DAW" out of the core vocabulary entirely, so a standalone scanner and the nested
wrapper are expressed by the same types.

### Processing and metadata contracts

Both backends validate `AudioConfig` at activation and check every `AudioBuffers`
view against that configuration before constructing native pointers. Mismatched
blocks return `Error` with cleared audio; zero-frame blocks do not call native DSP.

Prepare `EventSink::with_capacity` off the audio thread. `push` never grows it;
failure is sticky in `overflowed()` until the caller clears the collection interval.
Backends append call-relative events and propagate native output capacity loss.
The caller owns timestamp rebasing and recovery from incomplete output.

Call `SubPluginMain::tick` on main even without an editor. Native restart requests
are recorded and delivered there; callbacks schedule work instead of reentering
the plugin. `refresh_metadata` reports `Unchanged`, `Refreshed`, or
`NeedsDeactivation`. Return the processor and retry when required, then rebuild
from the updated descriptors before activating. Failed updates remain pending.
`io_layout` only reads; request desired main widths explicitly with
`request_main_bus_channels` while inactive.

`NoteEnd` represents native voice completion. `note_end_ports` identifies input
ports that supply it. A caller's note-off reclamation policy for other ports must
track actual deliveries; it must not fabricate native completion events.
