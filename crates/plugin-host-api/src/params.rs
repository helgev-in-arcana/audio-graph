//! Parameter model representing raw plain values with explicit ranges.

/// Format-crossing stable parameter identity.
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
        /// The host may apply non-destructive modulation.
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
    /// Hierarchical path (slash-separated module/unit path).
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
/// Parameter reads are batched to avoid chatty inter-thread or inter-process communication.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    /// Non-destructive modulation is supported natively.
    pub modulation: bool,
    /// Modulation can be addressed per voice.
    pub poly_modulation: bool,
    pub note_expression: bool,
    /// Parameters may appear/disappear at runtime.
    pub dynamic_params: bool,
}

/// Voice polyphony information reported by an instrument plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VoiceInfo {
    /// Voices the plugin will actually use with its current patch.
    pub count: u32,
    /// The most it could ever use. Never smaller than `count`.
    pub capacity: u32,
    /// Whether two notes with the same key and channel may overlap.
    pub overlapping_notes: bool,
}

/// Minimal stand-in for the `bitflags` crate without external dependencies.
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
    /// Indicates whether this bus is auxiliary (e.g., a sidechain or secondary output) rather than the main audio bus.
    pub is_aux: bool,
}

/// Total audio and event I/O layout reported by a plugin.
///
/// Contains all input and output bus descriptions as well as MIDI/event capabilities.
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
