//! Standing in for the DAW: what a test needs to drive the wrapper.
//!
//! `Wrapper::activate` takes plain structs, but `Wrapper::process` wants
//! nice-plug's audio buffers and a `ProcessContext`, neither of which a host
//! hands out. Both are ordinary enough to build — a `Buffer` is a vector of
//! channel slices and the context is eight methods — and building them is the
//! difference between checking the wrapper's audio path here and checking it
//! only through a bundled binary.
//!
//! Shared by the test binaries beside it, so each of those says what it is
//! testing and nothing about how a plugin is loaded.

#![allow(dead_code)]

use std::cell::Cell;
use std::path::PathBuf;

use audio_graph_plugin::{Wrapper, WrapperFx};
use nice_plug::prelude::*;

/// Locates the built CLAP test fixture and copies it under a `.clap` name.
///
/// The facade infers the format from the extension and cargo's artefact is
/// named `.dll`, so it is copied rather than renamed: the original belongs to
/// cargo and the next build would replace it anyway.
///
/// `name` separates one test binary's copy from another's, so neither can write
/// over a file the other has loaded.
///
/// Panics when the fixture is missing rather than skipping, because a skip
/// would make a green run mean nothing.
pub fn fixture_as_clap(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary has a path");
    let build_dir = exe
        .parent()
        .and_then(std::path::Path::parent)
        .expect("the test binary is two levels below the build directory");
    let source = [
        "clap_test_plugin.dll",
        "libclap_test_plugin.so",
        "libclap_test_plugin.dylib",
    ]
    .iter()
    .map(|n| build_dir.join(n))
    .find(|p| p.is_file())
    .unwrap_or_else(|| {
        panic!(
            "clap-test-plugin is not in {}.\n\
             Run `cargo build --workspace` before `cargo test --workspace`: \
             cargo does not build another package's cdylib on its own.",
            build_dir.display()
        )
    });

    let target = build_dir.join(format!("{name}.clap"));
    std::fs::copy(&source, &target).expect("the fixture can be copied");
    target
}

/// The fixture's saved state: gain, offset, mode and latency, as little-endian
/// doubles in parameter order.
///
/// Mirrors the fixture's own format rather than importing it, the way the CLAP
/// backend's tests mirror its constants: a drift between the two should fail
/// the test rather than be papered over.
pub fn fixture_state(latency: f64) -> [u8; 32] {
    let mut blob = [0u8; 32];
    blob[..8].copy_from_slice(&1.0f64.to_le_bytes());
    blob[24..].copy_from_slice(&latency.to_le_bytes());
    blob
}

pub const STEREO: NonZeroU32 = new_nonzero_u32(2);

/// The effect form's layout, as the DAW hands it over.
pub fn fx_layout() -> AudioIOLayout {
    AudioIOLayout {
        main_input_channels: Some(STEREO),
        main_output_channels: Some(STEREO),
        aux_input_ports: &[STEREO],
        ..AudioIOLayout::const_default()
    }
}

/// Playing back at the desk.
pub const LIVE: BufferConfig = BufferConfig {
    sample_rate: 48_000.0,
    min_buffer_size: None,
    max_buffer_size: 512,
    process_mode: ProcessMode::Realtime,
};

/// The same project written to a file as fast as the machine manages. A larger
/// block is what a host asks for once it no longer has to keep up with a sound
/// card.
pub const BOUNCE: BufferConfig = BufferConfig {
    max_buffer_size: 4096,
    process_mode: ProcessMode::Offline,
    ..LIVE
};

/// The half of the DAW that is not audio: transport, notes, and the things the
/// wrapper can tell it.
pub struct Daw {
    pub transport: Transport,
    /// Note events to hand over, in order, one block's worth.
    pub incoming: Vec<NoteEvent<()>>,
    /// What the wrapper sent back.
    pub outgoing: Vec<NoteEvent<()>>,
    /// The latency last reported, and whether it was reported at all.
    ///
    /// A `Cell` because the wrapper reports through `&self`: a host takes this
    /// from the audio thread and is expected not to need a lock for it.
    pub latency: Cell<Option<u32>>,
}

impl Daw {
    pub fn playing() -> Daw {
        Daw {
            transport: Transport {
                playing: true,
                tempo: Some(120.0),
                pos_samples: Some(0),
                ..Transport::new(LIVE.sample_rate)
            },
            incoming: Vec::new(),
            outgoing: Vec::new(),
            latency: Cell::new(None),
        }
    }
}

impl ProcessContext<WrapperFx> for Daw {
    fn plugin_api(&self) -> PluginApi {
        PluginApi::Clap
    }

    fn execute_background(&self, _task: <WrapperFx as Plugin>::BackgroundTask) {}

    fn execute_gui(&self, _task: <WrapperFx as Plugin>::BackgroundTask) {}

    fn transport(&self) -> &Transport {
        &self.transport
    }

    fn next_event(&mut self) -> Option<NoteEvent<()>> {
        // Drained from the front, so the order the DAW put them in is the order
        // the wrapper sees.
        (!self.incoming.is_empty()).then(|| self.incoming.remove(0))
    }

    fn send_event(&mut self, event: NoteEvent<()>) {
        self.outgoing.push(event);
    }

    fn set_latency_samples(&self, samples: u32) {
        self.latency.set(Some(samples));
    }

    fn set_current_voice_capacity(&self, _capacity: u32) {}
}

/// One block of audio, owning the samples on both sides of the wrapper.
///
/// The main bus is in place, the way both plugin formats hand it over: what is
/// written here before the call is the input, and what is left here afterwards
/// is the output.
pub struct Block {
    main: Vec<Vec<f32>>,
    sidechain: Vec<Vec<f32>>,
    frames: usize,
}

impl Block {
    /// A block of `frames` samples, stereo on the main bus and on the
    /// sidechain, silent until something fills it.
    pub fn silent(frames: usize) -> Block {
        Block {
            main: vec![vec![0.0; frames]; 2],
            sidechain: vec![vec![0.0; frames]; 2],
            frames,
        }
    }

    /// Put the same value in every sample of the main bus.
    ///
    /// A signal whose peak survives any amount of routing, so a test can tell
    /// "the audio came through" from "something silenced it" without a word
    /// about phase or alignment.
    pub fn fill(&mut self, level: f32) -> &mut Block {
        for channel in &mut self.main {
            channel.fill(level);
        }
        self
    }

    /// The loudest sample anywhere on the main bus.
    pub fn peak(&self) -> f32 {
        self.main
            .iter()
            .flat_map(|channel| channel.iter())
            .fold(0.0f32, |peak, sample| peak.max(sample.abs()))
    }

    /// Hand the block to the wrapper, as a host's process callback does.
    pub fn process(&mut self, wrapper: &mut Wrapper, daw: &mut Daw) {
        // Destructured first: the two buffers borrow different fields, and
        // reaching through `self` for the second while the first is holding a
        // slice would be a second mutable borrow of the whole block.
        let Block {
            main,
            sidechain,
            frames,
        } = self;
        let frames = *frames;

        let mut buffer = Buffer::default();
        // SAFETY: the slices point into `main`, which outlives the call below,
        // and every channel is `frames` long by construction.
        unsafe {
            buffer.set_slices(frames, |slices| {
                slices.clear();
                slices.extend(main.iter_mut().map(|channel| &mut channel[..frames]));
            });
        }

        let mut side = Buffer::default();
        // SAFETY: as above, into `sidechain`.
        unsafe {
            side.set_slices(frames, |slices| {
                slices.clear();
                slices.extend(sidechain.iter_mut().map(|channel| &mut channel[..frames]));
            });
        }

        let mut inputs = [side];
        let mut aux = AuxiliaryBuffers {
            inputs: &mut inputs,
            outputs: &mut [],
        };
        wrapper.process::<WrapperFx>(&mut buffer, &mut aux, daw);
    }
}
