//! Parameter model — plain values with an explicit range.
//!
//! Normalising to 0..1 in the core would bake VST3's poverty in: CLAP's stepped
//! and enum semantics do not survive the round trip. Backends normalise on the
//! way out instead.

/// Format-crossing stable parameter identity.
///
/// VST3 `ParamID` and CLAP `clap_id` are both 32-bit, so one opaque newtype
/// covers both without either format leaking into the core vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ParamId(pub u32);

bitflags_lite! {
    /// Capability/behaviour flags for a single parameter.
    pub struct ParamFlags: u32 {
        /// Value moves in discrete steps (integers between `min` and `max`).
        const STEPPED     = 1 << 0;
        /// Wraps around at the range ends (e.g. a phase control).
        const PERIODIC    = 1 << 1;
        /// Exists but should not be shown in a generic UI.
        const HIDDEN      = 1 << 2;
        /// Reported by the plugin, never written by the host.
        const READONLY    = 1 << 3;
        /// This is the plugin's bypass parameter.
        const BYPASS      = 1 << 4;
        /// The host may automate it.
        const AUTOMATABLE = 1 << 5;
        /// The host may apply non-destructive modulation (CLAP `PARAM_MOD`).
        const MODULATABLE = 1 << 6;
        /// Modulation may differ per voice.
        const POLY_MODULATABLE = 1 << 7;
    }
}

/// Everything the engine needs to know about one sub-plugin parameter.
#[derive(Debug, Clone, PartialEq)]
pub struct ParamInfo {
    pub id: ParamId,
    pub name: String,
    /// Hierarchical path: CLAP's `module`, VST3's unit chain. `/`-separated.
    pub module: String,
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub flags: ParamFlags,
}

impl ParamInfo {
    /// Map a plain value into 0..1. Used by backends that speak normalised.
    pub fn normalize(&self, plain: f64) -> f64 {
        let span = self.max - self.min;
        if span == 0.0 {
            0.0
        } else {
            ((plain - self.min) / span).clamp(0.0, 1.0)
        }
    }

    /// Inverse of [`ParamInfo::normalize`].
    pub fn denormalize(&self, normalized: f64) -> f64 {
        self.min + normalized.clamp(0.0, 1.0) * (self.max - self.min)
    }

    pub fn clamp(&self, plain: f64) -> f64 {
        plain.clamp(self.min.min(self.max), self.max.max(self.min))
    }
}

/// One entry of a batched parameter read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamValue {
    pub id: ParamId,
    pub plain: f64,
}

/// Snapshot containing parameter values for an entire plugin instance.
///
/// The API deliberately has no `get_param(id)`. Reads are batched so the
/// boundary cannot be made chatty, which is what keeps an out-of-process
/// backend viable.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParamSnapshot {
    pub values: Vec<ParamValue>,
}

impl ParamSnapshot {
    pub fn get(&self, id: ParamId) -> Option<f64> {
        self.values.iter().find(|v| v.id == id).map(|v| v.plain)
    }
}

/// Capabilities reported by the loaded sub-plugin instance.
///
/// The engine queries this *ahead of time* — per-voice sources are greyed out
/// when `poly_modulation` is false, because that is a format limitation and not
/// a missing feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    /// Non-destructive modulation is native (CLAP `PARAM_MOD`).
    pub modulation: bool,
    /// Modulation can be addressed per voice.
    pub poly_modulation: bool,
    pub note_expression: bool,
    /// Parameters may appear/disappear at runtime.
    pub dynamic_params: bool,
}

/// How many voices an instrument has, when it will say.
///
/// CLAP's `voice-info` is the only place this comes from; VST3 has no
/// equivalent, so a VST3 sub-plugin reports `None` rather than a guess. Read
/// after loading and again whenever the plugin says it changed: a synth may
/// change it when its patch's polyphony setting moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VoiceInfo {
    /// Voices the plugin will actually use with its current patch.
    pub count: u32,
    /// The most it could ever use. Never smaller than `count`.
    pub capacity: u32,
    /// Whether two notes with the same key and channel may overlap. A host
    /// that ends notes by key alone gets this wrong for the plugins that say
    /// yes, which is why the flag exists at all.
    pub overlapping_notes: bool,
}

/// Minimal stand-in for the `bitflags` crate.
///
/// Only a handful of ops are needed and this keeps `plugin-host-api`
/// dependency-free, which matters because every other crate depends on it.
#[macro_export]
#[doc(hidden)]
macro_rules! bitflags_lite {
    (
        $(#[$meta:meta])*
        pub struct $name:ident: $ty:ty {
            $($(#[$fmeta:meta])* const $flag:ident = $value:expr;)*
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
        pub struct $name(pub $ty);

        impl $name {
            pub const NONE: $name = $name(0);
            $($(#[$fmeta])* pub const $flag: $name = $name($value);)*

            #[inline]
            pub const fn contains(self, other: $name) -> bool {
                (self.0 & other.0) == other.0
            }
            #[inline]
            pub const fn union(self, other: $name) -> $name {
                $name(self.0 | other.0)
            }
            #[inline]
            pub fn set(&mut self, other: $name, on: bool) {
                if on { self.0 |= other.0 } else { self.0 &= !other.0 }
            }
        }

        impl core::ops::BitOr for $name {
            type Output = $name;
            #[inline]
            fn bitor(self, rhs: $name) -> $name { $name(self.0 | rhs.0) }
        }
        impl core::ops::BitOrAssign for $name {
            #[inline]
            fn bitor_assign(&mut self, rhs: $name) { self.0 |= rhs.0 }
        }
    };
}
use crate::bitflags_lite;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_round_trips() {
        let p = ParamInfo {
            id: ParamId(1),
            name: "cutoff".into(),
            module: String::new(),
            min: 20.0,
            max: 20_000.0,
            default: 1000.0,
            flags: ParamFlags::AUTOMATABLE,
        };
        let n = p.normalize(1000.0);
        assert!((p.denormalize(n) - 1000.0).abs() < 1e-9);
    }

    #[test]
    fn degenerate_range_does_not_divide_by_zero() {
        let p = ParamInfo {
            id: ParamId(1),
            name: "fixed".into(),
            module: String::new(),
            min: 1.0,
            max: 1.0,
            default: 1.0,
            flags: ParamFlags::NONE,
        };
        assert_eq!(p.normalize(1.0), 0.0);
    }

    #[test]
    fn flags_compose() {
        let f = ParamFlags::STEPPED | ParamFlags::AUTOMATABLE;
        assert!(f.contains(ParamFlags::STEPPED));
        assert!(!f.contains(ParamFlags::BYPASS));
    }
}

/// Audio bus description reported by the plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusInfo {
    /// The plugin's own name for it: "Main", "Sidechain", "Key".
    pub name: String,
    pub channels: u16,
    /// True for everything the format marks auxiliary. A sidechain is aux; the
    /// bus a plugin actually processes is not.
    pub is_aux: bool,
}

/// Total audio and event I/O layout reported by a plugin.
///
/// Discovered rather than declared: what a plugin reports before negotiation is
/// a wish, and the node's sockets have to match what it will actually accept.
/// Returned in one call for the same reason as
/// [`SubPluginMain::params`] — there is no per-bus getter anywhere.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IoLayout {
    pub inputs: Vec<BusInfo>,
    pub outputs: Vec<BusInfo>,
    pub accepts_notes: bool,
    pub emits_notes: bool,
}

impl IoLayout {
    /// Channel width of the main input bus, or zero for an instrument.
    pub fn main_input_channels(&self) -> u16 {
        self.inputs.first().map_or(0, |b| b.channels)
    }

    /// The aux input buses, in order. These are the sidechain sockets.
    pub fn aux_inputs(&self) -> &[BusInfo] {
        self.inputs.get(1..).unwrap_or(&[])
    }
}
