//! The wrapping plugin itself: one plugin to the DAW, a host on the inside.
//!
//! Two classes are exported from this one binary (ARCHITECTURE.md §6). The
//! sub-plugin's kind is only known at runtime, but a plugin's own category is
//! static, so the wrapper has to declare both up front: an effect with a stereo
//! input, and an instrument without one. The implementation is shared; only the
//! bus layout and the descriptor differ.

mod editor;
mod graph_ui;
mod host_context;
mod params;
mod plugin;
mod shared;

use std::sync::Arc;

use nice_plug::prelude::*;

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
            const VENDOR: &'static str = "audio-graph";
            const URL: &'static str = "https://example.invalid/audio-graph";
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
            type BackgroundTask = ();

            fn params(&self) -> Arc<dyn Params> {
                self.0.params()
            }

            fn editor(&mut self, _executor: AsyncExecutor<Self>) -> Option<Self::Editor> {
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
                _aux: &mut AuxiliaryBuffers,
                context: &mut impl ProcessContext<Self>,
            ) -> ProcessStatus {
                self.0.process(buffer, context)
            }

            fn deactivate(&mut self) {
                self.0.deactivate();
            }
        }

        impl Vst3Plugin for $ty {
            const VST3_CLASS_ID: [u8; 16] = *$vst3_id;
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

/// Stereo in, stereo out.
const FX_LAYOUTS: &[AudioIOLayout] = &[AudioIOLayout {
    main_input_channels: NonZeroU32::new(2),
    main_output_channels: NonZeroU32::new(2),
    ..AudioIOLayout::const_default()
}];

/// No audio input. An instrument that declares one is refused outright by some
/// hosts, and offered a silent buffer by others.
const INSTRUMENT_LAYOUTS: &[AudioIOLayout] = &[AudioIOLayout {
    main_input_channels: None,
    main_output_channels: NonZeroU32::new(2),
    ..AudioIOLayout::const_default()
}];

wrapper_class! {
    WrapperFx,
    name: "Audio Graph FX",
    kind: WrapperKind::Effect,
    layouts: FX_LAYOUTS,
    vst3_id: b"AudioGraphFx0001",
    vst3_categories: &[Vst3SubCategory::Fx, Vst3SubCategory::Tools],
    clap_id: "audio-graph.wrapper.fx",
    clap_features: &[ClapFeature::AudioEffect, ClapFeature::Stereo, ClapFeature::Utility],
}

wrapper_class! {
    WrapperInstrument,
    name: "Audio Graph Instrument",
    kind: WrapperKind::Instrument,
    layouts: INSTRUMENT_LAYOUTS,
    vst3_id: b"AudioGraphInst01",
    vst3_categories: &[Vst3SubCategory::Instrument, Vst3SubCategory::Synth],
    clap_id: "audio-graph.wrapper.instrument",
    clap_features: &[ClapFeature::Instrument, ClapFeature::Stereo],
}

// §12's first open question, answered: nice-plug's export macros already take
// several plugin types, so exporting both classes from one binary needs no
// changes to nice-plug.
nice_export_vst3!(WrapperFx, WrapperInstrument);
nice_export_clap!(WrapperFx, WrapperInstrument);
