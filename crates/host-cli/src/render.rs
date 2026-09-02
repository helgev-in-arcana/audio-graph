//! Offline audio rendering utilities for plugin testing.

use std::path::Path;
use std::sync::Arc;

use plugin_host::{
    AudioBuffers, AudioConfig, BufferLayout, ClassInfo, Event, EventSink, NoteEvent, Plugin,
    ProcessStatus, SubPluginMain, TimeContext,
};

use crate::host::CliHost;
use crate::wav::Audio;

/// Pick the class to instantiate: an explicit id, otherwise the module's first.
///
/// The id is whatever the format calls one — a VST3 class id in hex, a CLAP
/// reverse-DNS name — so the same argument works for both and this never has to
/// ask which format it is looking at.
pub fn choose_class(path: &Path, wanted: Option<&str>) -> Result<ClassInfo, String> {
    let classes = plugin_host::scan_module(path).map_err(|e| e.to_string())?;
    match wanted.filter(|t| !t.is_empty()) {
        Some(text) => classes
            .into_iter()
            .find(|c| c.id == text)
            .ok_or_else(|| format!("no plugin with id {text} in {}", path.display())),
        None => classes
            .into_iter()
            .next()
            .ok_or_else(|| format!("{} exports no plugin", path.display())),
    }
}

/// Load `path`, choosing `wanted` or its first plugin, and say which it was.
///
/// The pair is what nearly every command here needs, and putting it in one
/// place is what keeps the CLI from knowing which format it just loaded.
pub fn load(
    path: &Path,
    wanted: Option<&str>,
    host: Arc<dyn plugin_host::HostContext>,
) -> Result<(ClassInfo, Plugin), String> {
    let class = choose_class(path, wanted)?;
    let plugin = Plugin::load(path, Some(&class.id), host).map_err(|e| e.to_string())?;
    Ok((class, plugin))
}

/// Main-bus widths, as a harness has to activate with them: `(in, out)`.
///
/// The plugin's declared shape before negotiation, which is the only thing a
/// caller can ask for — and, for CLAP, the only thing there is (there is no
/// negotiation at all).
pub fn bus_widths(plugin: &Plugin) -> (u32, u32) {
    let layout = SubPluginMain::io_layout(plugin);
    (
        u32::from(layout.main_input_channels()),
        layout.outputs.first().map_or(0, |b| u32::from(b.channels)),
    )
}

pub struct RenderOutcome {
    pub audio: Audio,
    pub latency: u32,
    pub blocks: usize,
    pub host_log: Vec<String>,
    /// What the plugin sent back, gathered across every block.
    ///
    /// A backend clears the sink each call, so watching one block would only
    /// ever show the last one's worth. This is how a note the plugin says it
    /// has finished with becomes visible from outside.
    pub emitted: Vec<Event>,
}

/// Run `input` (or silence, for an instrument) through the plugin.
///
/// `events` are absolute-time; they are sliced per block here, which is also
/// what exercises `sample_offset` handling end to end.
pub fn render(
    path: &Path,
    class_cid: Option<&str>,
    input: &Audio,
    block_size: u32,
    events: &[(usize, Event)],
) -> Result<RenderOutcome, String> {
    render_with_state(path, class_cid, None, input, block_size, events)
}

/// The same, starting from a saved state blob.
///
/// The wrapper keeps everything interesting — which sub-plugin, which
/// bindings, the node graph — in its state rather than in its parameters, so
/// rendering it under a *particular* patch means restoring one first. That is
/// what makes a node graph checkable without a DAW.
pub fn render_with_state(
    path: &Path,
    class_cid: Option<&str>,
    state: Option<&[u8]>,
    input: &Audio,
    block_size: u32,
    events: &[(usize, Event)],
) -> Result<RenderOutcome, String> {
    let class = choose_class(path, class_cid)?;

    let host = Arc::new(CliHost::new());
    let mut plugin =
        Plugin::load(path, Some(&class.id), host.clone()).map_err(|e| e.to_string())?;
    if let Some(state) = state {
        // Before activate, as a DAW does: the wrapper loads its sub-plugin at
        // activate, and it has to know which one by then.
        plugin.load_state(state).map_err(|e| e.to_string())?;
    }

    let layout = SubPluginMain::io_layout(&plugin);
    // An instrument reports no input bus; feeding it one would fail bus setup.
    let input_channels = u32::from(layout.main_input_channels()).min(input.channels);
    let plugin_out = layout.outputs.first().map_or(0, |b| u32::from(b.channels));
    let output_channels = if plugin_out == 0 {
        2
    } else {
        plugin_out.min(2)
    };

    let config = AudioConfig {
        sample_rate: input.sample_rate,
        max_block_size: block_size,
        input_channels,
        output_channels,
        aux_inputs: Default::default(),
        aux_outputs: Default::default(),
        offline: true,
    };

    let mut processor = plugin.activate(config).map_err(|e| e.to_string())?;
    let latency = plugin.latency_samples();

    let mut output = Audio::silence(input.sample_rate, output_channels, input.frames);
    let mut in_scratch = vec![0.0f32; (input_channels.max(1) * block_size) as usize];
    let mut out_scratch = vec![0.0f32; (output_channels * block_size) as usize];
    let mut sink = EventSink::with_capacity(256);
    let mut emitted: Vec<Event> = Vec::new();
    let mut block_events: Vec<Event> = Vec::with_capacity(256);

    let mut context = TimeContext {
        playing: true,
        ..Default::default()
    };
    let mut position = 0usize;
    let mut blocks = 0usize;

    while position < input.frames {
        let frames = block_size.min((input.frames - position) as u32);

        for ch in 0..input_channels as usize {
            let src = input.channel(ch as u32);
            let dst = &mut in_scratch[ch * frames as usize..(ch + 1) * frames as usize];
            dst.copy_from_slice(&src[position..position + frames as usize]);
        }
        out_scratch.fill(0.0);

        // Rebase each event's timestamp onto this block.
        block_events.clear();
        for (at, event) in events {
            if *at >= position && *at < position + frames as usize {
                block_events.push(offset_event(*event, (*at - position) as u32));
            }
        }

        context.project_time_samples = position as i64;
        context.project_time_music =
            position as f64 / input.sample_rate * (context.tempo_bpm / 60.0);

        let mut buffers = AudioBuffers::new(
            &in_scratch[..(input_channels * frames) as usize],
            &mut out_scratch[..(output_channels * frames) as usize],
            input_channels,
            output_channels,
            frames,
            BufferLayout::Planar,
        );

        let status = processor.process(&mut buffers, &block_events, &context, &mut sink);
        emitted.extend_from_slice(sink.events());
        if status == ProcessStatus::Error {
            plugin.deactivate(processor);
            return Err(format!("plugin returned an error at sample {position}"));
        }

        for ch in 0..output_channels as usize {
            let start = ch * output.frames + position;
            output.samples[start..start + frames as usize]
                .copy_from_slice(&out_scratch[ch * frames as usize..(ch + 1) * frames as usize]);
        }

        position += frames as usize;
        blocks += 1;
    }

    plugin.deactivate(processor);

    Ok(RenderOutcome {
        audio: output,
        latency,
        blocks,
        host_log: host.take_log(),
        emitted,
    })
}

fn offset_event(event: Event, sample_offset: u32) -> Event {
    match event {
        Event::Note(NoteEvent::NoteOn {
            note_id,
            port,
            channel,
            key,
            velocity,
            ..
        }) => Event::Note(NoteEvent::NoteOn {
            note_id,
            port,
            channel,
            key,
            velocity,
            sample_offset,
        }),
        Event::Note(NoteEvent::NoteOff {
            note_id,
            port,
            channel,
            key,
            velocity,
            ..
        }) => Event::Note(NoteEvent::NoteOff {
            note_id,
            port,
            channel,
            key,
            velocity,
            sample_offset,
        }),
        Event::Param(plugin_host::ParamEvent::SetValue {
            id, target, value, ..
        }) => Event::Param(plugin_host::ParamEvent::SetValue {
            id,
            target,
            value,
            sample_offset,
        }),
        // Everything else is rebased through the core model's own accessor,
        // which knows every variant and does not have to be extended here.
        Event::Note(n) => Event::Note(n.at_offset(sample_offset)),
        other => other,
    }
}

/// A control change at an absolute time, as a 0..127 MIDI value.
pub fn cc(number: u8, value: u8, at: usize) -> (usize, Event) {
    (
        at,
        Event::Note(NoteEvent::Cc {
            port: 0,
            channel: 0,
            cc: number,
            value: f64::from(value) / 127.0,
            sample_offset: 0,
        }),
    )
}

/// A held note, as a pair of absolute-time events.
pub fn note(key: i16, start: usize, length: usize) -> Vec<(usize, Event)> {
    vec![
        (
            start,
            Event::Note(NoteEvent::NoteOn {
                note_id: Some(key as i32),
                port: 0,
                channel: 0,
                key,
                velocity: 0.9,
                sample_offset: 0,
            }),
        ),
        (
            start + length,
            Event::Note(NoteEvent::NoteOff {
                note_id: Some(key as i32),
                port: 0,
                channel: 0,
                key,
                velocity: 0.0,
                sample_offset: 0,
            }),
        ),
    ]
}
