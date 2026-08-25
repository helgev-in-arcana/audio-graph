//! Just enough RIFF/WAVE to get audio in and out of the offline renderer.
//!
//! Hand-rolled rather than pulled in as a dependency: the renderer needs one
//! layout (interleaved, 32-bit float or 16-bit PCM) and nothing else, and this
//! is a development harness, not a shipped codec.

use std::io::Write;
use std::path::Path;

pub struct Audio {
    pub sample_rate: f64,
    pub channels: u32,
    /// Planar: channel 0's frames, then channel 1's, matching the layout the
    /// host API uses so no transposition is needed at the boundary.
    pub samples: Vec<f32>,
    pub frames: usize,
}

impl Audio {
    pub fn silence(sample_rate: f64, channels: u32, frames: usize) -> Audio {
        Audio {
            sample_rate,
            channels,
            samples: vec![0.0; channels as usize * frames],
            frames,
        }
    }

    pub fn channel(&self, index: u32) -> &[f32] {
        let start = index as usize * self.frames;
        &self.samples[start..start + self.frames]
    }

    /// Peak absolute sample across all channels — the cheapest "did anything
    /// happen" check, which is what the milestone DoDs actually ask for.
    pub fn peak(&self) -> f32 {
        self.samples.iter().fold(0.0f32, |a, s| a.max(s.abs()))
    }

    pub fn rms(&self) -> f32 {
        if self.samples.is_empty() {
            return 0.0;
        }
        let sum: f64 = self.samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
        (sum / self.samples.len() as f64).sqrt() as f32
    }
}

pub fn read(path: &Path) -> Result<Audio, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(format!("{}: not a RIFF/WAVE file", path.display()));
    }

    let mut pos = 12;
    let mut channels = 0u32;
    let mut sample_rate = 0f64;
    let mut bits = 0u16;
    let mut format = 0u16;
    let mut data: Option<&[u8]> = None;

    // Chunk walk rather than assuming fmt-then-data: real files interleave
    // LIST/fact chunks and a fixed-offset reader trips over them.
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body_start = pos + 8;
        let body_end = (body_start + size).min(bytes.len());

        match id {
            b"fmt " if size >= 16 => {
                let b = &bytes[body_start..body_end];
                format = u16::from_le_bytes(b[0..2].try_into().unwrap());
                channels = u16::from_le_bytes(b[2..4].try_into().unwrap()) as u32;
                sample_rate = u32::from_le_bytes(b[4..8].try_into().unwrap()) as f64;
                bits = u16::from_le_bytes(b[14..16].try_into().unwrap());
            }
            b"data" => data = Some(&bytes[body_start..body_end]),
            _ => {}
        }
        // Chunks are word-aligned; odd sizes carry a pad byte.
        pos = body_start + size + (size & 1);
    }

    let data = data.ok_or_else(|| format!("{}: no data chunk", path.display()))?;
    if channels == 0 {
        return Err(format!("{}: no fmt chunk", path.display()));
    }

    let interleaved: Vec<f32> = match (format, bits) {
        (1, 16) => data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| i16::from_le_bytes(c) as f32 / 32768.0)
            .collect(),
        (1, 24) => data
            .as_chunks::<3>()
            .0
            .iter()
            .map(|&[a, b, c]| {
                let v = i32::from_le_bytes([0, a, b, c]) >> 8;
                v as f32 / 8_388_608.0
            })
            .collect(),
        (3, 32) => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&c| f32::from_le_bytes(c))
            .collect(),
        // 0xFFFE is WAVE_FORMAT_EXTENSIBLE; the sub-format lives in the fmt
        // extension, but bit depth alone disambiguates the cases we accept.
        (0xFFFE, 16) => data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| i16::from_le_bytes(c) as f32 / 32768.0)
            .collect(),
        (0xFFFE, 32) => data
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&c| f32::from_le_bytes(c))
            .collect(),
        _ => {
            return Err(format!(
                "{}: unsupported format {format} / {bits}-bit",
                path.display()
            ));
        }
    };

    let frames = interleaved.len() / channels as usize;
    let mut samples = vec![0.0f32; frames * channels as usize];
    for frame in 0..frames {
        for ch in 0..channels as usize {
            samples[ch * frames + frame] = interleaved[frame * channels as usize + ch];
        }
    }

    Ok(Audio {
        sample_rate,
        channels,
        samples,
        frames,
    })
}

/// Write 32-bit float WAVE, so a render round-trip loses nothing.
pub fn write(path: &Path, audio: &Audio) -> Result<(), String> {
    let channels = audio.channels as usize;
    let data_bytes = audio.frames * channels * 4;
    let mut out = Vec::with_capacity(44 + data_bytes);

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data_bytes) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");

    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    out.extend_from_slice(&(channels as u16).to_le_bytes());
    out.extend_from_slice(&(audio.sample_rate as u32).to_le_bytes());
    out.extend_from_slice(&((audio.sample_rate as u32) * channels as u32 * 4).to_le_bytes());
    out.extend_from_slice(&((channels * 4) as u16).to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());

    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    for frame in 0..audio.frames {
        for ch in 0..channels {
            out.extend_from_slice(&audio.samples[ch * audio.frames + frame].to_le_bytes());
        }
    }

    let mut file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(&out)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}
