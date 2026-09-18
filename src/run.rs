// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, ExitStatus};

use eyre::{Result, WrapErr, bail};
use log::{debug, info};
use owo_colors::OwoColorize as _;
use owo_colors::colors::Blue;

use crate::logging::LogDisplay as _;
use crate::manifest::{self, Manifest};

pub fn script(dir: &Path, name: &str, args: &[String]) -> Result<ExitStatus> {
    let root = fs::canonicalize(dir)?;
    let manifest = manifest::read(&root)?;
    if !manifest.scripts.contains_key(name) {
        let available: Vec<_> = manifest.scripts.keys().map(String::as_str).collect();
        bail!(
            "package.json has no script named {name} (available: {})",
            available.join(", ")
        );
    }

    let mut status = ExitStatus::default();
    for (event, body) in chain(&manifest.scripts, name) {
        let line = script_line(&event, body, name, args);
        info!("running {} {}", event.log_display::<Blue>(), line.dimmed());
        status = command(&root, &manifest, "sh")?
            .arg("-c")
            .arg(&line)
            .env("npm_lifecycle_event", &event)
            .status()
            .wrap_err_with(|| format!("running script {event}"))?;
        if !status.success() {
            break;
        }
    }
    Ok(status)
}

/// `pre` and `post` hooks run around the script they are named after, as in npm.
fn chain<'s>(scripts: &'s BTreeMap<String, String>, name: &str) -> Vec<(String, &'s str)> {
    [format!("pre{name}"), name.to_string(), format!("post{name}")]
        .into_iter()
        .filter_map(|event| {
            let body = scripts.get(&event)?;
            Some((event, body.as_str()))
        })
        .collect()
}

fn script_line(event: &str, body: &str, name: &str, args: &[String]) -> String {
    if event != name {
        return body.to_string();
    }
    std::iter::once(body.to_string())
        .chain(args.iter().map(|arg| shell_quote(arg)))
        .collect::<Vec<_>>()
        .join(" ")
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

    fn scripts(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(name, body)| ((*name).to_string(), (*body).to_string()))
            .collect()
    }

    #[test]
    fn runs_hooks_around_a_script() {
        let scripts = scripts(&[
            ("prebuild", "clean"),
            ("build", "tsc"),
            ("postbuild", "report"),
            ("test", "vitest"),
        ]);

        assert_eq!(
            chain(&scripts, "build"),
            vec![
                ("prebuild".to_string(), "clean"),
                ("build".to_string(), "tsc"),
                ("postbuild".to_string(), "report"),
            ]
        );
        assert_eq!(
            chain(&scripts, "test"),
            vec![("test".to_string(), "vitest")]
        );
    }

    #[test]
    fn passes_arguments_to_the_named_script_only() {
        let scripts = scripts(&[("prebuild", "clean"), ("build", "tsc")]);
        let args = ["--watch".to_string()];
        let lines: Vec<String> = chain(&scripts, "build")
            .into_iter()
            .map(|(event, body)| script_line(&event, body, "build", &args))
            .collect();

        assert_eq!(lines, ["clean", "tsc '--watch'"]);
    }
}
