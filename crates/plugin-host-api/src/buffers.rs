//! Audio buffer representation.
//!
//! Audio channels are stored in a contiguous, flat buffer rather than nested slices
//! to ensure efficient memory layout and compatibility with shared memory buffers.

/// How channels are arranged inside the flat backing store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BufferLayout {
    /// `c0f0 c1f0 c0f1 c1f1 ...`
    Interleaved,
    /// `c0f0 c0f1 ... c1f0 c1f1 ...`
    #[default]
    Planar,
}

/// Maximum number of auxiliary buses a plugin may be configured with in one direction.
///
/// Auxiliary buses follow the main bus (e.g. sidechain inputs or secondary outputs).
/// Using a fixed-size array keeps [`AudioConfig`] `Copy` without heap allocations.
pub const MAX_AUX_BUSES: usize = 3;

/// The auxiliary buses of one plugin in one direction, represented by their channel widths.
///
/// Defaults to empty, indicating no auxiliary buses are active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AuxBuses {
    widths: [u16; MAX_AUX_BUSES],
    count: u8,
}

impl AuxBuses {
    /// Creates an [`AuxBuses`] instance from the provided channel widths, truncated to [`MAX_AUX_BUSES`].
    pub fn new(widths: &[u16]) -> AuxBuses {
        let mut out = AuxBuses::default();
        for &width in widths.iter().take(MAX_AUX_BUSES) {
            out.widths[out.count as usize] = width;
            out.count += 1;
        }
        out
    }

    pub fn len(&self) -> usize {
        self.count as usize
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn get(&self, index: usize) -> Option<u16> {
        (index < self.len()).then(|| self.widths[index])
    }

    pub fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        self.widths[..self.len()].iter().copied()
    }

    /// Channels across every aux bus.
    pub fn total_channels(&self) -> u32 {
        self.iter().map(u32::from).sum()
    }
}

/// Fixed audio configuration provided when activating a plugin.
///
/// Changing these settings requires deactivating and reactivating the plugin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioConfig {
    pub sample_rate: f64,
    /// Largest block `process` will ever be called with.
    pub max_block_size: u32,
    /// Channels on the *main* input bus. Zero for an instrument.
    pub input_channels: u32,
    pub output_channels: u32,
    /// Extra input buses beyond the main one (e.g., sidechain inputs).
    ///
    /// Stored separately from `input_channels` because bus 0 is the primary input
    /// processed by the plugin, while auxiliary buses serve as secondary inputs.
    pub aux_inputs: AuxBuses,
    /// Extra output buses beyond the main one (e.g., auxiliary or multi-out pairs).
    ///
    /// Stored separately from `output_channels`. Only requested auxiliary buses
    /// are active to avoid unnecessary computation.
    pub aux_outputs: AuxBuses,
    /// True when the host is rendering faster than real time.
    pub offline: bool,
}

impl AudioConfig {
    /// Every input channel the plugin will be handed, main bus and aux buses
    /// together. This is the width of [`AudioBuffers`]'s input region.
    pub fn total_input_channels(&self) -> u32 {
        self.input_channels + self.aux_inputs.total_channels()
    }

    /// The same on the way out, and the width of the output region.
    pub fn total_output_channels(&self) -> u32 {
        self.output_channels + self.aux_outputs.total_channels()
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000.0,
            max_block_size: 512,
            input_channels: 2,
            output_channels: 2,
            aux_inputs: AuxBuses::default(),
            aux_outputs: AuxBuses::default(),
            offline: false,
        }
    }
}

/// Borrowed view over flat audio buffer memory for a single processing block.
///
/// The input region contains the main bus followed contiguously by each auxiliary bus,
/// where `input_channels` is the total channel count and `aux_inputs` defines the bus widths.
/// The output region follows the same layout.
pub struct AudioBuffers<'a> {
    input: &'a [f32],
    output: &'a mut [f32],
    input_channels: u32,
    output_channels: u32,
    aux_inputs: AuxBuses,
    aux_outputs: AuxBuses,
    frame_count: u32,
    layout: BufferLayout,
}

impl<'a> AudioBuffers<'a> {
    /// # Panics
    /// If either slice is shorter than `channels * frame_count`.
    pub fn new(
        input: &'a [f32],
        output: &'a mut [f32],
        input_channels: u32,
        output_channels: u32,
        frame_count: u32,
        layout: BufferLayout,
    ) -> Self {
        assert!(
            input.len() >= (input_channels * frame_count) as usize,
            "input buffer too small"
        );
        assert!(
            output.len() >= (output_channels * frame_count) as usize,
            "output buffer too small"
        );
        Self {
            input,
            output,
            input_channels,
            output_channels,
            aux_inputs: AuxBuses::default(),
            aux_outputs: AuxBuses::default(),
            frame_count,
            layout,
        }
    }

    /// Declare that the input region carries aux buses after the main one.
    ///
    /// `input_channels` must already count them; this only says where the
    /// joins are. Builder-style because the great majority of calls have no
    /// aux buses and should not have to say so.
    ///
    /// # Panics
    /// If the buses do not add up to `input_channels`.
    pub fn with_aux_inputs(mut self, aux: AuxBuses) -> Self {
        assert!(
            aux.total_channels() <= self.input_channels,
            "aux buses claim more channels than the input region has"
        );
        self.aux_inputs = aux;
        self
    }

    pub fn aux_inputs(&self) -> AuxBuses {
        self.aux_inputs
    }

    /// Declare that the output region carries aux buses after the main one.
    /// The mirror of [`AudioBuffers::with_aux_inputs`], and the same rule:
    /// `output_channels` already counts them.
    ///
    /// # Panics
    /// If the buses do not add up to `output_channels`.
    pub fn with_aux_outputs(mut self, aux: AuxBuses) -> Self {
        assert!(
            aux.total_channels() <= self.output_channels,
            "aux buses claim more channels than the output region has"
        );
        self.aux_outputs = aux;
        self
    }

    pub fn aux_outputs(&self) -> AuxBuses {
        self.aux_outputs
    }

    /// Channels on the main output bus: everything the aux buses do not claim.
    pub fn main_output_channels(&self) -> u32 {
        self.output_channels - self.aux_outputs.total_channels()
    }

    /// Channels on the main input bus: everything the aux buses do not claim.
    pub fn main_input_channels(&self) -> u32 {
        self.input_channels - self.aux_inputs.total_channels()
    }

    pub fn frame_count(&self) -> u32 {
        self.frame_count
    }
    pub fn input_channels(&self) -> u32 {
        self.input_channels
    }
    pub fn output_channels(&self) -> u32 {
        self.output_channels
    }
    pub fn layout(&self) -> BufferLayout {
        self.layout
    }

    /// Contiguous input channel, only available in planar layout.
    pub fn input_channel(&self, channel: u32) -> Option<&[f32]> {
        if self.layout != BufferLayout::Planar || channel >= self.input_channels {
            return None;
        }
        let n = self.frame_count as usize;
        let start = channel as usize * n;
        Some(&self.input[start..start + n])
    }

    /// Contiguous output channel, only available in planar layout.
    pub fn output_channel_mut(&mut self, channel: u32) -> Option<&mut [f32]> {
        if self.layout != BufferLayout::Planar || channel >= self.output_channels {
            return None;
        }
        let n = self.frame_count as usize;
        let start = channel as usize * n;
        Some(&mut self.output[start..start + n])
    }

    pub fn raw_input(&self) -> &[f32] {
        self.input
    }
    pub fn raw_output_mut(&mut self) -> &mut [f32] {
        self.output
    }

    /// Zero the whole output region. Used when a plugin reports silence or a
    /// process call is skipped.
    pub fn clear_output(&mut self) {
        let n = (self.output_channels * self.frame_count) as usize;
        self.output[..n].fill(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planar_channels_are_contiguous() {
        let input = vec![1.0, 1.0, 2.0, 2.0];
        let mut output = vec![0.0; 4];
        let mut b = AudioBuffers::new(&input, &mut output, 2, 2, 2, BufferLayout::Planar);
        assert_eq!(b.input_channel(0), Some(&[1.0f32, 1.0][..]));
        assert_eq!(b.input_channel(1), Some(&[2.0f32, 2.0][..]));
        assert!(b.input_channel(2).is_none());
        b.output_channel_mut(1).unwrap().fill(9.0);
        assert_eq!(output, vec![0.0, 0.0, 9.0, 9.0]);
    }
}
