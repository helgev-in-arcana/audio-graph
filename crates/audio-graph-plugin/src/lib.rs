//! The wrapping plugin itself: one plugin to the DAW, a host on the inside.
//!
//! Two classes are exported from this one binary (ARCHITECTURE.md §6). The
//! sub-plugin's kind is only known at runtime, but a plugin's own category is
//! static, so the wrapper has to declare both up front: an effect with a stereo
//! input, and an instrument without one. The implementation is shared; only the
//! bus layout and the descriptor differ.

mod config;
mod editor;
mod graph_ui;
mod host_context;
mod params;
mod plugin;
mod shared;
mod tick;

use std::sync::Arc;

use nice_plug::prelude::*;

pub use config::{LANES, MAX_INSTANCES, SLOT_COUNT, SUB_HOST};
pub use host_context::WrapperHostContext;
pub use params::{SlotParam, WrapperParams};
pub use plugin::{Wrapper, WrapperKind};
pub use shared::{MainState, Shared};

/// The effect form: audio in, audio out.
#[derive(Default)]
pub struct WrapperFx(Wrapper);

/// The instrument form: no audio input, notes in, audio out.
#[derive(Default)]
pub struct WrapperInstrument(Wrapper);

/// Both classes are the same plugin with a different bus layout and identity,
/// so the trait impls are generated rather than written twice.
macro_rules! wrapper_class {
    (
        $ty:ident,
        name: $name:literal,
        kind: $kind:expr,
        layouts: $layouts:expr,
        vst3_id: $vst3_id:expr,
        vst3_categories: $vst3_categories:expr,
        clap_id: $clap_id:literal,
        clap_features: $clap_features:expr,
    ) => {
        impl Plugin for $ty {
            const NAME: &'static str = $name;
            const VENDOR: &'static str = "https://github.com/helgev-in-arcana";
            const URL: &'static str = "https://github.com/helgev-in-arcana/audio-graph";
            const EMAIL: &'static str = "";
            const VERSION: &'static str = env!("CARGO_PKG_VERSION");

            const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = $layouts;

            // Note input on both forms. The effect needs it too: note
            // expression is one of the sources the node graph will read (§9.3),
            // and a plugin that declares no note input never receives them.
            const MIDI_INPUT: MidiConfig = MidiConfig::Basic;

            // The sub-plugin is where sample-accurate parameter changes land,
            // and the wrapper quantises to sub-blocks itself (§9.2). Letting
            // the host split our buffer as well would fight that.
            const SAMPLE_ACCURATE_AUTOMATION: bool = false;

            // The editor is where a sub-plugin is chosen and its parameters
            // bound to slots; without it the DAW sees 32 unlabelled slots and
            // no way to point them at anything.
            type Editor = nice_plug_egui::EguiEditor<$crate::editor::WrapperEditor>;
            type SysExMessage = ();
            // One task, and it is the periodic main-thread tick CLAP requires
            // of a host (see `tick`). It is a *foreground* task in every sense
            // that matters: it only ever runs through `execute_gui`.
            type BackgroundTask = $crate::tick::Task;

            fn task_executor(&mut self) -> TaskExecutor<Self> {
                Box::new(self.0.task_executor())
            }

            fn params(&self) -> Arc<dyn Params> {
                self.0.params()
            }

            fn editor(&mut self, executor: AsyncExecutor<Self>) -> Option<Self::Editor> {
                // Not editor business, but this is the one moment nice-plug
                // offers a way onto the main thread, and it happens at
                // instance creation rather than when a window opens.
                self.0.start_ticking(move || {
                    executor.execute_gui($crate::tick::Task::Tick);
                });
                $crate::editor::create(self.0.shared().clone())
            }

            fn activate(
                &mut self,
                audio_io_layout: &AudioIOLayout,
                buffer_config: &BufferConfig,
                context: &mut impl ActivateContext<Self>,
            ) -> bool {
                let latency = self.0.activate($kind, audio_io_layout, buffer_config);
                match latency {
                    Some(samples) => {
                        // §7.4: the DAW is told the sub-plugin's latency plus
                        // ours, or the track sits misaligned.
                        context.set_latency_samples(samples);
                        true
                    }
                    None => false,
                }
            }

            fn reset(&mut self) {
                self.0.reset();
            }

            fn process(
                &mut self,
                buffer: &mut Buffer,
                aux: &mut AuxiliaryBuffers,
                context: &mut impl ProcessContext<Self>,
            ) -> ProcessStatus {
                self.0.process(buffer, aux, context)
            }

            fn deactivate(&mut self) {
                self.0.deactivate();
            }
        }

        impl Vst3Plugin for $ty {
            const VST3_CLASS_ID: [u8; 16] = $vst3_id;
            const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = $vst3_categories;
        }

        impl ClapPlugin for $ty {
            const CLAP_ID: &'static str = $clap_id;
            const CLAP_DESCRIPTION: Option<&'static str> =
                Some("Hosts another plugin and drives its parameters");
            const CLAP_MANUAL_URL: Option<&'static str> = None;
            const CLAP_SUPPORT_URL: Option<&'static str> = None;
            const CLAP_FEATURES: &'static [ClapFeature] = $clap_features;
        }
    };
}

/// Stereo in, stereo out, plus one stereo sidechain the DAW can feed.
///
/// The aux bus is fixed at compile time because VST3 cannot add one at runtime
/// (§14.11) — the same reason the slot count is fixed (§8.1). One is enough for
/// the shape this exists for: a compressor inside the graph keyed off another
/// track. A patch that wires nothing to it costs the DAW an unused bus, which
/// every host already deals with.
const FX_LAYOUTS: &[AudioIOLayout] = &[AudioIOLayout {
    main_input_channels: NonZeroU32::new(2),
    main_output_channels: NonZeroU32::new(2),
    aux_input_ports: &[TWO],
    names: PortNames {
        aux_inputs: &["Sidechain"],
        ..PortNames::const_default()
    },
    ..AudioIOLayout::const_default()
}];

/// No audio input. An instrument that declares one is refused outright by some
/// hosts, and offered a silent buffer by others.
///
/// The sidechain bus is still offered: a graph whose instrument feeds a
/// compressor keyed off another track is a normal thing to build, and an
/// instrument with no input bus at all cannot express it.
const INSTRUMENT_LAYOUTS: &[AudioIOLayout] = &[AudioIOLayout {
    main_input_channels: None,
    main_output_channels: NonZeroU32::new(2),
    aux_input_ports: &[TWO],
    names: PortNames {
        aux_inputs: &["Sidechain"],
        ..PortNames::const_default()
    },
    ..AudioIOLayout::const_default()
}];

/// Written out because `NonZeroU32::new` is not usable in a const initialiser
/// of an array element here.
const TWO: NonZeroU32 = match NonZeroU32::new(2) {
    Some(n) => n,
    None => unreachable!(),
};

wrapper_class! {
    WrapperFx,
    name: "Audio Graph FX",
    kind: WrapperKind::Effect,
    layouts: FX_LAYOUTS,
    // 21f59683-14c0-466e-911b-cc12b7853aaa. Random, and fixed for good: a host
    // finds this plugin by its class id alone, so changing it makes every
    // saved project report the plugin as missing. It moves only alongside a
    // deliberate break of that kind.
    vst3_id: [
        0x21, 0xf5, 0x96, 0x83, 0x14, 0xc0, 0x46, 0x6e, 0x91, 0x1b, 0xcc, 0x12, 0xb7, 0x85, 0x3a,
        0xaa,
    ],
    vst3_categories: &[Vst3SubCategory::Fx, Vst3SubCategory::Tools],
    clap_id: "io.github.helgev-in-arcana.audio-graph.fx",
    clap_features: &[ClapFeature::AudioEffect, ClapFeature::Stereo, ClapFeature::Utility],
}

wrapper_class! {
    WrapperInstrument,
    name: "Audio Graph Instrument",
    kind: WrapperKind::Instrument,
    layouts: INSTRUMENT_LAYOUTS,
    // c6d15c26-fa9c-43b4-bd58-e7f061c7d794. Fixed for good, as above.
    vst3_id: [
        0xc6, 0xd1, 0x5c, 0x26, 0xfa, 0x9c, 0x43, 0xb4, 0xbd, 0x58, 0xe7, 0xf0, 0x61, 0xc7, 0xd7,
        0x94,
    ],
    vst3_categories: &[Vst3SubCategory::Instrument, Vst3SubCategory::Synth],
    clap_id: "io.github.helgev-in-arcana.audio-graph.instrument",
    clap_features: &[ClapFeature::Instrument, ClapFeature::Stereo],
}

// §12's first open question, answered: nice-plug's export macros already take
// several plugin types, so exporting both classes from one binary needs no
// changes to nice-plug.
nice_export_vst3!(WrapperFx, WrapperInstrument);
nice_export_clap!(WrapperFx, WrapperInstrument);
