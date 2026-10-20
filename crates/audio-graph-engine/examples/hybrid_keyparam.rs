use std::hint::black_box;
use std::time::Instant;

use audio_graph_engine::{
    Engine, Granularity, Graph, KeyParam, KeyParamMode, Math, MathOp, NodeKind, ParamPort, Plugin,
    PluginPorts, ProgramPublisher, SlotIn, compile,
};
use plugin_host::{Event, NoteEvent};
use subhost_adapter::{NoInstances, SlotSchedule};

const SLOTS: usize = 32;
const PATHS: usize = 12;
const SAMPLES: usize = 21;

fn graph(name: &str) -> Graph {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0; 2]);
    let sink = graph.add(
        NodeKind::Plugin(Plugin {
            instance: 0,
            ports: PluginPorts {
                accepts_notes: true,
                params: (0..PATHS)
                    .map(|id| ParamPort {
                        id: id as u32,
                        name: format!("p{id}"),
                    })
                    .collect(),
                ..PluginPorts::default()
            },
        }),
        [0.0; 2],
    );
    graph.connect(notes, 0, sink, 0);
    for path in 0..PATHS {
        let input = graph.add(NodeKind::SlotIn(SlotIn { slot: path }), [0.0; 2]);
        let custom = name != "core" && (name != "mixed" || path % 2 == 0);
        let result = if custom {
            let count = if name == "key2" { 2 } else { 8 };
            let node = graph.add(
                NodeKind::KeyParam(KeyParam {
                    mode: if path % 2 == 0 {
                        KeyParamMode::Select
                    } else {
                        KeyParamMode::Toggle
                    },
                    keys: (60..60 + count).collect(),
                    values: (0..count).map(|i| f64::from(i + 1) / 10.0).collect(),
                    mute_keys: name == "key8_muted",
                }),
                [0.0; 2],
            );
            graph.connect(notes, 0, node, 0);
            graph.connect(input, 0, node, 1);
            if path == 0 {
                graph.connect(node, 1, sink, 0);
            }
            node
        } else {
            let mut previous = input;
            for _ in 0..4 {
                let node = graph.add(
                    NodeKind::Math(Math {
                        op: MathOp::Multiply,
                        b: 0.9,
                    }),
                    [0.0; 2],
                );
                graph.connect(previous, 0, node, 0);
                previous = node;
            }
            previous
        };
        graph.connect(result, 0, sink, path as u8 + 1);
    }
    graph
}

fn events(pattern: &str, frames: u32) -> Vec<Event> {
    let pairs = match pattern {
        "idle" => 0,
        "periodic" => 1,
        "dense" => 16,
        _ => unreachable!(),
    };
    let mut events = Vec::new();
    for i in 0..pairs {
        let key = 60 + (i % 8) as i16;
        let at = i * frames / pairs;
        events.push(Event::Note(NoteEvent::NoteOn {
            note_id: None,
            port: 0,
            channel: 0,
            key,
            velocity: 1.0,
            sample_offset: at,
        }));
        events.push(Event::Note(NoteEvent::NoteOff {
            note_id: None,
            port: 0,
            channel: 0,
            key,
            velocity: 0.0,
            sample_offset: at + frames / pairs / 2,
        }));
    }
    events
}

fn stats(mut samples: Vec<f64>) -> (f64, f64, f64) {
    samples.sort_by(f64::total_cmp);
    (
        samples[SAMPLES / 2],
        samples[SAMPLES / 10],
        samples[SAMPLES * 9 / 10],
    )
}

fn output_checksum(graph: &Graph, frames: u32, resolution: u32, events: &[Event]) -> u64 {
    let publisher = ProgramPublisher::default();
    publisher.publish(compile(graph, SLOTS).unwrap(), 48_000.0);
    let mut engine = Engine::new();
    engine.prepare(frames, &[]);
    assert!(engine.adopt(&publisher));
    let mut schedule = SlotSchedule::new(SLOTS + PATHS, frames, resolution).unwrap();
    let mut ended = Vec::with_capacity(256);
    let mut checksum = 0xcbf29ce484222325u64;
    for _ in 0..64 {
        assert!(engine.run_block(
            &mut schedule,
            &[0.5; SLOTS],
            events,
            frames,
            Granularity {
                resolution,
                quantum: 32
            },
            48_000.0,
            120.0,
            &[],
            &mut [],
            &mut NoInstances,
        ));
        for value in schedule.rows().iter().chain(schedule.end_row().unwrap()) {
            assert!(value.is_finite());
            checksum ^= value.to_bits();
            checksum = checksum.wrapping_mul(0x100000001b3);
        }
        engine.end_block(std::iter::empty(), &mut ended);
        ended.clear();
    }
    checksum
}

fn main() {
    println!(
        "actual Engine::run_block + end_block; 48kHz; 12 independent output paths; 21 samples; ns/block"
    );
    println!("fixture,frames,resolution,events,median_ns,p10_ns,p90_ns,iterations,checksum");
    for name in ["core", "key2", "key8", "mixed", "key8_muted"] {
        let graph = graph(name);
        let program = compile(&graph, SLOTS).unwrap();
        let dump = format!("{program:#?}");
        let register_line = dump
            .lines()
            .find(|line| line.starts_with("    registers:"))
            .unwrap();
        let ops = dump
            .split("    ops: [\n")
            .nth(1)
            .unwrap()
            .split("    ],")
            .next()
            .unwrap()
            .lines()
            .filter(|line| {
                line.starts_with("        ")
                    && !line.starts_with("         ")
                    && !line.trim_start().starts_with('}')
            })
            .count();
        let mut compile_samples = Vec::new();
        for _ in 0..SAMPLES {
            let start = Instant::now();
            for _ in 0..16 {
                drop(black_box(compile(black_box(&graph), SLOTS).unwrap()));
            }
            compile_samples.push(start.elapsed().as_nanos() as f64 / 16.0);
        }
        let (median, p10, p90) = stats(compile_samples);
        eprintln!(
            "{name}: parameter_ops={ops}, {}, compile_and_drop_ns median={median:.0} p10={p10:.0} p90={p90:.0}",
            register_line.trim().trim_end_matches(',')
        );
        for frames in [64, 512] {
            for resolution in [32, 4, 1] {
                for pattern in ["idle", "periodic", "dense"] {
                    let events = events(pattern, frames);
                    let checksum = output_checksum(&graph, frames, resolution, &events);
                    let publisher = ProgramPublisher::default();
                    publisher.publish(compile(&graph, SLOTS).unwrap(), 48_000.0);
                    let mut engine = Engine::new();
                    engine.prepare(frames, &[]);
                    assert!(engine.adopt(&publisher));
                    let mut schedule =
                        SlotSchedule::new(SLOTS + PATHS, frames, resolution).unwrap();
                    let daw_slots = [0.5; SLOTS];
                    let mut ended = Vec::with_capacity(256);
                    let mut run = || {
                        assert!(engine.run_block(
                            &mut schedule,
                            &daw_slots,
                            &events,
                            frames,
                            Granularity {
                                resolution,
                                quantum: 32
                            },
                            48_000.0,
                            120.0,
                            &[],
                            &mut [],
                            &mut NoInstances,
                        ));
                        engine.end_block(std::iter::empty(), &mut ended);
                        black_box(schedule.end_row());
                        ended.clear();
                    };
                    let start = Instant::now();
                    for _ in 0..64 {
                        run();
                    }
                    let estimate = start.elapsed().as_nanos() as usize / 64;
                    let iterations = (3_000_000 / estimate.max(1)).clamp(16, 8192);
                    let mut samples = Vec::with_capacity(SAMPLES);
                    for _ in 0..SAMPLES {
                        let start = Instant::now();
                        for _ in 0..iterations {
                            run();
                        }
                        samples.push(start.elapsed().as_nanos() as f64 / iterations as f64);
                    }
                    let (median, p10, p90) = stats(samples);
                    println!(
                        "{name},{frames},{resolution},{pattern},{median:.1},{p10:.1},{p90:.1},{iterations},{checksum:016x}"
                    );
                }
            }
        }
    }
}
