use plugin_host::Event;

/// The two layouts keep an in-place buffer exclusively borrowed rather than
/// manufacturing overlapping shared and mutable slices.
pub(crate) enum AudioIo<'a> {
    InPlace(InPlaceAudio<'a>),
    Separate(SeparateAudio<'a>),
}

pub(crate) struct InPlaceAudio<'a> {
    output: &'a mut [f32],
    channels: usize,
    stride: usize,
    frames: usize,
}

pub(crate) struct SeparateAudio<'a> {
    input: &'a [f32],
    output: InPlaceAudio<'a>,
}

/// Implementations are statically selected once per chunk, so a sample loop
/// does not dispatch dynamically or test whether its input aliases its output.
pub(crate) trait AudioSamples {
    fn channels(&self) -> usize;
    fn frames(&self) -> usize;
    fn read(&self, channel: usize, frame: usize) -> f32;
    fn write(&mut self, channel: usize, frame: usize, value: f32);
    fn passthrough(&mut self);
}

impl AudioSamples for InPlaceAudio<'_> {
    fn passthrough(&mut self) {}
    fn channels(&self) -> usize {
        self.channels
    }
    fn frames(&self) -> usize {
        self.frames
    }
    fn read(&self, channel: usize, frame: usize) -> f32 {
        self.output[channel * self.stride + frame]
    }
    fn write(&mut self, channel: usize, frame: usize, value: f32) {
        self.output[channel * self.stride + frame] = value;
    }
}

impl AudioSamples for SeparateAudio<'_> {
    fn passthrough(&mut self) {
        for ch in 0..self.output.channels {
            let start = ch * self.output.stride;
            let end = start + self.output.frames;
            self.output.output[start..end].copy_from_slice(&self.input[start..end]);
        }
    }
    fn channels(&self) -> usize {
        self.output.channels
    }
    fn frames(&self) -> usize {
        self.output.frames
    }
    fn read(&self, channel: usize, frame: usize) -> f32 {
        self.input[channel * self.output.stride + frame]
    }
    fn write(&mut self, channel: usize, frame: usize, value: f32) {
        self.output.write(channel, frame, value);
    }
}

impl<'a> AudioIo<'a> {
    pub(super) fn new(
        pool: &'a mut [f32],
        input: usize,
        output: usize,
        channels: usize,
        stride: usize,
        frames: usize,
    ) -> Self {
        let len = if channels == 0 {
            0
        } else {
            (channels - 1) * stride + frames
        };
        let view = |output| InPlaceAudio {
            output,
            channels,
            stride,
            frames,
        };
        if input == output {
            Self::InPlace(view(&mut pool[output..output + len]))
        } else if input < output {
            let (before, after) = pool.split_at_mut(output);
            Self::Separate(SeparateAudio {
                input: &before[input..input + len],
                output: view(&mut after[..len]),
            })
        } else {
            let (before, after) = pool.split_at_mut(input);
            Self::Separate(SeparateAudio {
                input: &after[..len],
                output: view(&mut before[output..output + len]),
            })
        }
    }
}

pub(crate) struct AudioEvalContext<'a> {
    pub audio: AudioIo<'a>,
    pub notes: &'a [Event],
    pub start: usize,
    pub sample_rate: f64,
    pub tempo: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every buffer ordering confines copies and writes to the selected channel windows.
    #[test]
    fn audio_windows_preserve_stride_gaps_and_neighboring_buffers() {
        const CHANNELS: usize = 3;
        const STRIDE: usize = 5;
        const FRAMES: usize = 2;
        for (input, output) in [(2, 2), (2, 20), (20, 2)] {
            let original: Vec<f32> = (0..40).map(|i| i as f32 + 0.25).collect();
            let mut pool = original.clone();
            let mut expected = original.clone();
            {
                let mut io = AudioIo::new(&mut pool, input, output, CHANNELS, STRIDE, FRAMES);
                let samples: &mut dyn AudioSamples = match &mut io {
                    AudioIo::InPlace(samples) => samples,
                    AudioIo::Separate(samples) => samples,
                };
                assert_eq!(samples.channels(), CHANNELS);
                assert_eq!(samples.frames(), FRAMES);
                samples.passthrough();
                for channel in 0..CHANNELS {
                    for frame in 0..FRAMES {
                        let at = channel * STRIDE + frame;
                        assert_eq!(samples.read(channel, frame), original[input + at]);
                        expected[output + at] = original[input + at];
                    }
                }
            }
            assert_eq!(pool, expected, "passthrough: {input} -> {output}");
            {
                let mut io = AudioIo::new(&mut pool, input, output, CHANNELS, STRIDE, FRAMES);
                let samples: &mut dyn AudioSamples = match &mut io {
                    AudioIo::InPlace(samples) => samples,
                    AudioIo::Separate(samples) => samples,
                };
                for channel in 0..CHANNELS {
                    for frame in 0..FRAMES {
                        let at = channel * STRIDE + frame;
                        let value = -samples.read(channel, frame);
                        samples.write(channel, frame, value);
                        expected[output + at] = value;
                        assert_eq!(
                            samples.read(channel, frame),
                            if input == output { value } else { -value }
                        );
                    }
                }
            }
            assert_eq!(pool, expected, "read/write: {input} -> {output}");
        }
    }

    /// Zero-channel views borrow empty slices and leave the pool untouched in every ordering.
    #[test]
    fn zero_channel_audio_is_empty() {
        for (input, output) in [(0, 0), (0, 4), (4, 0)] {
            let mut pool = [1.0, 2.0, 3.0, 4.0];
            match AudioIo::new(&mut pool, input, output, 0, 8, 3) {
                AudioIo::InPlace(mut samples) => {
                    assert_eq!(samples.channels(), 0);
                    assert!(samples.output.is_empty());
                    samples.passthrough();
                }
                AudioIo::Separate(mut samples) => {
                    assert_eq!(samples.channels(), 0);
                    assert!(samples.input.is_empty());
                    assert!(samples.output.output.is_empty());
                    samples.passthrough();
                }
            }
            assert_eq!(pool, [1.0, 2.0, 3.0, 4.0]);
        }
    }
}
