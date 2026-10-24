# audio-graph-settings

AudioGraph's product settings: the per-user `config.json`, plugin folders,
pinned module paths, and the adjacent derived catalogue path. Both the wrapper
and the development CLI use this crate; generic plugin hosts do not decide
where a user's plugins should live.

## Storage

| Platform | Default settings file |
| --- | --- |
| Windows | `%LOCALAPPDATA%\AudioGraph\config.json` |
| macOS | `~/Library/Application Support/AudioGraph/config.json` |
| Linux | `$XDG_CONFIG_HOME/audio-graph/config.json`, or `~/.config/audio-graph/config.json` |

`AUDIO_GRAPH_CONFIG` overrides the complete settings-file path. `catalogue_path()`
returns `plugins.json` beside that file. The catalogue is derived scan data
managed by [`plugin-scan`](../plugin-scan/README.md), not a field in the settings.
DAW project data and plugin state blobs are owned by the wrapper and adapter.

## Directory and persistence rules

- The first load seeds an absent file with existing conventional VST3/CLAP
  directories, including `CLAP_PATH`. After that, the saved list is authoritative.
- An explicitly empty list stays empty. `restore_defaults()` appends the
  conventional directories without removing the user's additions.
- Every listed directory is scanned for both supported formats. Pins refer to
  module paths; browser categories and ordering belong to the editor.
- A process-wide cache is shared by instances. Loads check the file's modified
  timestamp so edits from another process can be picked up.
- Malformed JSON loads as an empty/default configuration without overwriting
  the original. Before a subsequent save, it is preserved beside the target as
  `config.json.unreadable-<timestamp>`. A failed preservation aborts that save.
- `store()` updates the in-memory configuration even if persistence fails and
  returns an error for the caller to display. Successful writes use a synced
  temporary file and rename it over the destination.

`cargo run -p host-cli -- dirs` prints the effective directories and settings
path. Like opening the editor, this can create the initial settings file.
Tests use `AUDIO_GRAPH_CONFIG` to avoid modifying the user's profile.
