# subhost-adapter

Everything specific to hosting a plugin from *inside* another plugin.

## Scope, defined by subtraction

**Downward.** If a standalone offline renderer or a plugin scanner would still
need a piece of code, it belongs in `plugin-host`, not here. What is left is the
nesting itself: forwarding the DAW's transport down, combining latency on the
way up, publishing slots the DAW can automate and binding them to the
sub-plugin's own parameters, nesting one plugin's state inside another's, and
deciding what to do with the sub-plugin's edit notifications.

**Upward.** Nothing here knows what AudioGraph is. The wrapper above decides how
many slots to publish, how many lanes a sub-block carries and what its saved
document looks like, and hands those in (`SubHostConfig`, `SlotSchedule`,
`SubHostState`). A different wrapper — a chain, a rack, a bare pair of plugins —
makes different choices and gets the same crate.

## Responsibilities

- Loading, unloading and re-finding sub-plugins, and holding the loaded ones.
- The slot table: the parameters the wrapper publishes to the DAW, and their
  bindings to sub-plugin parameters.
- The sub-block schedule those values travel in.
- Forwarding the DAW's transport down and combining latency on the way up.
- Turning slot and lane values into sample-accurate parameter events, merged in
  order with the DAW's own, per chunk.
- Preserving each child's identity in notifications and output events.
- Nesting one plugin's opaque state inside another's.

## Not this crate's job

- **Anything a standalone renderer or a scanner would also need.** That is
  `plugin-host`. This crate is only the nesting.
- **Anything AudioGraph-specific.** Slot counts, lane counts and the saved
  document's shape are handed in by whatever wrapper is above.
- **Scheduling audio.** The caller decides when each instance runs and what it
  hears; this crate answers.

## Invariants

### `AudioInstances` is the line between scheduling and hosting

A caller owns a graph, a chain, a rack: it decides *when* each sub-plugin runs
and *what* it hears. It has no idea what is at the other end of one, and never
learns whether a VST3 or a CLAP answered.

Audio and note events cross as flat slices, with a `Copy` value describing the
audio chunk. `ScheduleView` provides read-only access to the parameter rows for
one processing call. A bound processor never stores it, so the graph can update
the next parameter stage as soon as the call returns. The caller decides which
notes each instance receives; the adapter maps its schedule lanes to plugin
parameters.

### The instance table is sparse, and stays sparse

Callers name an instance by index. Missing plugins retain their document entry
and opaque state; only their native handle is absent. Such a slot is reserved
until explicitly removed with `unload`. Entries are never renumbered. A state
entry beyond this build's instance limit is retained without allocating a sparse
native table up to its index.

The index identifies a slot, not the lifetime of its current occupant.
Each processor retains its own activation. `SubHostProcessors::deactivate`
returns those activations directly, so unloading or replacing a slot cannot
redirect an outstanding processor's return into another instance. A failed group
activation also returns all processors created before the failure.

`load_state` takes caller-selected search folders. Failed restoration retains
the original state and leaves the native plugin unloaded, so a default preset
cannot overwrite it. A failed save retains the last successful blob. The saved
document format is independent of whether a native instance is available today.

### A binding outlives what it points at

Slot bindings are stored; their *resolution* against a loaded plugin is derived
state, recomputed whenever a sub-plugin changes. Losing a plugin — a missing
file, a failed load, a moved folder — must never delete the binding, because
reloading the plugin has to bring the mapping back. Bindings are keyed on
`(instance, plugin_id, param_id)`: parameter order is not stable across plugin
versions, and `instance` is what keeps two copies of one plugin apart.

The table owned by `SubHost` keeps the slot count from `SubHostConfig`, because
direct parameter lanes start immediately after those slots. `slots()` exposes
a read-only table; `bind_slot`, `clear_slot`, and `rename_slot` edit individual
entries. Saved slot tables of other lengths are resized to the configured count
by `load_state`. Binding changes take effect in newly activated processors;
callers must rebuild their processors to adopt them.

### The audio side allocates nothing

`SubHostConfig`'s three numbers are ceilings, not guidance. Instance tables,
event scratch buffers and the slot schedule are all sized at activate, and
`process` may not grow any of them.

`activate` validates lane and instance dimensions before preparing native
processors. `SlotSchedule::new`, `begin`, and `ScheduleView::from_parts` return
errors for unrepresentable capacities or inconsistent shapes. A block beyond
the schedule's capacity is rejected instead of partially covered. Zero lanes
are valid for a host that supplies only incoming events.
Changing quantum updates the row count without reallocating; callers fill the
new grid before processing. Existing values are not automatically resampled.

### One parameter has one effective scheduled input

`SubHostConfig::target_priority` selects `PreferDirect`, `PreferSlots`, or
`RejectConflicts`. Within the preferred source, the last lane wins. Arbitration
happens during preparation, before deduplication, so an unchanged preferred
value cannot be overridden by a changing lower-priority lane. The caller selects
the policy; the adapter does not decide whether a graph or a DAW should win.

### Child output retains its identity

`SubHostContext` receives an `InstanceId` with each notification. Native backends
still receive their single-plugin `HostContext`, wrapped by the adapter.
`InstanceId` includes the slot index and a runtime generation. `SubHost::source`
identifies its current occupant; `SubHostProcessor::source` identifies the one
retained by that processor, even after replacement.

`SubHostProcessors::bind` takes an `InstanceEventSink` containing tagged events
from every child and chunk. Native output uses preallocated scratch before it
is copied into tagged storage. Capacity loss remains visible through the sink's
overflow flag. Callers choose how to route or combine the outputs. Callers using
`get_mut` and the single-processor API keep the native `EventSink` and can attach
the processor's source themselves.

Transport supplied to `bind` or `SubHostProcessor::process` describes the start
of the parent block. The adapter advances it to each chunk's start using the
activation's sample rate, preserving stopped positions and wrapping known loop
bounds. Tempo and meter are treated as constant within that parent block.
