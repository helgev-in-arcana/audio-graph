//! `cargo xtask bundle audio-graph-plugin --release`.
//!
//! The bundling itself lives in `nice-plug-xtask`, which is the bundler
//! nice-plug's own examples use. Sharing it rather than rolling our own is what
//! keeps the bundle layout and the export macros in agreement: the bundler
//! decides which formats to emit by reading the exported symbols out of the
//! built binary, and those symbols come from `nice_export_vst3!`.
//!
//! What is *not* shared is finding the workspace root. `nice-plug-xtask`'s own
//! entry point walks up to the outermost ancestor holding a `Cargo.toml`, which
//! walks straight out of a git worktree kept inside the repository. The failure
//! is silent and expensive: it builds and bundles the main checkout while you
//! are looking at the worktree. This crate always sits directly under its own
//! workspace root, so that root is the one thing we can name without guessing —
//! hence resolving it from `CARGO_MANIFEST_DIR` rather than by walking up.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, anyhow, bail};

fn main() -> nice_plug_xtask::Result<()> {
    let root = workspace_root();
    std::env::set_current_dir(&root)
        .with_context(|| format!("entering the workspace root {}", root.display()))?;
    let target_dir = target_dir(&root)?;

    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    if command != "bundle" {
        bail!(
            "usage: cargo xtask bundle [-p] <package> [cargo build options]\n\n\
             Only `bundle` is wired up. `bundle-universal` and `known-packages` exist \
             upstream in nice-plug-xtask; add them here when a macOS release needs them."
        );
    }

    let (packages, cargo_args) = split_bundle_args(args)?;

    // One `cargo build` covering every package so cargo can parallelise them,
    // then bundle each in turn — the order nice-plug-xtask's own entry point
    // uses, and the reason the two steps are separate calls.
    nice_plug_xtask::build(&packages, &cargo_args)?;
    for package in &packages {
        nice_plug_xtask::bundle(&target_dir, package, &cargo_args, false)?;
        verify(&target_dir, &root, package)?;
    }

    Ok(())
}

/// The root of *this* workspace: the parent of the xtask crate, fixed at
/// compile time, so no amount of nesting can redirect it.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the xtask crate sits directly under the workspace root")
        .to_path_buf()
}

/// Asking cargo rather than assuming `target/`, so `CARGO_TARGET_DIR` and
/// `build.target-dir` keep working.
fn target_dir(root: &Path) -> nice_plug_xtask::Result<PathBuf> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(root)
        .output()
        .context("running `cargo metadata`")?;
    if !output.status.success() {
        bail!(
            "`cargo metadata` failed while looking for the target directory:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let json = String::from_utf8_lossy(&output.stdout);
    // A hand-rolled field lookup rather than a `serde_json` dependency: this is
    // the only field the xtask ever reads out of the metadata, and it is a
    // plain string at the top level.
    let key = "\"target_directory\":\"";
    let start = json
        .find(key)
        .map(|i| i + key.len())
        .ok_or_else(|| anyhow!("`cargo metadata` reported no target_directory"))?;
    let end = json[start..]
        .find('"')
        .ok_or_else(|| anyhow!("`cargo metadata`'s target_directory was not terminated"))?;
    // JSON escapes the backslashes in a Windows path; nothing else in a path
    // cargo itself produced needs unescaping.
    Ok(PathBuf::from(
        json[start..start + end].replace("\\\\", "\\"),
    ))
}

/// `-p foo -p bar <cargo args>`, or a bare `foo <cargo args>`.
fn split_bundle_args(
    args: impl Iterator<Item = String>,
) -> nice_plug_xtask::Result<(Vec<String>, Vec<String>)> {
    let mut args = args.peekable();
    let mut packages = Vec::new();
    if args.peek().map(String::as_str) == Some("-p") {
        while args.peek().map(String::as_str) == Some("-p") {
            args.next();
            packages.push(
                args.next()
                    .ok_or_else(|| anyhow!("`-p` needs a package name after it"))?,
            );
        }
    } else {
        packages.push(
            args.next()
                .ok_or_else(|| anyhow!("expected a package to bundle"))?,
        );
    }
    Ok((packages, args.collect()))
}

/// The formats a bundle can take.
///
/// Which of them appear is decided by the symbols the built binary exports, so
/// dropping an export macro costs a format without costing a build: everything
/// stays green and half of what people install stops being there. Requiring
/// both is what makes that loud. A package that ships only one format would
/// need this to become a `bundler.toml` question instead.
const FORMATS: [&str; 2] = ["vst3", "clap"];

/// Checks that bundling produced something a host can load.
///
/// The bundler decides the formats from the binary and writes them out; nothing
/// in that path fails loudly when it writes a shell of a directory with no
/// binary inside. Such a bundle is present by name and empty on load, which is
/// the worst of both — it is the shape that reaches a user as "the plugin does
/// not show up" with nothing in any log to say why.
///
/// This sits behind bundling rather than in the workflows so that it holds for
/// whoever bundles by hand as well, and so there is one of it rather than one
/// per workflow to keep in agreement.
fn verify(target_dir: &Path, root: &Path, package: &str) -> nice_plug_xtask::Result<()> {
    let name = bundle_name(root, package)?;
    let home = target_dir.join("bundled");
    for format in FORMATS {
        let bundle = home.join(format!("{name}.{format}"));
        let binary = binary_in(&bundle, &name)?;
        println!("{}: {}", bundle.display(), binary.display());
    }
    Ok(())
}

/// The binary `bundle` carries, whatever shape the bundle takes.
///
/// VST3 is a directory on every OS, CLAP is one on macOS and the shared library
/// itself everywhere else, so both shapes are allowed. What is not allowed is a
/// directory with nothing loadable under it. The search is for a file named
/// after the bundle rather than for any file at all: `Info.plist` is a file
/// too, and finding it would let an empty macOS bundle pass.
fn binary_in(bundle: &Path, name: &str) -> nice_plug_xtask::Result<PathBuf> {
    if bundle.is_dir() {
        return search(bundle, name)?
            .ok_or_else(|| anyhow!("{} carries no binary named {name}", bundle.display()));
    }

    let size = std::fs::metadata(bundle)
        .with_context(|| format!("{} was not produced", bundle.display()))?
        .len();
    if size == 0 {
        bail!("{} is empty", bundle.display());
    }
    Ok(bundle.to_path_buf())
}

/// The first non-empty file under `dir` whose stem is `name`.
///
/// The stem rather than the whole file name, because the same binary is
/// `AudioGraph.so`, `AudioGraph.vst3` and bare `AudioGraph` depending on the
/// platform the bundle was built for.
fn search(dir: &Path, name: &str) -> nice_plug_xtask::Result<Option<PathBuf>> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    for entry in entries {
        let path = entry
            .with_context(|| format!("reading an entry of {}", dir.display()))?
            .path();
        if path.is_dir() {
            if let Some(found) = search(&path, name)? {
                return Ok(Some(found));
            }
        } else if path.file_stem().is_some_and(|stem| stem == name)
            && path.metadata().is_ok_and(|meta| meta.len() > 0)
        {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// What the bundle is called: the product's name, not the crate's.
///
/// `bundler.toml` is the bundler's own file and the bundler is what puts the
/// name on the directory, so reading the same entry is what makes this look for
/// what was actually written. Falling back to the package name is the
/// bundler's own behaviour for a package the file says nothing about.
fn bundle_name(root: &Path, package: &str) -> nice_plug_xtask::Result<String> {
    let path = root.join("bundler.toml");
    let config = match std::fs::read_to_string(&path) {
        Ok(config) => config,
        // The file is optional; without it every package keeps its own name.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(anyhow!("reading {}: {error}", path.display())),
    };
    Ok(named(&config, package).unwrap_or_else(|| package.to_owned()))
}

/// The `name` of `package`'s section in a `bundler.toml`.
///
/// A hand-rolled lookup rather than a `toml` dependency, for the same reason
/// the target directory is read the way it is: one key, one shape, read once.
fn named(config: &str, package: &str) -> Option<String> {
    let mut ours = false;
    for line in config.lines() {
        let line = line.trim();
        if let Some(section) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            ours = section.trim().trim_matches('"') == package;
        } else if ours
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "name"
        {
            return Some(value.trim().trim_matches('"').to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundle is looked for under the name the bundler writes it as.
    ///
    /// Looking under the crate name instead would report every bundle missing,
    /// which is the same noise as a bundler that produced nothing.
    #[test]
    fn the_name_comes_from_the_bundler_config() {
        let config = "[audio-graph-plugin]\nname = \"AudioGraph\"\n";
        assert_eq!(
            named(config, "audio-graph-plugin").as_deref(),
            Some("AudioGraph")
        );
    }

    /// A directory with nothing loadable in it is a failure, not a bundle.
    ///
    /// This is the shape the check exists for: present by name, empty on load.
    /// A bundle carries files that are not the binary, so finding any file at
    /// all would let that shape through, and so would finding a binary of
    /// length zero.
    #[test]
    fn a_directory_without_a_binary_is_not_a_bundle() {
        let bundle = std::env::temp_dir().join("audio-graph-xtask-verify/AudioGraph.vst3");
        let contents = bundle.join("Contents/x86_64-linux");
        std::fs::create_dir_all(&contents).expect("making a bundle to look at");
        std::fs::write(contents.join("Info.plist"), b"not a binary").expect("writing a plist");
        assert!(binary_in(&bundle, "AudioGraph").is_err());

        let binary = contents.join("AudioGraph.so");
        std::fs::write(&binary, b"").expect("writing an empty binary");
        assert!(binary_in(&bundle, "AudioGraph").is_err());

        std::fs::write(&binary, b"ELF").expect("writing a binary");
        assert_eq!(binary_in(&bundle, "AudioGraph").ok(), Some(binary));

        std::fs::remove_dir_all(bundle.parent().expect("a parent to clean up")).ok();
    }

    /// A package the config says nothing about keeps its own name.
    #[test]
    fn a_package_without_a_section_is_not_renamed() {
        let config = "[other-plugin]\nname = \"Other\"\n";
        assert_eq!(named(config, "audio-graph-plugin"), None);
        // A section without a `name` is the same case: nothing renames it.
        assert_eq!(named("[audio-graph-plugin]\n", "audio-graph-plugin"), None);
    }
}
