// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use eyre::{Result, WrapErr, bail};
use log::info;
use owo_colors::colors::Blue;

use crate::install;
use crate::logging::{LogDisplay as _, plural};

/// Prints the store paths instead of pushing them when `store` is `None`.
pub fn run(dir: &Path, store: Option<&str>) -> Result<()> {
    let root = fs::canonicalize(dir).wrap_err_with(|| format!("opening {}", dir.display()))?;
    let Some(workspace) = install::current_workspace(&root)? else {
        bail!("the last install does not match this project; run `hinata install` first");
    };

    let builds_path = workspace.join("builds.json");
    let builds: Vec<PathBuf> = serde_json::from_str(
        &fs::read_to_string(&builds_path)
            .wrap_err_with(|| format!("reading {}", builds_path.display()))?,
    )?;
    if builds.is_empty() {
        info!("no packages ran install scripts, so there is nothing to push");
        return Ok(());
    }

    let Some(store) = store else {
        for path in &builds {
            println!("{}", path.display());
        }
        return Ok(());
    };

    info!(
        "pushing {} and their dependencies to {}",
        plural(builds.len(), "build", "builds"),
        store.log_display::<Blue>()
    );
    let status = Command::new("nix")
        .args(["copy", "--extra-experimental-features", "nix-command"])
        .args(["--to", store])
        .args(&builds)
        .status()
        .wrap_err("running nix; is it installed?")?;
    if !status.success() {
        bail!("nix copy failed ({status})");
    }

    Ok(())
}
