use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KeyTrigger {
    Key(u8),
    Velocity { key: u8, min: u8, max: u8 },
}

impl From<u8> for KeyTrigger {
    fn from(key: u8) -> Self {
        Self::Key(key)
    }
}

impl KeyTrigger {
    pub fn key(self) -> u8 {
        match self {
            Self::Key(key) | Self::Velocity { key, .. } => key,
        }
    }

    pub fn valid(self) -> bool {
        self.key() < 128
            && match self {
                Self::Key(_) => true,
                Self::Velocity { min, max, .. } => min > 0 && min <= max && max <= 127,
            }
    }

    pub fn matches(self, key: i16, velocity: f64) -> bool {
        if !self.valid() || i16::from(self.key()) != key {
            return false;
        }
        match self {
            Self::Key(_) => true,
            Self::Velocity { min, max, .. } => {
                if !velocity.is_finite() {
                    return false;
                }
                let value = (velocity.clamp(0.0, 1.0) * 127.0).round().max(1.0) as u8;
                (min..=max).contains(&value)
            }
        }
    }

    pub fn overlaps(self, other: Self) -> bool {
        let range = |key| match key {
            Self::Key(_) => (1, 127),
            Self::Velocity { min, max, .. } => (min, max),
        };
        let (a, b) = range(self);
        let (c, d) = range(other);
        self.key() == other.key() && a <= d && c <= b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_plain_keys_and_velocity_boundaries_keep_their_meaning() {
        assert_eq!(serde_json::from_str::<KeyTrigger>("24").unwrap(), 24.into());
        let key = KeyTrigger::Velocity {
            key: 24,
            min: 43,
            max: 84,
        };
        for value in 1..=127 {
            assert_eq!(
                key.matches(24, f64::from(value) / 127.0),
                (43..=84).contains(&value)
            );
        }
        assert!(!key.matches(25, 0.5));
        assert!(!key.matches(24, f64::NAN));
        assert!(
            KeyTrigger::Velocity {
                key: 24,
                min: 1,
                max: 42
            }
            .matches(24, 0.0)
        );
        assert_eq!(
            serde_json::from_str::<KeyTrigger>(&serde_json::to_string(&key).unwrap()).unwrap(),
            key
        );
    }
}
