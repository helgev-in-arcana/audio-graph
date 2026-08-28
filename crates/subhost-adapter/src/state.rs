//! State serialization types and helpers for hosted sub-plugins.
//!
//! Provides serializable structures for sub-plugin references and opaque plugin
//! state chunks (base64 encoded), along with minimal base64 utilities.

use serde::{Deserialize, Serialize};

/// Persistent reference identifying a sub-plugin to load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubPluginRef {
    /// Plugin format identifier (e.g. `"vst3"`, `"clap"`).
    pub format: String,
    /// Unique plugin identifier (e.g. VST3 class ID hex string or CLAP plugin ID).
    pub plugin_id: String,
    /// Filesystem path hint where the plugin was previously located.
    pub path_hint: String,
    /// Human-readable display name of the plugin.
    pub display_name: String,
}

/// Serialized state for a single sub-plugin instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceState {
    /// Instance index within the sub-host.
    pub instance: usize,
    pub reference: SubPluginRef,
    /// Base64-encoded opaque state blob saved from the sub-plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

impl InstanceState {
    pub fn state_bytes(&self) -> Option<Vec<u8>> {
        base64_decode(self.state.as_deref()?)
    }
}

/// State container for all slots and sub-plugin instances in a [`SubHost`][crate::SubHost].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SubHostState {
    pub slots: Vec<crate::slots::Slot>,
    pub instances: Vec<InstanceState>,
}

/// Encodes raw bytes into standard base64 string format.
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

/// Decodes a base64-encoded string into raw bytes, returning `None` if the input is invalid.
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
        // Invalid base64 strings should be rejected cleanly.
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
