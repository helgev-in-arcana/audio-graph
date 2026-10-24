# clap-test-plugin

A deterministic CLAP plugin, built as a `cdylib`, so that `clap-host` and
`plugin-host` can be tested against something whose every output is predictable.

## Responsibilities

- Exporting real CLAP entry points and extensions — params, audio ports, note
  ports, state, voice info, render mode, latency, GUI — so the host exercises the
  same code paths a third-party plugin would.
- Behaving arithmetically, so a test can assert on exact numbers rather than on
  "something changed": `out = in * gain + offset`, a fixed sidechain gain, a
  fixed level added per note-on, a scaled aux output, a configurable latency.
- Serialising every parameter into its state blob, so a save/restore round trip
  is checkable to the bit.

## Not this crate's job

- **Being a useful audio plugin.** Nothing here is meant to be listened to.
- **Covering every corner of CLAP.** An extension is added when a test needs it.
- **Holding the assertions.** Those live in the host crates' tests; this is the
  fixture they point at.

## Invariants

- **Every output is a closed-form function of the inputs.** Anything with
  internal state a test cannot predict does not belong here.
- **The fixture must exist for the tests to mean anything.** Its absence is a
  panic, not a skip: a green run with the fixture missing would be a green run
  that proved nothing.
- **Cargo does not build another package's `cdylib` on its own.** Run
  `cargo build --workspace --locked` before `cargo test --workspace --locked`.

The fixture also exposes controls for metadata changes, event-capacity failures,
callbacks, and native resource lifetime. Tests in `clap-host`, `plugin-host`,
`plugin-scan`, `subhost-adapter`, and `audio-graph-plugin` use those controls to
check the surrounding layers. Test helpers load isolated module copies where
module-global state must not be shared between cases.

No installed third-party plugin or `AUDIO_GRAPH_TEST_PLUGINS` setting is needed
for these fixture tests. GUI cases require Win32 or an X server; Linux CI uses
Xvfb. This crate is a test dependency and is not bundled as a product plugin.
