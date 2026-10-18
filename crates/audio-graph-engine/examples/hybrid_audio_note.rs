use std::hint::black_box;
use std::time::Instant;

use audio_graph_engine::{
    AudioIn, AudioOut, AudioTremolo, Beats, Chunking, DelayRead, DelayWrite, Engine, FilterMode,
    Granularity, Graph, KeySwitchMode, MidiDelay, Mix, NodeKind, NoteFilter, NoteMerge, Plugin,
    PluginPorts, PortType, ProgramPublisher, Ratio, TremoloKeys, TremoloRow, compile,
};
use plugin_host::{Event, NoteEvent};
use subhost_adapter::{AudioChunk, AudioInstances, ScheduleView, SlotSchedule};

const PATHS: u8 = 6;
const SAMPLES: usize = 21;
const RATE: f64 = 48_000.0;

fn audio_graph(name: &str, channels: u16, looped: bool) -> Graph {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0; 2]);
    let input = graph.add(NodeKind::AudioIn(AudioIn { bus: 0, channels }), [0.0; 2]);
    let output = graph.add(NodeKind::AudioOut(AudioOut { bus: 0, channels }), [0.0; 2]);
    let mut source = input;
    let writer = if looped {
        let ty = PortType::Audio { channels };
        let read = graph.add(
            NodeKind::DelayRead(DelayRead {
                line: 0,
                ty,
                max_time: 0.1,
                time: 0.01,
            }),
            [0.0; 2],
        );
        let write = graph.add(NodeKind::DelayWrite(DelayWrite { line: 0, ty }), [0.0; 2]);
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels,
                inputs: 2,
                gains: vec![0.0, -18.0],
            }),
            [0.0; 2],
        );
        graph.connect(input, 0, mix, 0);
        graph.connect(read, 0, mix, 2);
        source = mix;
        Some(write)
    } else {
        None
    };
    let fanout = name == "audio_fanout";
    let mut last = source;
    let mut ends = Vec::new();
    for _ in 0..PATHS {
        let kind = if name == "audio_core" {
            NodeKind::Mix(Mix {
                channels,
                inputs: 1,
                gains: vec![-0.1],
            })
        } else {
            NodeKind::AudioTremolo(AudioTremolo {
                channels,
                keys: TremoloKeys {
                    mode: KeySwitchMode::Select,
                    rows: vec![TremoloRow {
                        key: 24,
                        step: Beats::new(1, 64),
                    }],
                    stop_key: 23,
                    share: Ratio::EVEN,
                },
                fade_in_ms: 0.25,
                fade_out_ms: 0.25,
            })
        };
        let node = graph.add(kind, [0.0; 2]);
        graph.connect(if fanout { source } else { last }, 0, node, 0);
        if name != "audio_core" {
            graph.connect(notes, 0, node, 1);
        }
        last = node;
        ends.push(node);
    }
    if fanout {
        let mix = graph.add(
            NodeKind::Mix(Mix {
                channels,
                inputs: PATHS,
                gains: vec![-18.0; PATHS as usize],
            }),
            [0.0; 2],
        );
        for (i, node) in ends.into_iter().enumerate() {
            graph.connect(node, 0, mix, i as u8 * 2);
        }
        last = mix;
    }
    graph.connect(last, 0, output, 0);
    if let Some(write) = writer {
        graph.connect(last, 0, write, 0);
    }
    graph
}

fn note_graph(name: &str) -> Graph {
    let mut graph = Graph::new();
    let notes = graph.add(NodeKind::NoteIn, [0.0; 2]);
    let merge = graph.add(NodeKind::NoteMerge(NoteMerge { inputs: PATHS }), [0.0; 2]);
    for path in 0..PATHS {
        let node = match name {
            "note_route" => notes,
            "note_filter" => graph.add(
                NodeKind::NoteFilter(NoteFilter {
                    channels: vec![0],
                    channel_mode: FilterMode::Keep,
                    controllers: vec![1],
                    controller_mode: FilterMode::Drop,
                }),
                [0.0; 2],
            ),
            "note_delay" => graph.add(
                NodeKind::MidiDelay(MidiDelay {
                    seconds: 0.003 + f64::from(path) / RATE,
                    beats: Beats::new(1, 64),
                    sync: false,
                }),
                [0.0; 2],
            ),
            _ => unreachable!(),
        };
        if node != notes {
            graph.connect(notes, 0, node, 0);
        }
        graph.connect(node, 0, merge, path);
    }
    let sink = graph.add(
        NodeKind::Plugin(Plugin {
            instance: 0,
            ports: PluginPorts {
                audio_out: vec![2],
                accepts_notes: true,
                ..PluginPorts::default()
            },
        }),
        [0.0; 2],
    );
    let output = graph.add(
        NodeKind::AudioOut(AudioOut {
            bus: 0,
            channels: 2,
        }),
        [0.0; 2],
    );
    graph.connect(merge, 0, sink, 0);
    graph.connect(sink, 0, output, 0);
    graph
}

fn events(pattern: &str, frames: u32, audio: bool) -> Vec<Event> {
    let pairs = match pattern {
        "idle" => 0,
        "periodic" => 1,
        "dense" => 16,
        _ => unreachable!(),
    };
    let mut events = Vec::new();
    for i in 0..pairs {
        let key = if audio { 24 } else { 60 + (i % 12) as i16 };
        let at = i * frames / pairs;
        events.push(Event::Note(NoteEvent::NoteOn {
            note_id: None,
            port: 0,
            channel: 0,
            key,
            velocity: 0.75,
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

struct Sink {
    record: bool,
    events: Vec<Event>,
}

impl AudioInstances for Sink {
    fn reports_note_end(&self, _instance: u32, _port: i16) -> bool {
        false
    }
    fn process(
        &mut self,
        _instance: u32,
        notes: &[Event],
        _input: &[f32],
        output: &mut [f32],
        _chunk: AudioChunk,
        _schedule: ScheduleView<'_>,
    ) {
        black_box(notes);
        if self.record {
            self.events.extend_from_slice(notes);
        }
        output.fill(0.0);
    }
}

fn hash(checksum: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *checksum ^= u64::from(byte);
        *checksum = checksum.wrapping_mul(0x100000001b3);
    }
}

fn measure(name: &str, channels: u16, frames: u32, quantum: u32, resolution: u32, pattern: &str) {
    let audio = name.starts_with("audio");
    let graph = if audio {
        audio_graph(name, channels, quantum != 0)
    } else {
        note_graph(name)
    };
    let publisher = ProgramPublisher::default();
    publisher.publish(compile(&graph, 0).unwrap(), RATE);
    let mut engine = Engine::new();
    engine.prepare(frames, &[channels]);
    assert!(engine.adopt(&publisher));
    assert_eq!(
        engine.chunking(),
        if quantum == 0 {
            Chunking::WholeBlock
        } else {
            Chunking::SubBlock
        }
    );
    let mut schedule = SlotSchedule::new(0, frames, resolution).unwrap();
    let input: Vec<f32> = (0..usize::from(channels) * frames as usize)
        .map(|i| ((i as f32 * 0.013).sin() * 0.25) + 0.01)
        .collect();
    let mut output = vec![0.0; input.len()];
    let events = events(pattern, frames, audio);
    let mut sink = Sink {
        record: true,
        events: Vec::with_capacity(4096),
    };
    let mut ended = Vec::with_capacity(4096);
    let mut checksum = 0xcbf29ce484222325u64;
    let mut event_count = 0usize;
    let mut block = 0usize;
    let mut run = |engine: &mut Engine, sink: &mut Sink| {
        let incoming = if audio && block % 16 != 0 {
            &[][..]
        } else {
            &events
        };
        assert!(engine.run_block(
            &mut schedule,
            &[],
            incoming,
            frames,
            Granularity {
                resolution,
                quantum: quantum.max(16)
            },
            RATE,
            120.0,
            &input,
            &mut output,
            sink
        ));
        engine.end_block(std::iter::empty(), &mut ended);
        if sink.record {
            for value in &output {
                assert!(value.is_finite());
                hash(&mut checksum, &value.to_bits().to_le_bytes());
            }
            event_count += sink.events.len();
            hash(&mut checksum, format!("{:?}", sink.events).as_bytes());
            hash(&mut checksum, format!("{ended:?}").as_bytes());
        }
        black_box(&output);
        ended.clear();
        sink.events.clear();
        block += 1;
    };
    for _ in 0..64 {
        run(&mut engine, &mut sink);
    }
    sink.record = false;
    let start = Instant::now();
    for _ in 0..64 {
        run(&mut engine, &mut sink);
    }
    let estimate = start.elapsed().as_nanos() as usize / 64;
    let iterations = (3_000_000 / estimate.max(1)).clamp(16, 8192) / 16 * 16;
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        for _ in 0..iterations {
            run(&mut engine, &mut sink);
        }
        samples.push(start.elapsed().as_nanos() as f64 / iterations as f64);
    }
    samples.sort_by(f64::total_cmp);
    println!(
        "{name},{channels},{frames},{quantum},{resolution},{pattern},{:.1},{:.1},{:.1},{iterations},{checksum:016x},{event_count}",
        samples[SAMPLES / 2],
        samples[SAMPLES / 10],
        samples[SAMPLES * 9 / 10]
    );
}

fn main() {
    eprintln!(
        "Engine run_block + end_block; 48 kHz; 6 nodes; 21 samples; ns/block; quantum 0 = whole block; other quantum = feedback loop"
    );
    println!(
        "fixture,channels,frames,quantum,resolution,events,median_ns,p10_ns,p90_ns,iterations,checksum,event_count"
    );
    for name in ["audio_core", "audio_chain", "audio_fanout"] {
        for frames in [64, 512] {
            for quantum in [0, 32, 16] {
                for pattern in ["idle", "periodic"] {
                    measure(name, 2, frames, quantum, 32, pattern);
                }
            }
        }
    }
    for name in ["audio_chain", "audio_fanout"] {
        for frames in [64, 512] {
            for pattern in ["idle", "periodic"] {
                measure(name, 1, frames, 0, 32, pattern);
            }
        }
    }
    for name in ["note_route", "note_filter", "note_delay"] {
        for resolution in [32, 1] {
            for pattern in ["idle", "periodic", "dense"] {
                measure(name, 2, 512, 0, resolution, pattern);
            }
        }
    }
}
