//! Running a `Program`, on the audio thread.
//!
//! The rules this file exists to keep (§9.1): no allocation, no locking, no
//! `Drop` of anything the main thread gave us. Every buffer is sized once in
//! [`Engine::new`] against the compiler's ceilings, so adopting a new program
//! is a pointer swap and a short loop, never a resize.
//!
//! It also holds the state that must *survive* a swap — LFO phases and the
//! current note expression values. Recompiling happens on every drag of every
//! control, and an oscillator that restarted each time would make the editor
//! unusable for exactly the thing an LFO is for.

use plugin_host_api::{NoteEvent, NoteExpression};

use crate::graph::{ExprSource, MathOp, Waveform};
use crate::handoff::Handoff;
use crate::program::{MAX_LFOS, MAX_REGISTERS, Op, Operand, Program, RateSpec};

/// What the graph is being evaluated against for one sub-block.
#[derive(Debug, Clone, Copy)]
pub struct BlockContext {
    pub sample_rate: f64,
    pub tempo_bpm: f64,
    /// How many samples this evaluation covers. Phases advance by this much
    /// afterwards, which is what makes the sub-block rate (§9.2) a property of
    /// the caller rather than of the engine.
    pub frames: u32,
}

/// The per-note controllers, flattened to one value each.
///
/// v1 is monophonic (see [`ExprSource`]): the graph has one value per source,
/// and the newest note wins. Nothing here assumes it will stay that way — the
/// wrapper feeds note events in, and a per-voice engine would feed the same
/// events into more of these.
#[derive(Debug, Clone, Copy)]
struct Expressions {
    /// Indexed by `NoteExpression as usize`.
    values: [f64; 7],
    velocity: f64,
    key: f64,
    held: u32,
}

impl Default for Expressions {
    fn default() -> Self {
        Expressions {
            // Centres, not zeros: pan and tuning are signed, and a graph
            // reading Pan before any note has arrived should read "middle".
            values: [1.0, 0.5, 0.0, 0.0, 0.0, 0.0, 0.0],
            velocity: 0.0,
            key: 0.5,
            held: 0,
        }
    }
}

fn expression_index(kind: NoteExpression) -> usize {
    match kind {
        NoteExpression::Volume => 0,
        NoteExpression::Pan => 1,
        NoteExpression::Tuning => 2,
        NoteExpression::Vibrato => 3,
        NoteExpression::Expression => 4,
        NoteExpression::Brightness => 5,
        NoteExpression::Pressure => 6,
    }
}

pub struct Engine {
    program: Option<Box<Program>>,
    registers: Vec<f64>,
    /// LFO phase, 0..1, per state index of the current program.
    phases: Vec<f64>,
    /// Sample-and-hold values, per state index.
    holds: Vec<f64>,
    /// Scratch for carrying phases across a program swap. Sized once so the
    /// swap itself allocates nothing.
    carry: Vec<(u32, f64, f64)>,
    /// Which node each phase belongs to, mirrored from the program so the swap
    /// can match old state to new without touching the old program again.
    phase_nodes: Vec<u32>,
    expressions: Expressions,
    rng: u32,
}

impl Default for Engine {
    fn default() -> Self {
        Engine::new()
    }
}

impl Engine {
    pub fn new() -> Engine {
        Engine {
            program: None,
            registers: vec![0.0; MAX_REGISTERS],
            phases: vec![0.0; MAX_LFOS],
            holds: vec![0.0; MAX_LFOS],
            carry: vec![(0, 0.0, 0.0); MAX_LFOS],
            phase_nodes: vec![u32::MAX; MAX_LFOS],
            expressions: Expressions::default(),
            // Any odd seed; the sequence only has to be uncorrelated, not
            // unpredictable.
            rng: 0x2545_F491,
        }
    }

    /// Whether the graph currently drives `slot`, and so overrides the DAW.
    pub fn drives(&self, slot: usize) -> bool {
        self.program.as_ref().is_some_and(|p| p.drives(slot))
    }

    pub fn has_program(&self) -> bool {
        self.program.as_ref().is_some_and(|p| !p.is_empty())
    }

    /// Pick up a newly compiled program, if one is waiting.
    ///
    /// Returns whether anything changed. Non-blocking and non-allocating: the
    /// old program goes back to the main thread to be freed (§9.1).
    pub fn adopt(&mut self, handoff: &Handoff<Program>) -> bool {
        // Remember which node each running phase belongs to *before* the swap;
        // afterwards the old program is gone.
        let live = self.phase_nodes.len().min(self.phases.len());
        for i in 0..live {
            self.carry[i] = (self.phase_nodes[i], self.phases[i], self.holds[i]);
        }
        let carried = self
            .program
            .as_ref()
            .map_or(0, |p| p.lfo_nodes.len().min(MAX_LFOS));

        if !handoff.take(&mut self.program) {
            return false;
        }
        let next = self.program.as_ref().expect("take reported a swap");

        for (i, &node) in next.lfo_nodes.iter().take(MAX_LFOS).enumerate() {
            // Linear over at most MAX_LFOS entries. A map would be faster in
            // theory and an allocation in practice.
            match self.carry[..carried].iter().find(|&&(id, _, _)| id == node) {
                Some(&(_, phase, hold)) => {
                    self.phases[i] = phase;
                    self.holds[i] = hold;
                }
                None => {
                    self.phases[i] = 0.0;
                    self.holds[i] = 0.0;
                }
            }
            self.phase_nodes[i] = node;
        }
        for i in next.lfo_nodes.len()..MAX_LFOS {
            self.phase_nodes[i] = u32::MAX;
        }
        true
    }

    /// Give back whatever program is loaded, so the main thread can free it.
    ///
    /// Called when the plugin is torn down, from the main thread.
    pub fn release(&mut self) -> Option<Box<Program>> {
        self.program.take()
    }

    /// Fold one note event into the expression state.
    pub fn note(&mut self, event: &NoteEvent) {
        match *event {
            NoteEvent::NoteOn { key, velocity, .. } => {
                self.expressions.velocity = velocity;
                self.expressions.key = f64::from(key).clamp(0.0, 127.0) / 127.0;
                self.expressions.held = self.expressions.held.saturating_add(1);
            }
            NoteEvent::NoteOff { .. } | NoteEvent::NoteEnd { .. } => {
                self.expressions.held = self.expressions.held.saturating_sub(1);
            }
            NoteEvent::Expression {
                expression, value, ..
            } => {
                self.expressions.values[expression_index(expression)] = value;
            }
            NoteEvent::Midi { .. } => {}
        }
    }

    /// Forget every held note. Called on `reset`, where the DAW has told us the
    /// transport jumped and any note-offs we were waiting for will never come.
    pub fn reset(&mut self) {
        self.expressions = Expressions::default();
        self.phases.iter_mut().for_each(|p| *p = 0.0);
    }

    /// Evaluate the program for one sub-block.
    ///
    /// `slots` comes in holding the DAW's automation and goes out with the
    /// graph's outputs written over the slots it drives. Slots the graph does
    /// not touch keep the DAW's value, which is what makes a wrapper with an
    /// empty graph behave exactly as it did before M5.
    pub fn run(&mut self, ctx: &BlockContext, slots: &mut [f64]) {
        // Moved out and put back rather than borrowed: the ops write into
        // `self.registers`, and holding a `&self.program` across that would be
        // a borrow conflict for no benefit at run time.
        let Some(program) = self.program.take() else {
            return;
        };
        if program.is_empty() {
            self.program = Some(program);
            return;
        }

        let dt = if ctx.sample_rate > 0.0 {
            f64::from(ctx.frames) / ctx.sample_rate
        } else {
            0.0
        };

        for op in &program.ops {
            match *op {
                Op::Const { out, value } => self.registers[out as usize] = value,
                Op::Slot { out, slot } => {
                    self.registers[out as usize] = slots.get(slot as usize).copied().unwrap_or(0.0)
                }
                Op::Expr { out, source } => {
                    self.registers[out as usize] = read_expression(&self.expressions, source)
                }
                Op::Lfo {
                    out,
                    state,
                    waveform,
                    rate,
                    offset_phase,
                    depth,
                    centre,
                } => {
                    let i = state as usize;
                    let phase = (self.phases[i] + offset_phase).rem_euclid(1.0);
                    let shape = match waveform {
                        Waveform::Sine => (phase * std::f64::consts::TAU).sin(),
                        Waveform::Triangle => 1.0 - 4.0 * (phase - 0.5).abs(),
                        Waveform::Saw => 2.0 * phase - 1.0,
                        Waveform::Square => {
                            if phase < 0.5 {
                                -1.0
                            } else {
                                1.0
                            }
                        }
                        Waveform::Random => self.holds[i],
                    };
                    self.registers[out as usize] = centre + depth * shape;

                    let hz = match rate {
                        RateSpec::Hz(hz) => hz,
                        RateSpec::CyclesPerBeat(cpb) => cpb * ctx.tempo_bpm / 60.0,
                    };
                    let advanced = self.phases[i] + hz * dt;
                    if advanced >= 1.0 && waveform == Waveform::Random {
                        // A new value at each cycle boundary. Scaled to -1..1
                        // so it lands in the same range as the other shapes.
                        self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        self.holds[i] = f64::from(self.rng >> 8) / f64::from(1u32 << 23) - 1.0;
                    }
                    self.phases[i] = advanced.rem_euclid(1.0);
                }
                Op::Math { out, a, b, op } => {
                    let a = self.registers[a as usize];
                    let b = match b {
                        Operand::Reg(reg) => self.registers[reg as usize],
                        Operand::Value(value) => value,
                    };
                    self.registers[out as usize] = match op {
                        MathOp::Add => a + b,
                        MathOp::Subtract => a - b,
                        MathOp::Multiply => a * b,
                        MathOp::Min => a.min(b),
                        MathOp::Max => a.max(b),
                        // Guarded: a zero or negative exponent on a zero input
                        // is an infinity, and an infinity that reaches a
                        // third-party plugin's parameter is a crash waiting to
                        // be blamed on the plugin.
                        MathOp::Curve => a.clamp(0.0, 1.0).powf(b.clamp(0.01, 100.0)),
                    };
                }
                Op::Range {
                    out,
                    a,
                    in_lo,
                    in_span,
                    out_lo,
                    out_span,
                    clamp,
                } => {
                    let value = self.registers[a as usize];
                    let t = if in_span == 0.0 {
                        0.0
                    } else {
                        (value - in_lo) / in_span
                    };
                    let t = if clamp { t.clamp(0.0, 1.0) } else { t };
                    self.registers[out as usize] = out_lo + t * out_span;
                }
            }
        }

        for &(slot, reg) in &program.outputs {
            if let Some(target) = slots.get_mut(slot as usize) {
                // Slots are 0..1 by definition (§8.1); the mapping onto the
                // sub-plugin's plain range belongs to the slot table. A NaN
                // from a degenerate graph must not get past here.
                let value = self.registers[reg as usize];
                *target = if value.is_finite() {
                    value.clamp(0.0, 1.0)
                } else {
                    0.0
                };
            }
        }

        self.program = Some(program);
    }
}

fn read_expression(state: &Expressions, source: ExprSource) -> f64 {
    match source {
        ExprSource::Volume => state.values[0],
        ExprSource::Pan => state.values[1],
        ExprSource::Tuning => state.values[2],
        ExprSource::Vibrato => state.values[3],
        ExprSource::Expression => state.values[4],
        ExprSource::Brightness => state.values[5],
        ExprSource::Pressure => state.values[6],
        ExprSource::Velocity => state.velocity,
        ExprSource::Gate => f64::from(u8::from(state.held > 0)),
        ExprSource::KeyTrack => state.key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::compile;
    use crate::graph::{Graph, NodeKind, Rate};

    const SLOTS: usize = 32;

    fn ctx(frames: u32) -> BlockContext {
        BlockContext {
            sample_rate: 48_000.0,
            tempo_bpm: 120.0,
            frames,
        }
    }

    fn load(engine: &mut Engine, graph: &Graph) {
        let handoff = Handoff::new();
        handoff.send(Box::new(compile(graph, SLOTS).unwrap()));
        assert!(engine.adopt(&handoff));
    }

    #[test]
    fn a_slot_the_graph_does_not_drive_keeps_the_daws_value() {
        let mut graph = Graph::new();
        let c = graph.add(NodeKind::Constant { value: 0.25 }, [0.0, 0.0]);
        let out = graph.add(NodeKind::SlotOut { slot: 0 }, [0.0, 0.0]);
        graph.connect(c, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = vec![0.9; SLOTS];
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[0], 0.25);
        assert_eq!(slots[1], 0.9, "an undriven slot is left alone");
    }

    #[test]
    fn the_daws_automation_can_be_read_shaped_and_written_back() {
        let mut graph = Graph::new();
        let input = graph.add(NodeKind::SlotIn { slot: 3 }, [0.0, 0.0]);
        let half = graph.add(
            NodeKind::Math {
                op: MathOp::Multiply,
                b: 0.5,
            },
            [0.0, 0.0],
        );
        let out = graph.add(NodeKind::SlotOut { slot: 4 }, [0.0, 0.0]);
        graph.connect(input, half, 0);
        graph.connect(half, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = vec![0.0; SLOTS];
        slots[3] = 0.8;
        engine.run(&ctx(32), &mut slots);
        assert!((slots[4] - 0.4).abs() < 1e-12);
    }

    #[test]
    fn an_lfo_sweeps_and_comes_back() {
        let mut graph = Graph::new();
        let lfo = graph.add(
            NodeKind::Lfo {
                waveform: Waveform::Saw,
                rate: Rate::Hz(1.0),
                phase: 0.0,
                depth: 0.5,
                offset: 0.5,
            },
            [0.0, 0.0],
        );
        let out = graph.add(NodeKind::SlotOut { slot: 0 }, [0.0, 0.0]);
        graph.connect(lfo, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = vec![0.0; SLOTS];
        let mut seen: Vec<f64> = Vec::new();
        // One second at 48 kHz in 32-sample sub-blocks: a whole cycle.
        for _ in 0..1500 {
            engine.run(&ctx(32), &mut slots);
            seen.push(slots[0]);
        }
        let lowest = seen.iter().cloned().fold(f64::INFINITY, f64::min);
        let highest = seen.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        assert!(lowest < 0.02, "a saw should reach the bottom, got {lowest}");
        assert!(highest > 0.98, "a saw should reach the top, got {highest}");
        assert!(seen.iter().all(|v| (0.0..=1.0).contains(v)));
    }

    #[test]
    fn tempo_sync_follows_the_host() {
        let mut graph = Graph::new();
        let lfo = graph.add(
            NodeKind::Lfo {
                waveform: Waveform::Saw,
                // One cycle per beat: at 120 bpm that is 2 Hz.
                rate: Rate::Beats(1.0),
                phase: 0.0,
                depth: 0.5,
                offset: 0.5,
            },
            [0.0, 0.0],
        );
        let out = graph.add(NodeKind::SlotOut { slot: 0 }, [0.0, 0.0]);
        graph.connect(lfo, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        // Quarter of a beat at 120 bpm = 0.125 s = 6000 samples.
        let mut slots = vec![0.0; SLOTS];
        engine.run(
            &BlockContext {
                sample_rate: 48_000.0,
                tempo_bpm: 120.0,
                frames: 6000,
            },
            &mut slots,
        );
        engine.run(
            &BlockContext {
                sample_rate: 48_000.0,
                tempo_bpm: 120.0,
                frames: 1,
            },
            &mut slots,
        );
        assert!(
            (slots[0] - 0.25).abs() < 1e-3,
            "expected a quarter cycle, got {}",
            slots[0]
        );
    }

    #[test]
    fn recompiling_does_not_restart_a_running_lfo() {
        let mut graph = Graph::new();
        let lfo = graph.add(
            NodeKind::Lfo {
                waveform: Waveform::Saw,
                rate: Rate::Hz(1.0),
                phase: 0.0,
                depth: 0.5,
                offset: 0.5,
            },
            [0.0, 0.0],
        );
        let out = graph.add(NodeKind::SlotOut { slot: 0 }, [0.0, 0.0]);
        graph.connect(lfo, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = vec![0.0; SLOTS];
        for _ in 0..200 {
            engine.run(&ctx(32), &mut slots);
        }
        let before = slots[0];
        assert!(before > 0.05, "the LFO should have moved by now");

        // Something unrelated changes — a new node appears — and the graph is
        // recompiled, as it is on every edit.
        graph.add(NodeKind::Constant { value: 0.0 }, [0.0, 0.0]);
        load(&mut engine, &graph);
        engine.run(&ctx(1), &mut slots);

        assert!(
            (slots[0] - before).abs() < 0.01,
            "the phase jumped across a recompile: {before} -> {}",
            slots[0]
        );
    }

    #[test]
    fn note_expression_reaches_the_graph() {
        let mut graph = Graph::new();
        let expr = graph.add(
            NodeKind::Expression {
                source: ExprSource::Pressure,
            },
            [0.0, 0.0],
        );
        let out = graph.add(NodeKind::SlotOut { slot: 7 }, [0.0, 0.0]);
        graph.connect(expr, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);

        let mut slots = vec![0.0; SLOTS];
        engine.note(&NoteEvent::Expression {
            note_id: 1,
            port: 0,
            channel: 0,
            key: 60,
            expression: NoteExpression::Pressure,
            value: 0.7,
            sample_offset: 0,
        });
        engine.run(&ctx(32), &mut slots);
        assert!((slots[7] - 0.7).abs() < 1e-12);
    }

    #[test]
    fn the_gate_follows_held_notes() {
        let mut graph = Graph::new();
        let gate = graph.add(
            NodeKind::Expression {
                source: ExprSource::Gate,
            },
            [0.0, 0.0],
        );
        let out = graph.add(NodeKind::SlotOut { slot: 0 }, [0.0, 0.0]);
        graph.connect(gate, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);
        let mut slots = vec![0.0; SLOTS];

        let on = |key: i16| NoteEvent::NoteOn {
            note_id: key as i32,
            port: 0,
            channel: 0,
            key,
            velocity: 1.0,
            sample_offset: 0,
        };
        let off = |key: i16| NoteEvent::NoteOff {
            note_id: key as i32,
            port: 0,
            channel: 0,
            key,
            velocity: 0.0,
            sample_offset: 0,
        };

        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[0], 0.0);

        engine.note(&on(60));
        engine.note(&on(64));
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[0], 1.0);

        // Releasing one of two held notes must not drop the gate.
        engine.note(&off(60));
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[0], 1.0);

        engine.note(&off(64));
        engine.run(&ctx(32), &mut slots);
        assert_eq!(slots[0], 0.0);
    }

    #[test]
    fn a_degenerate_graph_cannot_hand_a_nan_to_the_sub_plugin() {
        let mut graph = Graph::new();
        let a = graph.add(NodeKind::Constant { value: 0.0 }, [0.0, 0.0]);
        let div = graph.add(
            NodeKind::Math {
                op: MathOp::Curve,
                b: 0.0,
            },
            [0.0, 0.0],
        );
        let out = graph.add(NodeKind::SlotOut { slot: 0 }, [0.0, 0.0]);
        graph.connect(a, div, 0);
        graph.connect(div, out, 0);

        let mut engine = Engine::new();
        load(&mut engine, &graph);
        let mut slots = vec![0.5; SLOTS];
        engine.run(&ctx(32), &mut slots);
        assert!(slots[0].is_finite());
        assert!((0.0..=1.0).contains(&slots[0]));
    }

    #[test]
    fn an_engine_with_no_program_leaves_everything_alone() {
        let mut engine = Engine::new();
        let mut slots = vec![0.3; SLOTS];
        engine.run(&ctx(32), &mut slots);
        assert!(slots.iter().all(|&v| v == 0.3));
        assert!(!engine.has_program());
    }
}
