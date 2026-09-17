// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use eyre::{Result, WrapErr, bail, eyre};
use log::debug;
use owo_colors::colors::Blue;
use serde::Deserialize;
use serde_json::Value;

use crate::lock::{self, Lock};
use crate::logging::LogDisplay as _;
use crate::manifest;

pub const FLAKE_LOCK: &str = "flake.lock";

pub const LIBRARY: [(&str, &str); 3] = [
    (
        "nix_support/default.nix",
        include_str!("./nix_support/default.nix"),
    ),
    (
        "nix_support/hinata.sh",
        include_str!("./nix_support/hinata.sh"),
    ),
    (
        "nix_support/install.nix",
        include_str!("./nix_support/install.nix"),
    ),
];

/// `builtins.fetchTree` needs the flakes feature.
const FEATURES: [&str; 2] = ["--extra-experimental-features", "nix-command flakes"];

/// Nix evaluates from a temporary directory: reading a path makes it inspect the parent
/// directories, which can be denied for protected project locations.
pub fn build_workspace(
    root: &Path,
    lock: &Lock,
    lock_json: &str,
    dev: bool,
    node_major: Option<u32>,
    substituters: &BTreeMap<String, String>,
    out_link: &Path,
) -> Result<PathBuf> {
    let scratch = tempfile::Builder::new()
        .prefix("hinata-")
        .tempdir()
        .wrap_err("creating a temporary directory")?;
    let dir = fs::canonicalize(scratch.path())?;
    debug!(
        "evaluating from {}{}",
        dir.display().log_display::<Blue>(),
        node_major.map_or_else(String::new, |major| format!(" for Node.js {major}"))
    );
    for (path, contents) in LIBRARY {
        let path = dir.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, contents).wrap_err_with(|| format!("writing {}", path.display()))?;
    }
    fs::write(dir.join("hinata.lock"), lock_json)?;

    for patch in lock
        .packages
        .values()
        .filter_map(|package| package.patch.as_ref())
    {
        let path = dir.join(&patch.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(root.join(&patch.path), &path)
            .wrap_err_with(|| format!("copying {}", patch.path))?;
    }

    let mut command = Command::new("nix");
    command
        .current_dir(&dir)
        .arg("build")
        .args(FEATURES)
        .args(["--impure", "--print-out-paths", "--out-link"])
        .arg(out_link)
        .arg("--file")
        .arg(dir.join("nix_support/install.nix"))
        .args(["--arg", "dev", if dev { "true" } else { "false" }]);

    if let Some(major) = node_major {
        command.args(["--arg", "nodeMajor", &major.to_string()]);
    }

    // Nix ignores these, with a warning, unless the user is trusted or they are trusted substituters.
    if !substituters.is_empty() {
        let urls: Vec<&str> = substituters.keys().map(String::as_str).collect();
        let keys: Vec<&str> = substituters.values().map(String::as_str).collect();
        command
            .args(["--extra-substituters", &urls.join(" ")])
            .args(["--extra-trusted-public-keys", &keys.join(" ")]);
    }

    let output = command
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .wrap_err("running nix; is it installed?")?;

    if !output.status.success() {
        bail!("nix build failed ({})", output.status);
    }

    let stdout = String::from_utf8(output.stdout).wrap_err("nix printed a non-UTF-8 path")?;
    let path = stdout
        .lines()
        .next()
        .ok_or_else(|| eyre!("nix build printed no output path"))?;

    Ok(PathBuf::from(path))
}

#[derive(Deserialize)]
struct Metadata {
    locked: BTreeMap<String, Value>,
}

/// Runs outside the project, where a bare reference such as `nixpkgs` could name a directory.
pub fn lock_nixpkgs(flake_ref: &str) -> Result<lock::Nixpkgs> {
    let output = Command::new("nix")
        .current_dir(std::env::temp_dir())
        .args(["flake", "metadata", "--json"])
        .args(FEATURES)
        .arg(flake_ref)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .wrap_err("running nix; is it installed?")?;

    if !output.status.success() {
        bail!("nix flake metadata {flake_ref} failed ({})", output.status);
    }

    let metadata: Metadata = serde_json::from_slice(&output.stdout)
        .wrap_err_with(|| format!("parsing the metadata of {flake_ref}"))?;
    shareable(flake_ref, metadata.locked).ok_or_else(|| {
        eyre!("{flake_ref} locks to a local path, which other machines cannot fetch")
    })
}

/// Versions of Node.js in `nixpkgs`, keyed by attribute.
pub fn node_versions(nixpkgs: &lock::Nixpkgs) -> Result<BTreeMap<String, String>> {
    let output = Command::new("nix")
        .current_dir(std::env::temp_dir())
        .args(["eval", "--impure", "--json"])
        .args(FEATURES)
        .args(["--expr", include_str!("./nix_support/nodejs.nix")])
        .env("HINATA_NIXPKGS", serde_json::to_string(&nixpkgs.locked)?)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .wrap_err("running nix; is it installed?")?;

    if !output.status.success() {
        bail!(
            "listing Node.js versions in Nixpkgs from {} failed ({})",
            nixpkgs.from,
            output.status
        );
    }

    serde_json::from_slice(&output.stdout).wrap_err("parsing Node.js versions in Nixpkgs")
}

#[derive(Deserialize)]
struct FlakeLock {
    nodes: BTreeMap<String, FlakeNode>,
    root: String,
}

#[derive(Deserialize)]
struct FlakeNode {
    #[serde(default)]
    inputs: BTreeMap<String, Value>,
    locked: Option<BTreeMap<String, Value>>,
}

/// The `nixpkgs` input of the flake in `root`. Inputs that follow others are lists, and ignored.
pub fn flake_lock_nixpkgs(root: &Path) -> Result<Option<lock::Nixpkgs>> {
    let path = root.join(FLAKE_LOCK);
    let Some(source) =
        manifest::read_if_exists(&path).wrap_err_with(|| format!("reading {}", path.display()))?
    else {
        return Ok(None);
    };
    let flake: FlakeLock =
        serde_json::from_str(&source).wrap_err_with(|| format!("parsing {}", path.display()))?;

    flake
        .nodes
        .get(&flake.root)
        .and_then(|root| root.inputs.get("nixpkgs"))
        .and_then(Value::as_str)
        .and_then(|name| flake.nodes.get(name))
        .and_then(|node| node.locked.clone())
        .map(|locked| {
            shareable(FLAKE_LOCK, locked).ok_or_else(|| {
                eyre!(
                    "the nixpkgs input in {} is a local path, which other machines cannot fetch",
                    path.display()
                )
            })
        })
        .transpose()
}

/// `None` for local paths, which other machines cannot fetch.
fn shareable(flake_ref: &str, mut locked: BTreeMap<String, Value>) -> Option<lock::Nixpkgs> {
    if locked.get("type").and_then(Value::as_str) == Some("path") {
        return None;
    }
    // Internal to Nix, which refuses it in `builtins.fetchTree`.
    locked.remove("__final");

    Some(lock::Nixpkgs {
        from: flake_ref.to_string(),
        locked,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_only_shareable_sources() {
        let locked = |json: &str| serde_json::from_str::<BTreeMap<String, Value>>(json).unwrap();

        let github = r#"{ "type": "github", "owner": "NixOS", "repo": "nixpkgs", "rev": "abc", "narHash": "sha256-x", "lastModified": 1 }"#;
        let mut final_github = locked(github);
        final_github.insert("__final".to_string(), Value::from(true));
        assert_eq!(
            shareable("nixpkgs", final_github).unwrap().locked,
            locked(github)
        );

        assert!(
            shareable(
                "nixpkgs",
                locked(
                    r#"{ "type": "path", "path": "/nix/store/x-source", "narHash": "sha256-x" }"#
                ),
            )
            .is_none()
        );
    }
}
