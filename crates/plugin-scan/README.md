# plugin-scan

Finding plugins, above `plugin-host`:

- **`plugin_directories` / `find_modules` / `installed_modules`** — the modules in
  the folders the caller names. Paths only: nothing is opened.
- **`catalogue`** — what each module contains, as last scanned, cached at a path the
  caller chooses and invalidated by file stamps. Refreshing it opens modules, so it
  belongs off the UI thread.
- **`reference_candidates`** — where a saved `(format, id, path_hint)` may be, from
  the hint and the catalogue, without opening anything.

Which folders to look in is never decided here: `audio-graph-settings` owns the
conventional ones and the user's list.

`plugin-host` loads one module it is given and needs none of this; a caller that
only ever loads known paths does not depend on this crate.
