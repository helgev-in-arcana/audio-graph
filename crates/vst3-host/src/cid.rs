//! Class identifiers.

use vst3::Steinberg::TUID;

/// A VST3 class ID in a platform-independent form.
///
/// `TUID`'s in-memory byte order differs between Windows and everything else,
/// so the raw bytes are not a safe thing to persist: a project saved on Windows
/// would fail to find its plugin on macOS. `Cid` stores the four 32-bit words
/// that the SDK's `INLINE_UID` macro takes, which is the form the byte-order
/// difference is defined *against*, and is therefore stable across platforms.
/// This is what §8.3 persists as the authoritative `plugin_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cid(pub [u32; 4]);

impl Cid {
    pub fn from_tuid(tuid: &TUID) -> Self {
        let b: [u8; 16] = std::array::from_fn(|i| tuid[i] as u8);

        #[cfg(target_os = "windows")]
        let words = [
            u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            u32::from_le_bytes([b[6], b[7], b[4], b[5]]),
            u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
            u32::from_be_bytes([b[12], b[13], b[14], b[15]]),
        ];

        #[cfg(not(target_os = "windows"))]
        let words = [
            u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
            u32::from_be_bytes([b[8], b[9], b[10], b[11]]),
            u32::from_be_bytes([b[12], b[13], b[14], b[15]]),
        ];

        Cid(words)
    }

    pub fn to_tuid(self) -> TUID {
        let [a, b, c, d] = self.0;
        vst3::uid(a, b, c, d)
    }

    /// 32 uppercase hex digits, the form used in `moduleinfo.json`.
    pub fn to_hex(self) -> String {
        let [a, b, c, d] = self.0;
        format!("{a:08X}{b:08X}{c:08X}{d:08X}")
    }

    pub fn from_hex(s: &str) -> Option<Cid> {
        let s = s.trim();
        if s.len() != 32 || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let mut words = [0u32; 4];
        for (i, w) in words.iter_mut().enumerate() {
            *w = u32::from_str_radix(&s[i * 8..i * 8 + 8], 16).ok()?;
        }
        Some(Cid(words))
    }
}

impl std::fmt::Display for Cid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_hex())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tuid_round_trips_through_platform_byte_order() {
        let cid = Cid([0x12345678, 0x9ABCDEF0, 0x0FEDCBA9, 0x87654321]);
        assert_eq!(Cid::from_tuid(&cid.to_tuid()), cid);
    }

    #[test]
    fn hex_round_trips() {
        let cid = Cid([0x12345678, 0x9ABCDEF0, 0x0FEDCBA9, 0x87654321]);
        assert_eq!(Cid::from_hex(&cid.to_hex()), Some(cid));
        assert_eq!(Cid::from_hex("nope"), None);
    }
}
