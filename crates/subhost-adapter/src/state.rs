//! Naming a sub-plugin, and nesting its state inside the wrapper's.
//!
//! What a wrapper saves is its own business — which slots it publishes, what a
//! patch looks like. What is *not* its own business is the sub-plugin's chunk:
//! it is opaque, it has to survive a round trip unread, and the reference that
//! says which plugin to reload has to outlive that plugin going missing. Those
//! are the pieces here.

use serde::{Deserialize, Serialize};

/// Identifies which sub-plugin to reload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubPluginRef {
    /// `"vst3"` today; the field exists so a CLAP reference (M6) is additive.
    pub format: String,
    /// Authoritative identity: the VST3 class id in its platform-independent
    /// hex form, so a project moves between machines and operating systems.
    pub plugin_id: String,
    /// Where it was last found. A *hint* only — plugin folders differ between
    /// machines, so a missing path triggers a search by `plugin_id` rather than
    /// a failure.
    pub path_hint: String,
    /// For showing the user what is missing when it cannot be found.
    pub display_name: String,
}

/// One hosted sub-plugin, as saved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceState {
    /// Which plugin node this belongs to. Stored rather than implied by
    /// position, because the table is sparse: deleting a node must not
    /// renumber the ones after it.
    pub instance: usize,
    pub reference: SubPluginRef,
    /// The sub-plugin's own opaque chunk.
    ///
    /// Base64 rather than raw bytes because a wrapper's saved state is usually
    /// a text document, and a chunk that has to survive one unread cannot be
    /// carried as bytes in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

impl InstanceState {
    pub fn state_bytes(&self) -> Option<Vec<u8>> {
        base64_decode(self.state.as_deref()?)
    }
}

/// What a [`SubHost`][crate::SubHost] saves and restores.
///
/// Not itself a saved document: a wrapper owns its own file format — a version
/// number, whatever patch it holds — and this is only the part of it that this
/// crate can fill in and read back. Keeping the two apart is what lets a
/// wrapper lay its file out however it likes, and lets a project written by a
/// newer build survive a round trip through an older one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubHostState {
    pub slots: Vec<crate::slots::Slot>,
    pub instances: Vec<InstanceState>,
}

/// Minimal standard base64. Written out rather than taken as a dependency:
/// it is thirty lines, and this is the only place the project needs it.
///
/// Public because a wrapper's own state has the same problem this solves: an
/// opaque chunk, in a text document.
pub fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);

    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    fn value(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => u32::from(c - b'A'),
            b'a'..=b'z' => u32::from(c - b'a') + 26,
            b'0'..=b'9' => u32::from(c - b'0') + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }

    let bytes: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }

    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let pad = chunk.iter().filter(|&&c| c == b'=').count();
        if pad > 2 {
            return None;
        }
        let mut n = 0u32;
        for &c in chunk {
            n = n << 6 | if c == b'=' { 0 } else { value(c)? };
        }
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_handles_every_padding_case() {
        for len in 0..8 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let encoded = base64_encode(&bytes);
            assert_eq!(encoded.len() % 4, 0, "len {len} produced unpadded output");
            assert_eq!(base64_decode(&encoded), Some(bytes), "len {len}");
        }
    }

    #[test]
    fn base64_rejects_garbage_instead_of_returning_nonsense() {
        // A corrupted project should surface as "sub-plugin state unreadable",
        // not as a chunk of wrong bytes handed to a third-party plugin.
        assert_eq!(base64_decode("!!!!"), None);
        assert_eq!(base64_decode("abc"), None);
    }

    #[test]
    fn an_instance_carries_its_chunk_through_a_round_trip() {
        let state = InstanceState {
            instance: 2,
            reference: SubPluginRef {
                format: "vst3".into(),
                plugin_id: "ABCD".repeat(8),
                path_hint: "/plugins/Thing.vst3".into(),
                display_name: "Thing".into(),
            },
            state: Some(base64_encode(&[0, 1, 2, 253, 254, 255])),
        };

        let json = serde_json::to_string(&state).unwrap();
        let back: InstanceState = serde_json::from_str(&json).unwrap();

        assert_eq!(back, state);
        assert_eq!(back.state_bytes().unwrap(), vec![0, 1, 2, 253, 254, 255]);
    }
}
