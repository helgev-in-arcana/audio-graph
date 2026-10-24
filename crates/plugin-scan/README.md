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

## Catalogue and restoration

`catalogue::cached(path)` reads existing results without opening plugins. A
missing or malformed cache gives an empty list because the catalogue is derived
data, separate from the user's settings and saved plugin state.

Run `catalogue::refresh(directories, cache_path)` off the UI thread, with a
`plugin_host::init_thread()` guard alive on that thread. It loads new or changed
modules, retains unchanged results including scan errors, and removes entries
outside the current directory listing. A busy module is deferred rather than
cached as a permanent load failure. `catalogue::forget(path)` removes the cache
so the next refresh rescans unchanged modules too.

`reference_candidates(reference, known)` returns the existing path hint first,
then catalogue entries matching both format and class ID, preferring unchanged
files. It neither opens modules nor searches folders. The host must load each
candidate with the saved ID to verify it. A reference with no candidate remains
unresolved until the caller supplies more scan results.

Filesystem discovery examines each selected folder and one level below it.
It does not descend into plugin bundles or search arbitrary directory trees.
