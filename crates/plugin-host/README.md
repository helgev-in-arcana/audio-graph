# plugin-host

`plugin-host` is the format-neutral hosting and scanning facade. It receives
scan directories and catalogue paths from its caller; AudioGraph's persistent
folders, pins, and `config.json` live in `audio-graph-settings`.

One facade over both plugin backends. A caller says "load this path", "give me
its parameters", "open its editor" and never learns which format answered.

## Responsibilities

- **Anything whose answer differs by format.** Where plugins live on disk, how a
  module is enumerated, how an instance is created, how an editor is attached.
- **`Plugin`** — one loaded sub-plugin, whichever format it came in, including
  module lifetime and the order things must be torn down in.
- **`Format`** — the closed set of formats, and the stable tags a saved project
  holds.
- **`scan` / `catalogue`** — finding installed modules and remembering what is
  inside them between runs. Product settings are owned by
  `audio-graph-settings`.
- **`MainThread` / `Processor` / `reclaim_main_thread`** — the common lifetime and
  owner-thread destruction contract, re-exported from `plugin-host-api` so
  backends and their callers use the same return path.
- Re-exporting `plugin-host-api` wholesale, so a caller needs one dependency
  instead of two.

## Not this crate's job

- **Hosting a plugin inside another plugin** — forwarding the DAW's transport,
  combining latency, publishing automatable slots, nesting state. That is
  `subhost-adapter`.
- **Defining the data model.** `plugin-host-api` owns it; nothing is added to it
  here.
- **Talking to a specific format.** `vst3-host`, `vst3-host-view` and `clap-host`
  do that; this crate is the arm that chooses between them.
- **Windows.** `host-window` owns the container window, the deferred queue and
  key forwarding.

The test for whether something belongs here: **would a standalone offline
renderer or a plugin scanner still need it?** If yes, it belongs here or below.
If it only makes sense because a DAW is above us, it belongs in
`subhost-adapter`.

## Invariants

- **A saved reference is `(format, plugin_id, path_hint)`, and the id is the
  authority.** Plugin folders differ between machines; a missing path triggers a
  search rather than a failure.
- **The catalogue is derived data.** Deleting the caller-selected cache file
  costs a rescan. The caller owns its settings independently of that file.
- **Enumerating a module means loading third-party code.** `installed_modules`
  returns paths only; anything that opens a module says so and expects to be
  called off the UI thread.
