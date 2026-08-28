//! Plain ↔ normalized conversion for the audio thread.
//!
//! Host audio engines typically use plain (unnormalized) values with domain ranges,
//! whereas VST3 processing operates on normalized 0.0..1.0 values. The authoritative
//! converter is `IEditController`, which is a main-thread interface that cannot be
//! called safely from the real-time audio thread.
//!
//! To support real-time conversion without locks or allocation, the parameter mapping
//! is captured during activation on the main thread into a lookup structure. Linear
//! and stepped parameters use closed-form conversions, while non-linear curves
//! use a sampled lookup table.

use plugin_host_api::{ParamId, ParamInfo};

/// How many samples define a non-linear curve. 257 points gives a worst-case
/// interpolation error well below what a 16-bit control surface can express,
/// while staying small enough to hold for hundreds of parameters.
const TABLE_SIZE: usize = 257;

/// Points used to decide whether the closed form is good enough.
const PROBE_COUNT: usize = 33;

/// Relative tolerance for calling a mapping linear.
const LINEARITY_TOLERANCE: f64 = 1e-9;

enum Curve {
    /// `normalized = (plain - min) / (max - min)`.
    Linear { min: f64, span: f64 },
    /// Plain value at each of `TABLE_SIZE` evenly spaced normalised positions,
    /// monotonically ordered as VST3 requires.
    Sampled(Box<[f64; TABLE_SIZE]>),
    /// Range is degenerate; every plain value maps to 0.
    Constant,
}

struct Entry {
    id: ParamId,
    curve: Curve,
}

/// Plain→normalised conversion for one plugin's whole parameter list.
pub struct ParamMap {
    entries: Vec<Entry>,
}

impl ParamMap {
    /// Build the table. `sample` must return the plain value corresponding to
    /// a normalised one — i.e. `IEditController::normalizedParamToPlain`.
    ///
    /// Called on the main thread during activate.
    pub fn build(params: &[ParamInfo], mut sample: impl FnMut(ParamId, f64) -> f64) -> ParamMap {
        let entries = params
            .iter()
            .map(|p| Entry {
                id: p.id,
                curve: build_curve(p, &mut sample),
            })
            .collect();
        ParamMap { entries }
    }

    /// Convert a plain value to normalised. Audio-thread safe.
    ///
    /// Unknown ids return `None` rather than guessing: sending a made-up
    /// normalised value to a parameter that is not there would be a silent
    /// wrong answer.
    pub fn normalize(&self, id: ParamId, plain: f64) -> Option<f64> {
        let entry = self.entries.iter().find(|e| e.id == id)?;
        Some(match &entry.curve {
            Curve::Constant => 0.0,
            Curve::Linear { min, span } => ((plain - min) / span).clamp(0.0, 1.0),
            Curve::Sampled(table) => invert_table(table, plain),
        })
    }
}

fn build_curve(param: &ParamInfo, sample: &mut impl FnMut(ParamId, f64) -> f64) -> Curve {
    let min = sample(param.id, 0.0);
    let max = sample(param.id, 1.0);
    let span = max - min;
    if span == 0.0 || !span.is_finite() {
        return Curve::Constant;
    }

    // Probe the curve before committing to a table. Linear is overwhelmingly
    // the common case and costs nothing to detect.
    let scale = span.abs();
    let mut linear = true;
    for i in 1..PROBE_COUNT - 1 {
        let n = i as f64 / (PROBE_COUNT - 1) as f64;
        let actual = sample(param.id, n);
        let expected = min + n * span;
        if (actual - expected).abs() > LINEARITY_TOLERANCE * scale {
            linear = false;
            break;
        }
    }

    if linear {
        return Curve::Linear { min, span };
    }

    let mut table = Box::new([0.0; TABLE_SIZE]);
    for (i, slot) in table.iter_mut().enumerate() {
        *slot = sample(param.id, i as f64 / (TABLE_SIZE - 1) as f64);
    }
    Curve::Sampled(table)
}

/// Find the normalised position whose plain value is `plain`.
///
/// The table is monotonic (VST3 guarantees the mapping is), but may ascend or
/// descend, so both directions are handled.
fn invert_table(table: &[f64; TABLE_SIZE], plain: f64) -> f64 {
    let ascending = table[TABLE_SIZE - 1] >= table[0];
    let first = table[0];
    let last = table[TABLE_SIZE - 1];

    let (lo_bound, hi_bound) = if ascending {
        (first, last)
    } else {
        (last, first)
    };
    if plain <= lo_bound {
        return if ascending { 0.0 } else { 1.0 };
    }
    if plain >= hi_bound {
        return if ascending { 1.0 } else { 0.0 };
    }

    // Binary search for the bracketing pair, then interpolate within it.
    let mut lo = 0usize;
    let mut hi = TABLE_SIZE - 1;
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        let cmp = if ascending {
            table[mid] <= plain
        } else {
            table[mid] >= plain
        };
        if cmp { lo = mid } else { hi = mid }
    }

    let (a, b) = (table[lo], table[hi]);
    let t = if a == b { 0.0 } else { (plain - a) / (b - a) };
    (lo as f64 + t) / (TABLE_SIZE - 1) as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use plugin_host_api::ParamFlags;

    fn info(id: u32, min: f64, max: f64) -> ParamInfo {
        ParamInfo {
            id: ParamId(id),
            name: String::new(),
            module: String::new(),
            min,
            max,
            default: min,
            flags: ParamFlags::NONE,
        }
    }

    #[test]
    fn linear_parameters_round_trip_exactly() {
        let params = [info(1, -60.0, 12.0)];
        let map = ParamMap::build(&params, |_, n| -60.0 + n * 72.0);
        assert_eq!(map.normalize(ParamId(1), -60.0), Some(0.0));
        assert_eq!(map.normalize(ParamId(1), 12.0), Some(1.0));
        let mid = map.normalize(ParamId(1), -24.0).unwrap();
        assert!((mid - 0.5).abs() < 1e-12);
    }

    #[test]
    fn log_scaled_parameters_invert_within_table_resolution() {
        // A cutoff sweep: the classic case where treating the mapping as
        // linear puts a "1 kHz" request at roughly 5 kHz.
        let params = [info(2, 20.0, 20_000.0)];
        let curve = |n: f64| 20.0 * (20_000.0f64 / 20.0).powf(n);
        let map = ParamMap::build(&params, |_, n| curve(n));

        for target in [20.0, 100.0, 440.0, 1000.0, 8000.0, 20_000.0] {
            let n = map.normalize(ParamId(2), target).unwrap();
            let back = curve(n);
            assert!(
                (back - target).abs() / target < 1e-3,
                "{target} Hz round-tripped to {back} Hz"
            );
        }
    }

    #[test]
    fn descending_curves_invert_too() {
        let params = [info(3, 1.0, 0.0)];
        let map = ParamMap::build(&params, |_, n| (1.0 - n).powi(2));
        let n = map.normalize(ParamId(3), 0.25).unwrap();
        assert!((n - 0.5).abs() < 1e-3, "got {n}");
    }

    #[test]
    fn out_of_range_values_clamp() {
        let params = [info(4, 0.0, 1.0)];
        let map = ParamMap::build(&params, |_, n| n);
        assert_eq!(map.normalize(ParamId(4), -5.0), Some(0.0));
        assert_eq!(map.normalize(ParamId(4), 5.0), Some(1.0));
    }

    #[test]
    fn degenerate_range_does_not_produce_nan() {
        let params = [info(5, 1.0, 1.0)];
        let map = ParamMap::build(&params, |_, _| 1.0);
        assert_eq!(map.normalize(ParamId(5), 1.0), Some(0.0));
    }

    #[test]
    fn unknown_ids_are_reported_not_guessed() {
        let map = ParamMap::build(&[info(1, 0.0, 1.0)], |_, n| n);
        assert_eq!(map.normalize(ParamId(99), 0.5), None);
    }
}
