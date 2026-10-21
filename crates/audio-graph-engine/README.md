# audio-graph-engine

The node graph: what turns the wrapper into an instrument of its own.

Audio, MIDI, and parameter signals are connected in one graph. Constants, LFOs,
note followers, and audio followers produce values for plugin parameters and
audio controls; `Slot In` reads automation supplied by the wrapper. Plugin
parameter sockets use direct schedule lanes as well as the wrapper's bound
slots. Nothing here depends on VST3 or CLAP types.

## Responsibilities

- The edit graph: nodes, links, positions, and the patch as it is saved.
- Compiling that graph into a flat `Program` — pruning, ordering, register and
  buffer allocation, latency compensation, note routing.
- Preparing a compiled `Program` for the active sample rate, then running the
  resulting `PreparedProgram` on the audio thread while holding state that has
  to survive a recompile.
- The node set itself, one file per node, including each node's own editor
  controls behind the `ui` feature.

## Not this crate's job

- **What a plugin is.** A plugin node reaches its sub-plugin only through
  `subhost_adapter::AudioInstances`: an instance number, note events, a schedule
  view, and flat audio slices. It never learns whether a VST3 or a CLAP answered.
- **What a slot is bound to.** `Slot In` reads a supplied slot value. The
  wrapper and adapter own slot bindings and map direct plugin parameter lanes.
- **Deciding the numbers.** How many slots exist, how many instances a patch may
  hold: the wrapper's, handed in.
- **The canvas.** Panning, zooming, drawing links, the add-node menu, loading
  plugins — all `audio-graph-plugin`'s. A node is handed a `Ui` the right size
  and a narrow set of facts about the world outside the graph.

## Layout

The crate is split along the one line that matters, the thread boundary:

| Module     | Side   | What it is                                                            |
| ---------- | ------ | --------------------------------------------------------------------- |
| `graph`    | main   | The edit side. Freely mutable, serialisable, allowed to be nonsense in the middle of an edit. |
| `compile`  | main   | Turns a `Graph` into a `Program` — flat, ordered, checked.            |
| `handoff`  | both   | Carries the program down to the audio thread and the old one back up, without a lock in either direction. |
| `ir`       | main   | Owns compiled and prepared programs and their publisher.             |
| `engine`   | audio  | Runs a `PreparedProgram`, allocating nothing and freeing nothing.     |
| `nodes`    | main / prepared kernels | Node data, compilation, optional controls, and node-local parameter, audio, and MIDI processing. |
| `notes`    | audio  | Note identity, delivery ownership, bounded voice storage, and completion. |

## Execution and editor integration

`Granularity` separates parameter `resolution` from audio-loop `quantum`.
Parameter rows are evaluated at the chosen resolution; audio stages outside a
feedback loop can process the whole host block. Audio delay lines have a minimum
of one quantum, while parameter delay lines have a minimum of one parameter row.
Changing resolution also requires the caller to prepare hosted processors for
the resulting event capacity.

The built-in node catalogue includes MIDI Split, Merge, Delay and Tremolo,
Audio Math and Tremolo, and Held Keys. The full user-facing descriptions belong
in [Nodes and Usage](../../docs/nodes-usage.md). Key Param Select uses a
`ParamKernel`; Audio Tremolo and MIDI Delay use typed custom processors. The
[execution ADR](../../dev/adr/2026-10-17_ir-dyn-hybrid-execution/README.md)
records the design and measurements behind those choices.

The default feature set is headless. Enable `ui` for `NodeUi`, the catalogue,
node controls, and `node_style`. A canvas applies `node_style` before scaling
for zoom so the controls fit `NODE_WIDTH`; the crate does not create a window.
Live socket values are supplied by the embedding editor through `NodeUi`.

The saved `Graph` preserves unknown nodes and connections. Compilation excludes
unreadable nodes and their connected routes without deleting their saved data.
The embedding product decides how to present the compatibility warning.

## Invariants

### The thread boundary is a module boundary

What reaches the audio side is a `PreparedProgram` and nothing else. The
`engine` module must not mention `graph` or a node kind outside of its tests,
which live in `engine/tests.rs` and test-only modules.

**A `use crate::graph::…` in any other file under `engine/` is the signal that
something has leaked across.** That is the cheapest way to check the invariant
by eye, and it is worth checking in review.

Parameter execution combines core `Op` instructions with prepared
`ParamKernel` objects. Each custom kernel is compiled alongside its node and
receives only local input lookups, a dedicated output slice, its own optional
latch value and a read-only note view. Global register and state IDs stay in
the compiled bindings. Inputs precede newly allocated outputs, so the evaluator
can borrow both without copying values or using unsafe code.

Kernel settings are immutable. Mutable processing state stays in `Engine`, so
the normal end-row snapshot applies to parameter kernels as well. Their boxes
are created during compilation and reclaimed through `ProgramPublisher`.

Audio custom processors borrow either an in-place buffer or disjoint input and
output regions. Each channel keeps the host block's stride even when processing
a shorter chunk. No sample gathering is needed to construct the local view.

Note custom processors borrow this row's input events and a capacity-limited
output writer. Timing and lanes are shared once per row, while parameter
lookups remain local to each node. Buffer accumulation and held-note tracking
belong to the evaluator. End-row previews evaluate parameters only.

Prepared custom processors pair immutable settings with a concrete state type.
The engine adopts their vectors and matches state by node identity and processor
type. Type checking occurs during adoption, not during evaluation. Unmatched
state releases any retained notes, and retired boxes return with the prepared
program for destruction off-thread. Reset behavior belongs to each state type;
adding a processor does not require another state branch in `Engine`.

Releasing the program keeps engine state for subsequent adoption. Active
custom processors therefore remain in the engine until they are replaced or
the engine is dropped on the main thread, like its built-in state tables.

### The audio side allocates nothing

Buffer pools are sized in `Engine::new` and `prepare`; custom state is allocated
with its processor on the publishing side. Adopting a program transfers these
allocations and retained state without resizing. No allocation, no locking,
and no `Drop` of anything the main thread handed over.

`ProgramPublisher` owns the only handoff used by the product. It prepares rings
on the non-realtime side and transfers matching rings from an unadopted
superseded value before publishing a replacement. The audio side only adopts a
`PreparedProgram`; it cannot construct one or mutate its IR.

`Engine::run_block` is the standard block entry point. It initializes the lane
schedule, ingests notes once, evaluates parameter and audio stages in order, and
passes a short-lived `ScheduleView` to each sub-plugin call. The caller only
adapts host buffers and binds the sub-plugin processors.

### Some state has to survive a program swap

Recompiling happens on every drag of every control. State that represents where
a running process *is* — LFO phases, delay ring contents, latch registers, the
current note expression values — is carried across the swap rather than reset. An
oscillator that restarted on every recompile would make the editor unusable for
exactly the thing an LFO is for.

Note buffers are matched by their producer, upstream stream, and static filter
conditions. Matching streams retain held notes and the previous block's event
tail; new or changed buffers start empty. Buffer storage and boundary indices
move together without resizing. Controller emitters resend their current value
after adoption. This matching runs only when a new program is adopted.

### The dependency on `subhost-adapter` points this way on purpose

A plugin node has something behind it, but this crate only ever sees it through
`subhost_adapter::AudioInstances`: an instance number, note events, a schedule
view, and flat audio slices. `subhost-adapter` is the general crate and this one is
AudioGraph's, so this one does the depending — never the reverse.

## Verification

From the repository root:

```sh
cargo test -p audio-graph-engine --locked
cargo test -p audio-graph-engine --features ui --locked
```

The first run checks the configuration used by the CLI. A workspace test alone
does not cover it because `audio-graph-plugin` enables `ui` through feature
unification. The second run also checks node control geometry without opening
a native window. The `hybrid_keyparam` and `hybrid_audio_note` examples provide
the performance workloads referenced by the ADR.
