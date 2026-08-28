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
- Note routing at the point where a stream's *name* becomes events.
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

**Everything crossing that trait is a flat slice or a `Copy` value.** No
pointers, no borrows into caller-owned structures. This is what keeps the
boundary workable if a sub-plugin is ever moved into a separate process, where
neither could cross. It is also why a note stream crosses as a *name* plus a key
mask rather than as an event buffer: the caller routes notes without knowing
what a note is, and this crate turns the name into events.

### The instance table is sparse, and stays sparse

Callers name an instance by index. An entry whose plugin has gone stays empty
rather than being closed up, because renumbering would repoint every binding
after it. This holds for `SubHost::instances`, `SubHostProcessors::entries`, and
the saved `InstanceState` list alike.

### A binding outlives what it points at

Slot bindings are stored; their *resolution* against a loaded plugin is derived
state, recomputed whenever a sub-plugin changes. Losing a plugin — a missing
file, a failed load, a moved folder — must never delete the binding, because
reloading the plugin has to bring the mapping back. Bindings are keyed on
`(instance, plugin_id, param_id)`: parameter order is not stable across plugin
versions, and `instance` is what keeps two copies of one plugin apart.

### The audio side allocates nothing

`SubHostConfig`'s three numbers are ceilings, not guidance. Instance tables,
event scratch buffers and the slot schedule are all sized at activate, and
`process` may not grow any of them.
