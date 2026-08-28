# audio-graph-engine

The node graph: what turns the wrapper into an instrument of its own.

Constants, LFOs and note expressions are combined into values that drive the
wrapper's slots, and the slots drive the sub-plugin's parameters. Nothing in
this crate knows what a VST3 is or what a slot is bound to — it reads numbers
and writes numbers, and the outer layers decide what those numbers mean.

## Responsibilities

- The edit graph: nodes, links, positions, and the patch as it is saved.
- Compiling that graph into a flat `Program` — pruning, ordering, register and
  buffer allocation, latency compensation, note routing.
- Running a `Program` on the audio thread, and holding the state that has to
  survive a recompile.
- The node set itself, one file per node, including each node's own editor
  controls behind the `ui` feature.

## Not this crate's job

- **What a plugin is.** A plugin node reaches its sub-plugin only through
  `subhost_adapter::AudioInstances`: an instance number, a note stream's name,
  and two flat slices. It never learns whether a VST3 or a CLAP answered.
- **What a slot is bound to.** A node reads a slot and a node writes a slot;
  the binding lives outside the graph.
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
| `engine`   | audio  | Runs a `Program`, allocating nothing and freeing nothing.             |

## Invariants

### The thread boundary is a module boundary

What reaches the audio side is a `Program` and nothing else. `engine.rs` must
not mention `graph` or a node kind outside of its own tests.

**A `use crate::graph::…` appearing above the `#[cfg(test)]` line in `engine.rs`
is the signal that something has leaked across.** That is the cheapest way to
check the invariant by eye, and it is worth checking in review.

### The audio side allocates nothing

Every buffer the engine uses is sized once in `Engine::new`, against the
compiler's ceilings. Adopting a new program is a pointer swap and a short loop,
never a resize. No allocation, no locking, and no `Drop` of anything the main
thread handed over.

### Some state has to survive a program swap

Recompiling happens on every drag of every control. State that represents where
a running process *is* — LFO phases, delay ring contents, latch registers, the
current note expression values — is carried across the swap rather than reset. An
oscillator that restarted on every recompile would make the editor unusable for
exactly the thing an LFO is for.

### The dependency on `subhost-adapter` points this way on purpose

A plugin node has something behind it, but this crate only ever sees it through
`subhost_adapter::AudioInstances`: an instance number, a note stream's *name*,
and two flat slices. `subhost-adapter` is the general crate and this one is
AudioGraph's, so this one does the depending — never the reverse.
