//! Which parameter a MIDI controller drives, for the audio thread.
//!
//! VST3 never delivers a control change to a plugin as a MIDI message. The
//! format requires that "any functionality that is to be controlled by MIDI
//! controllers must be exported as regular parameter", and `IMidiMapping` is
//! how a plugin says which parameter that is. A host that skips this simply
//! has no sustain pedal, no mod wheel, and no pitch bend.
//!
//! The table is read once at activate, on the main thread, for the same reason
//! [`crate::param_map`] is: `IMidiMapping` lives on `IEditController`, which
//! the audio thread may not touch.

use vst3::ComPtr;
use vst3::Steinberg::Vst::{ControllerNumbers_, IMidiMapping, IMidiMappingTrait};

/// Controller numbers 0..128 are the CCs; VST3 continues the range with two
/// more that are not CCs at all.
pub const AFTERTOUCH: u16 = ControllerNumbers_::kAfterTouch as u16;
pub const PITCH_BEND: u16 = ControllerNumbers_::kPitchBend as u16;
const COUNT: usize = ControllerNumbers_::kCountCtrlNumber as usize;
const CHANNELS: usize = 16;

/// Controller number and channel to parameter id.
///
/// Absent entries are the norm: most plugins map a handful of controllers and
/// answer `kResultFalse` for the rest.
pub struct MidiMap {
    /// `channel * COUNT + controller`, or empty when the plugin has no mapping.
    entries: Vec<Option<u32>>,
}

impl MidiMap {
    /// Ask the controller about every channel and controller number.
    ///
    /// 2080 calls sounds like a lot; it happens once per activate, on the main
    /// thread, and the interface offers no way to enumerate only what exists.
    pub fn build(mapping: Option<&ComPtr<IMidiMapping>>) -> MidiMap {
        let Some(mapping) = mapping else {
            return MidiMap {
                entries: Vec::new(),
            };
        };

        let mut entries = vec![None; CHANNELS * COUNT];
        for channel in 0..CHANNELS {
            for controller in 0..COUNT {
                let mut id = 0u32;
                let ok = unsafe {
                    mapping.getMidiControllerAssignment(
                        0,
                        channel as i16,
                        controller as i16,
                        &mut id,
                    )
                };
                if ok == vst3::Steinberg::kResultOk {
                    entries[channel * COUNT + controller] = Some(id);
                }
            }
        }
        MidiMap { entries }
    }

    /// A map with no assignments, for tests where the routing is not what is
    /// under test. Production gets the same thing from `build(None)`.
    #[cfg(test)]
    pub(crate) fn empty() -> MidiMap {
        MidiMap {
            entries: Vec::new(),
        }
    }

    /// Build a map directly from `(channel, controller, param id)` triples.
    #[cfg(test)]
    pub(crate) fn from_assignments(assignments: &[(i16, u16, u32)]) -> MidiMap {
        let mut entries = vec![None; CHANNELS * COUNT];
        for &(channel, controller, id) in assignments {
            entries[channel as usize * COUNT + usize::from(controller)] = Some(id);
        }
        MidiMap { entries }
    }

    /// The parameter a controller drives on a channel. Audio-thread safe.
    pub fn param(&self, channel: i16, controller: u16) -> Option<u32> {
        let channel = usize::try_from(channel).ok()?;
        if channel >= CHANNELS || usize::from(controller) >= COUNT {
            return None;
        }
        *self
            .entries
            .get(channel * COUNT + usize::from(controller))?
    }
}
