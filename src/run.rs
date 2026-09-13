// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::fs;
use std::path::Path;
use std::process::{Command, ExitStatus};

use eyre::{Result, WrapErr, eyre};
use log::{debug, info};
use owo_colors::OwoColorize as _;
use owo_colors::colors::Blue;

use crate::logging::LogDisplay as _;
use crate::manifest::{self, Manifest};

pub fn script(dir: &Path, name: &str, args: &[String]) -> Result<ExitStatus> {
    let root = fs::canonicalize(dir)?;
    let manifest = manifest::read(&root)?;
    let body = manifest.scripts.get(name).ok_or_else(|| {
        let available: Vec<_> = manifest.scripts.keys().map(String::as_str).collect();
        eyre!(
            "package.json has no script named {name} (available: {})",
            available.join(", ")
        )
    })?;
    let line = std::iter::once(body.clone())
        .chain(args.iter().map(|arg| shell_quote(arg)))
        .collect::<Vec<_>>()
        .join(" ");
    info!("running {} {}", name.log_display::<Blue>(), line.dimmed());
    command(&root, &manifest, "sh")?
        .arg("-c")
        .arg(&line)
        .env("npm_lifecycle_event", name)
        .status()
        .wrap_err_with(|| format!("running script {name}"))
}

pub fn exec(dir: &Path, program: &str, args: &[String]) -> Result<ExitStatus> {
    let root = fs::canonicalize(dir)?;
    let manifest = manifest::read(&root)?;
    debug!(
        "running {} with {} on PATH",
        program.log_display::<Blue>(),
        "node_modules/.bin".log_display::<Blue>()
    );
    command(&root, &manifest, program)?
        .args(args)
        .status()
        .wrap_err_with(|| format!("running {program}"))
}

fn command(root: &Path, manifest: &Manifest, program: &str) -> Result<Command> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let paths = std::iter::once(root.join("node_modules/.bin")).chain(std::env::split_paths(&path));
    let mut command = Command::new(program);
    command
        .current_dir(root)
        .env("PATH", std::env::join_paths(paths)?)
        .env("INIT_CWD", std::env::current_dir()?);
    if let Some(name) = &manifest.name {
        command.env("npm_package_name", name);
    }
    if let Some(version) = &manifest.version {
        command.env("npm_package_version", version);
    }
    Ok(command)
}

fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_shell_arguments() {
        assert_eq!(shell_quote("--watch"), "'--watch'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }
}
