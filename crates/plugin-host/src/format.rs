//! Which plugin format a path, an identity or an instance belongs to.

use std::path::Path;

/// The plugin formats this host can load.
///
/// An enum rather than a trait object: the set is closed, both variants are
/// compiled in, and the exhaustiveness check is what guarantees that adding a
/// third format cannot silently skip a code path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Format {
    Vst3,
    Clap,
}

/// Every format, in the order a browser should offer them.
pub const FORMATS: [Format; 2] = [Format::Vst3, Format::Clap];

impl Format {
    /// The file extension a module of this format carries.
    pub fn extension(self) -> &'static str {
        match self {
            Format::Vst3 => vst3_host::VST3_EXTENSION,
            Format::Clap => clap_host::CLAP_EXTENSION,
        }
    }

    /// The short tag written into saved state and shown in the UI.
    ///
    /// Stable: it is persisted in the DAW's project file, so renaming one of
    /// these breaks every existing project (ARCHITECTURE.md §8.3).
    pub fn tag(self) -> &'static str {
        match self {
            Format::Vst3 => "vst3",
            Format::Clap => "clap",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Format> {
        FORMATS.into_iter().find(|f| f.tag() == tag)
    }

    /// Infer the format from a module path's extension.
    ///
    /// The extension *is* the format for both: a `.vst3` and a `.clap` are
    /// distinguished by nothing else, since on Windows and Linux both are plain
    /// shared libraries.
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
