//! Audio buffer representation.
//!
//! Audio channels are stored in one contiguous, flat buffer rather than as a
//! slice of slices: a nested slice cannot live in shared memory, so the nested
//! form would quietly rule out ever moving a backend out of process.

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
/// Aux means everything after the main bus: on the way in a sidechain, a
/// second sidechain, a key input; on the way out a second stereo pair, a
/// per-scene output. Fixed-size so [`AudioConfig`] stays `Copy` and carries no
/// pointer, which is what lets it cross a process boundary unchanged.
pub const MAX_AUX_BUSES: usize = 3;

/// The auxiliary buses of one plugin in one direction, represented by their channel widths.
///
/// Empty is the common case and the default: most plugins have one input bus,
/// and a graph that wires nothing to a sidechain should not make the host
/// negotiate one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AuxBuses {
    widths: [u16; MAX_AUX_BUSES],
    count: u8,
}

impl AuxBuses {
    /// Takes the first [`MAX_AUX_BUSES`] widths. Extra ones are dropped rather
    /// than refused: the compiler has already checked the graph against the
    /// same ceiling, so anything beyond it is a bug on this side, not the
    /// user's.
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
/// Changing any of it requires a deactivate/activate cycle, which the shape of
/// [`SubPluginMain::activate`][crate::SubPluginMain::activate] enforces: the
/// processor is handed out by value, so the configuration cannot be changed
/// while one exists.
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
    /// Separate from `input_channels` rather than folded into a list of buses
    /// because bus 0 is not like the others: it is the one a plugin processes,
    /// and the rest are things it looks at.
    pub aux_inputs: AuxBuses,
    /// Extra output buses beyond the main one (e.g., auxiliary or multi-out pairs).
    ///
    /// The same asymmetry as the inputs, read the other way: bus 0 is the
    /// plugin's output, and the rest are things it also produces — a
    /// per-scene pair, a drum machine's individual outs. Only the buses the
    /// graph actually reads are asked for; the plugin's own extras beyond them
    /// are left inactive so it need not compute them.
    pub aux_outputs: AuxBuses,
    /// True when the host is rendering faster than real time.
    pub offline: bool,
}

impl AudioConfig {
    /// Reject dimensions that cannot be represented by the native processing APIs.
    pub fn validate(&self) -> crate::Result<()> {
        if !self.sample_rate.is_finite() || self.sample_rate <= 0.0 {
            return Err(crate::HostError::InvalidState("invalid sample rate"));
        }
        if self.max_block_size == 0 || self.max_block_size > i32::MAX as u32 {
            return Err(crate::HostError::InvalidState("invalid maximum block size"));
        }
        for (main, aux) in [
            (self.input_channels, self.aux_inputs),
            (self.output_channels, self.aux_outputs),
        ] {
            let valid = main
                .checked_add(aux.total_channels())
                .is_some_and(|channels| {
                    channels <= i32::MAX as u32
                        && sample_count(channels, self.max_block_size).is_some()
                });
            if !valid || aux.iter().any(|width| width == 0) {
                return Err(crate::HostError::InvalidState(
                    "invalid audio bus dimensions",
                ));
            }
        }
        Ok(())
    }

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

fn sample_count(channels: u32, frames: u32) -> Option<usize> {
    let samples = usize::try_from(channels)
        .ok()?
        .checked_mul(usize::try_from(frames).ok()?)?;
    (samples <= isize::MAX as usize / size_of::<f32>()).then_some(samples)
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
/// The input region holds the main bus first and then each aux bus, packed —
/// so `input_channels` is the total and `aux_inputs` says where the joins are.
/// The output region is the same shape. One region per direction rather than
/// one per bus because a nested slice cannot live in shared memory, and the
/// buses are contiguous anyway.
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
    /// If dimensions overflow the addressable sample range or either slice is too short.
    pub fn new(
        input: &'a [f32],
        output: &'a mut [f32],
        input_channels: u32,
        output_channels: u32,
        frame_count: u32,
        layout: BufferLayout,
    ) -> Self {
        assert!(
            sample_count(input_channels, frame_count).is_some_and(|n| input.len() >= n),
            "input buffer too small"
        );
        assert!(
            sample_count(output_channels, frame_count).is_some_and(|n| output.len() >= n),
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

    /// Whether native planar pointers can use this block with the activation's bus boundaries.
    ///
    /// Slice lengths are established by construction; matching totals alone would
    /// still allow a main channel to be interpreted as a sidechain channel.
    pub fn matches_config(&self, config: &AudioConfig) -> bool {
        self.layout == BufferLayout::Planar
            && self.frame_count <= config.max_block_size
            && self.main_input_channels() == config.input_channels
            && self.main_output_channels() == config.output_channels
            && self.aux_inputs == config.aux_inputs
            && self.aux_outputs == config.aux_outputs
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
        let n = self.output_channels as usize * self.frame_count as usize;
        self.output[..n].fill(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Native dimensions and sample ranges cannot wrap during activation.
    #[test]
    fn invalid_configurations_are_rejected() {
        for sample_rate in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert!(
                AudioConfig {
                    sample_rate,
                    ..Default::default()
                }
                .validate()
                .is_err()
            );
        }
        for max_block_size in [0, u32::MAX] {
            assert!(
                AudioConfig {
                    max_block_size,
                    ..Default::default()
                }
                .validate()
                .is_err()
            );
        }
        let config = AudioConfig {
            input_channels: u32::MAX,
            aux_inputs: AuxBuses::new(&[2]),
            ..Default::default()
        };
        assert!(config.validate().is_err());
        assert_eq!(sample_count(u32::MAX, u32::MAX), None);
        assert!(AudioConfig::default().validate().is_ok());
    }

    /// Equal channel totals do not make different bus boundaries interchangeable.
    #[test]
    fn blocks_match_layout_and_bus_boundaries() {
        let config = AudioConfig {
            max_block_size: 4,
            aux_inputs: AuxBuses::new(&[2]),
            ..Default::default()
        };
        let input = [0.0; 16];
        let mut output = [0.0; 8];
        for frames in [0, 1, 4] {
            let buffers =
                AudioBuffers::new(&input, &mut output, 4, 2, frames, BufferLayout::Planar)
                    .with_aux_inputs(AuxBuses::new(&[2]));
            assert!(buffers.matches_config(&config));
        }
        for (layout, aux) in [
            (BufferLayout::Interleaved, AuxBuses::new(&[2])),
            (BufferLayout::Planar, AuxBuses::new(&[1, 1])),
            (BufferLayout::Planar, AuxBuses::default()),
        ] {
            let buffers =
                AudioBuffers::new(&input, &mut output, 4, 2, 4, layout).with_aux_inputs(aux);
            assert!(!buffers.matches_config(&config));
        }
    }

    /// Short storage is rejected before a buffer view can reach a processor.
    #[test]
    #[should_panic(expected = "input buffer too small")]
    fn short_storage_is_rejected() {
        AudioBuffers::new(&[0.0; 3], &mut [0.0; 4], 2, 2, 2, BufferLayout::Planar);
    }

    /// Multiplication overflow cannot turn enormous dimensions into a small slice.
    #[test]
    #[should_panic(expected = "input buffer too small")]
    fn overflowing_storage_is_rejected() {
        AudioBuffers::new(
            &[],
            &mut [],
            u32::MAX,
            u32::MAX,
            u32::MAX,
            BufferLayout::Planar,
        );
    }

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
