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

`SubPluginMain` and `SubPluginProcessor` are separate on purpose, and `activate`
hands out the processor **by value**. Calling `process` on an inactive plugin is
therefore a compile error rather than a rule written down somewhere, and the
configuration cannot change while a processor exists.

`SubPluginMain` is deliberately not `Send`: both formats pin these calls to the
thread that created the instance.

### The host's services are injected, never assumed

A backend never builds its own host object — `vst3-host` does not construct an
`IHostApplication`, it receives a `HostContext`. That keeps "forwarded from the
DAW" out of the core vocabulary entirely, so a standalone scanner and the nested
wrapper are expressed by the same types.
