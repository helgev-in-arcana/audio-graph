use serde::{Deserialize, Serialize};

/// A length in beats, written the way a DAW writes one: a whole number over a
/// power of two, optionally as a triplet.
///
/// Held as the fraction rather than as the `f64` it comes to, because the
/// fraction is what the user set. 1/16 of a beat is 0.0625, which a decimal
/// control can neither show at a sensible width nor land on by dragging, and
/// a triplet eighth is 1/3, which it cannot show at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Beats {
    pub num: u32,
    /// One of [`Beats::DENOMINATORS`]. Anything else still has a value, since
    /// a saved patch can say anything, but the editor cannot offer it.
    pub den: u32,
    /// Two thirds as long: three in the space of two.
    #[serde(default)]
    pub triplet: bool,
}

impl Beats {
    /// One beat.
    pub const ONE: Beats = Beats {
        num: 1,
        den: 1,
        triplet: false,
    };

    /// The denominators the editor offers. 64 is a 256th note, well past the
    /// shortest note value anyone writes.
    pub const DENOMINATORS: [u32; 7] = [1, 2, 4, 8, 16, 32, 64];

    /// The largest numerator the editor offers: sixteen bars of 4/4.
    pub const MAX_NUM: u32 = 64;

    pub const fn new(num: u32, den: u32) -> Beats {
        Beats {
            num,
            den,
            triplet: false,
        }
    }

    /// How many beats this is.
    pub fn value(self) -> f64 {
        let straight = f64::from(self.num) / f64::from(self.den.max(1));
        if self.triplet {
            straight * 2.0 / 3.0
        } else {
            straight
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A triplet is two thirds of the straight value, and a zero denominator
    /// read from a patch is a whole beat rather than an infinity.
    #[test]
    fn a_fraction_comes_to_the_beats_it_spells() {
        assert_eq!(Beats::new(1, 16).value(), 0.0625);
        assert_eq!(Beats::new(3, 4).value(), 0.75);
        let triplet = Beats {
            triplet: true,
            ..Beats::new(1, 2)
        };
        assert!((triplet.value() - 1.0 / 3.0).abs() < 1e-12);
        assert_eq!(Beats::new(2, 0).value(), 2.0);
    }
}
