//! Plugin format identification and path resolution.

use std::path::Path;

/// Supported plugin formats that can be loaded by this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Format {
    Vst3,
    Clap,
}

/// Serialized using the stable string tag.
impl serde::Serialize for Format {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.tag())
    }
}

impl<'de> serde::Deserialize<'de> for Format {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Format, D::Error> {
        let tag = <String as serde::Deserialize>::deserialize(d)?;
        Format::from_tag(&tag)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown plugin format {tag:?}")))
    }
}

/// Every format, in display order.
pub const FORMATS: [Format; 2] = [Format::Vst3, Format::Clap];

impl Format {
    /// The standard file extension for this format.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Vst3 => vst3_host::VST3_EXTENSION,
            Format::Clap => clap_host::CLAP_EXTENSION,
        }
    }

    /// The short stable tag written into saved state and shown in the UI.
    pub fn tag(self) -> &'static str {
        match self {
            Format::Vst3 => "vst3",
            Format::Clap => "clap",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Format> {
        FORMATS.into_iter().find(|f| f.tag() == tag)
    }

    /// Infer the format from a module path's file extension.
    pub fn from_path(path: &Path) -> Option<Format> {
        let ext = path.extension()?.to_str()?;
        FORMATS
            .into_iter()
            .find(|f| ext.eq_ignore_ascii_case(f.extension()))
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Format::Vst3 => "VST3",
            Format::Clap => "CLAP",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_names_its_own_format() {
        assert_eq!(
            Format::from_path(Path::new("C:/x/OTT.vst3")),
            Some(Format::Vst3)
        );
        assert_eq!(
            Format::from_path(Path::new("/usr/lib/clap/Surge XT.clap")),
            Some(Format::Clap)
        );
        // Case-insensitive: Windows hands paths back however they were typed.
        assert_eq!(Format::from_path(Path::new("x.CLAP")), Some(Format::Clap));
        assert_eq!(Format::from_path(Path::new("x.dll")), None);
    }

    #[test]
    fn tags_round_trip() {
        // These are persisted, so a change here is a change to the save format.
        for format in FORMATS {
            assert_eq!(Format::from_tag(format.tag()), Some(format));
        }
        assert_eq!(Format::from_tag("vst3"), Some(Format::Vst3));
        assert_eq!(Format::from_tag("clap"), Some(Format::Clap));
    }
}
