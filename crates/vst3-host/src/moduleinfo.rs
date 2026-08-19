//! `moduleinfo.json` — the bundle's self-description.
//!
//! VST3 3.7.5 onwards lets a bundle declare its classes in a JSON file under
//! `Contents/`, so a host can scan it without executing third-party code. It is
//! optional and the factory always wins, so nothing here is on a critical path;
//! it exists so scanning a plugin folder does not mean loading every DLL in it.

use serde::Deserialize;

use crate::cid::Cid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleInfoError {
    Json(String),
}

impl std::fmt::Display for ModuleInfoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModuleInfoError::Json(s) => write!(f, "moduleinfo.json: {s}"),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ModuleInfo {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "Factory Info")]
    pub factory_info: ModuleFactoryInfo,
    #[serde(rename = "Classes")]
    pub classes: Vec<ModuleClass>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ModuleFactoryInfo {
    #[serde(rename = "Vendor")]
    pub vendor: String,
    #[serde(rename = "URL")]
    pub url: String,
    #[serde(rename = "E-Mail")]
    pub email: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ModuleClass {
    /// 32 hex digits, matching [`Cid::to_hex`].
    #[serde(rename = "CID")]
    pub cid: String,
    #[serde(rename = "Category")]
    pub category: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Vendor")]
    pub vendor: String,
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "SDKVersion")]
    pub sdk_version: String,
    #[serde(rename = "Sub Categories")]
    pub subcategories: Vec<String>,
}

impl ModuleClass {
    pub fn parsed_cid(&self) -> Option<Cid> {
        Cid::from_hex(&self.cid)
    }
}

impl ModuleInfo {
    pub fn parse(text: &str) -> Result<ModuleInfo, ModuleInfoError> {
        // Some vendors ship the file with `//` comments, which the SDK's own
        // reader tolerates; strip them rather than rejecting the whole bundle.
        let cleaned = strip_line_comments(text);
        serde_json::from_str(&cleaned).map_err(|e| ModuleInfoError::Json(e.to_string()))
    }
}

/// Remove `//` comments that are not inside a string literal.
fn strip_line_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        // a comment the SDK writer sometimes emits
        "Name": "Example",
        "Version": "1.0.0",
        "Factory Info": { "Vendor": "Acme", "URL": "https://acme.example", "E-Mail": "a@b.c" },
        "Classes": [
            {
                "CID": "12345678123456781234567812345678",
                "Category": "Audio Module Class",
                "Name": "Example Synth",
                "Vendor": "Acme",
                "Version": "1.0.0",
                "SDKVersion": "VST 3.7.5",
                "Sub Categories": ["Instrument", "Synth"]
            }
        ]
    }"#;

    #[test]
    fn parses_a_typical_file() {
        let info = ModuleInfo::parse(SAMPLE).unwrap();
        assert_eq!(info.name, "Example");
        assert_eq!(info.factory_info.vendor, "Acme");
        assert_eq!(info.classes.len(), 1);
        assert_eq!(info.classes[0].subcategories, ["Instrument", "Synth"]);
        assert!(info.classes[0].parsed_cid().is_some());
    }

    #[test]
    fn unknown_and_missing_fields_are_tolerated() {
        let info = ModuleInfo::parse(r#"{"Name":"X","Compatibility":[{"New":"y"}]}"#).unwrap();
        assert_eq!(info.name, "X");
        assert!(info.classes.is_empty());
    }

    #[test]
    fn comment_stripping_leaves_string_contents_alone() {
        let out = strip_line_comments(r#"{"url":"https://x.example"} // tail"#);
        assert_eq!(out.trim(), r#"{"url":"https://x.example"}"#);
    }
}
