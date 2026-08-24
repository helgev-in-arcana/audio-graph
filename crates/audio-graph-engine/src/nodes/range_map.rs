use serde::{Deserialize, Serialize};

/// Rescale one range onto another. The 0..1 → plain-units half of §9.3 is
/// the slot table's job (`ResolvedTarget::to_plain`); this is the shaping
/// that happens before it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RangeMap {
    pub in_lo: f64,
    pub in_hi: f64,
    pub out_lo: f64,
    pub out_hi: f64,
    pub clamp: bool,
}
