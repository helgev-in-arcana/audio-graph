# plugin-module

The parts of loading a plugin module that do not depend on its format, shared by
`vst3-host` and `clap-host`:

- **`Library`** — opening a shared library (`LoadLibraryExW` / `dlopen`) and looking
  up its symbols. It knows nothing about entry points; each format balances its own.
- **`bundle_binary`** — the shared library inside a bundle directory, given the
  subdirectory of `Contents/` the format keeps it in.
- **`ModuleLease`** — one binary belongs to one thread at a time, process-wide.
- **`Loaded`** — the per-thread table that makes opening the same module twice
  return the one already open, so a format's entry point runs once.
- **`list_modules` / `find_modules`** — the modules with a given extension in a
  folder and the folders one level down.

Which folders to look in is not here: that is the caller's to say.

`vst3-host` and `clap-host` use the loader and lifetime helpers. `plugin-scan`
also uses the filesystem discovery helpers; listing paths does not open their
libraries. `plugin-host` chooses the format and exposes the hosting facade.

`list_modules` only examines the named directory. `find_modules` also examines
its immediate subdirectories, treating bundle directories as modules rather
than descending into them. Neither function performs a recursive disk scan.

The lease identifies a binary by its canonical path when available. Opening a
module owned by another thread returns `HostError::ModuleBusy`; loading must be
retried after that owner releases it, rather than calling the native entry point
concurrently.
