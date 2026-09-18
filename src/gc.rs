// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use eyre::{Result, WrapErr};
use log::info;
use owo_colors::OwoColorize as _;
use owo_colors::colors::Blue;

use crate::logging::{LogDisplay as _, plural};
use crate::util;

pub fn run() -> Result<()> {
    let removed = util::prune_projects(&util::projects_dir()?);
    info!(
        "removed {} of projects that no longer exist",
        plural(removed, "GC root", "GC roots")
    );

    let metadata = util::metadata_dir()?;
    let size = dir_size(&metadata);
    if let Err(error) = fs::remove_dir_all(&metadata)
        && error.kind() != ErrorKind::NotFound
    {
        return Err(error).wrap_err_with(|| format!("removing {}", metadata.display()));
    }
    info!("cleared {} of registry metadata", megabytes(size).green());

    // Collecting garbage affects the whole Nix store, not only what hinata built.
    info!(
        "run {} to delete store paths that are no longer in use",
        "nix store gc".log_display::<Blue>()
    );

    Ok(())
}

fn dir_size(dir: &Path) -> u64 {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .map(|metadata| metadata.len())
        .sum()
}

fn megabytes(bytes: u64) -> String {
    format!("{}.{} MB", bytes / 1_000_000, bytes / 100_000 % 10)
}
