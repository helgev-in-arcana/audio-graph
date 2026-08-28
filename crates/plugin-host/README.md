# plugin-host

One facade over both plugin backends. A caller says "load this path", "give me
its parameters", "open its editor" and never learns which format answered.

## Responsibilities

- **Anything whose answer differs by format.** Where plugins live on disk, how a
  module is enumerated, how an instance is created, how an editor is attached.
- **`Plugin`** — one loaded sub-plugin, whichever format it came in, including
  module lifetime and the order things must be torn down in.
- **`Format`** — the closed set of formats, and the stable tags a saved project
  holds.
- **`scan` / `catalogue` / `config`** — finding installed modules, remembering
  what is inside them between runs, and the user's list of folders to look in.
- **`MainThread`** — the thread-affinity container. It lives here because the
  rule it encodes (a controller call is pinned to the thread that created the
  instance) is a *format's* rule.
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
- **The catalogue is derived data.** Deleting `plugins.json` costs a rescan and
  nothing else, which is why it is kept beside the settings rather than inside
  them — a corrupt cache must not take the user's folder list with it.
- **The config file is the whole answer, not an addition to the conventions.**
  The OS-conventional folders are seeded into it on first run and are the user's
  to remove from then on. Re-seeding keys off the file being absent, never off
  the list being short, so "scan nothing" stays a thing a user can ask for.
- **Enumerating a module means loading third-party code.** `installed_modules`
  returns paths only; anything that opens a module says so and expects to be
  called off the UI thread.
