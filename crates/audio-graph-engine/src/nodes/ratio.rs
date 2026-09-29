use serde::{Deserialize, Serialize};

/// How a length is shared between two parts, as two whole numbers: 1:1 is
/// even, 3:1 is three quarters to the first part.
///
/// Whole numbers rather than a percentage for the reason [`Beats`] is a
/// fraction: the shares that sound right against a beat are the small ones,
/// 1:1, 2:1, 3:1, and a percentage spells 2:1 as 66.667.
///
/// [`Beats`]: crate::nodes::Beats
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ratio {
    pub first: u32,
    pub second: u32,
}

impl Ratio {
    /// Even shares.
    pub const EVEN: Ratio = Ratio::new(1, 1);

    /// The largest share the editor offers. Past 16:1 the smaller part is
    /// too short to hear as a part.
    pub const MAX: u32 = 16;

    pub const fn new(first: u32, second: u32) -> Ratio {
        Ratio { first, second }
    }

    /// The first part's share of the whole, 0..=1. Two zero shares — which
    /// only a hand-edited patch can hold — split evenly rather than dividing
    /// by zero.
    pub fn share(self) -> f64 {
        let whole = self.first + self.second;
        if whole == 0 {
            return 0.5;
        }
        f64::from(self.first) / f64::from(whole)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first part gets its share of the whole, and a ratio of nothing to
    /// nothing is even rather than a NaN.
    #[test]
    fn a_ratio_shares_the_whole_between_its_parts() {
        assert_eq!(Ratio::EVEN.share(), 0.5);
        assert_eq!(Ratio::new(3, 1).share(), 0.75);
        assert_eq!(Ratio::new(1, 0).share(), 1.0);
        assert_eq!(Ratio::new(0, 0).share(), 0.5);
    }
}
