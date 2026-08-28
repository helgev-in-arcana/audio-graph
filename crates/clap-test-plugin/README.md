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
  `cargo build --workspace` before `cargo test --workspace`.
