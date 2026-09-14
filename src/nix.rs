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

use crate::lock::Lock;
use crate::logging::LogDisplay as _;

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
        .args(["build", "--extra-experimental-features", "nix-command"])
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
