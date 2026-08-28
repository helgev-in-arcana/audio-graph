//! `cargo xtask bundle audio-graph-plugin --release`.
//!
//! Builds and bundles plugin packages using `nice-plug-xtask`.
//! Resolves the workspace root deterministically relative to `CARGO_MANIFEST_DIR`.

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
