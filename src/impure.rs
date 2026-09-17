// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use eyre::{Result, WrapErr, bail, eyre};
use log::info;
use owo_colors::OwoColorize as _;
use owo_colors::colors::Blue;
use serde_json::Value;

use crate::link;
use crate::logging::LogDisplay as _;

/// Runs the install scripts of the package `name` at `dir` with the user's environment and network
/// access. `dir` is a read-only store path, so scripts can only write outside of the package.
pub fn run(
    project: &Path,
    name: &str,
    version: &str,
    dir: &Path,
    node: Option<&Path>,
) -> Result<()> {
    let manifest_path = dir.join("package.json");
    let manifest: Value = serde_json::from_str(
        &fs::read_to_string(&manifest_path)
            .wrap_err_with(|| format!("reading {}", manifest_path.display()))?,
    )
    .wrap_err_with(|| format!("parsing {}", manifest_path.display()))?;
    let scripts: Vec<(&str, &str)> = ["preinstall", "install", "postinstall"]
        .into_iter()
        .filter_map(|event| Some((event, manifest.get("scripts")?.get(event)?.as_str()?)))
        .collect();
    if scripts.is_empty() {
        return Ok(());
    }

    // The store has no `.bin` for the package's dependencies, so they are linked in a scratch
    // directory instead.
    let node_modules = dir
        .ancestors()
        .nth(name.split('/').count())
        .ok_or_else(|| eyre!("{} is not inside node_modules", dir.display()))?;
    let scratch = tempfile::Builder::new()
        .prefix("hinata-")
        .tempdir()
        .wrap_err("creating a temporary directory")?;
    let bins = scratch.path().join("node_modules");
    link::sync(&bins, &siblings(node_modules)?, node)?;
    let path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bins.join(".bin")).chain(std::env::split_paths(&path)),
    )?;

    for (event, script) in scripts {
        info!(
            "running the {} script of {} {}",
            event.log_display::<Blue>(),
            name.log_display::<Blue>(),
            script.dimmed()
        );
        let status = Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(dir)
            .env("PATH", &path)
            .env("INIT_CWD", project)
            .env("npm_package_name", name)
            .env("npm_package_version", version)
            .env("npm_lifecycle_event", event)
            .status()
            .wrap_err_with(|| format!("running the {event} script of {name}"))?;
        if !status.success() {
            bail!("the {event} script of {name} failed ({status})");
        }
    }
    Ok(())
}

fn siblings(node_modules: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut packages = BTreeMap::new();
    for entry in fs::read_dir(node_modules)
        .wrap_err_with(|| format!("reading {}", node_modules.display()))?
    {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if name.starts_with('@') {
            for scoped in fs::read_dir(entry.path())? {
                let scoped = scoped?;
                if let Some(child) = scoped.file_name().to_str() {
                    packages.insert(format!("{name}/{child}"), scoped.path());
                }
            }
        } else {
            packages.insert(name, entry.path());
        }
    }
    Ok(packages)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn runs_scripts_with_dependency_bins_and_npm_variables() {
        let root = tempfile::tempdir().unwrap();
        let node_modules = root.path().join("store/node_modules");
        let package = node_modules.join("@scope/pkg");
        let dep = node_modules.join("dep");
        let out = root.path().join("out");
        fs::create_dir_all(&package).unwrap();
        fs::create_dir_all(&dep).unwrap();
        fs::write(
            dep.join("package.json"),
            r#"{ "name": "dep", "bin": { "dep-bin": "bin.sh" } }"#,
        )
        .unwrap();
        fs::write(dep.join("bin.sh"), "#!/bin/sh\necho from-dep\n").unwrap();
        fs::set_permissions(dep.join("bin.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            package.join("package.json"),
            serde_json::json!({
                "name": "@scope/pkg",
                "scripts": {
                    "preinstall": format!("dep-bin > {}", out.display()),
                    "postinstall": format!(
                        r#"echo "$npm_package_name@$npm_package_version $npm_lifecycle_event $INIT_CWD" >> {}"#,
                        out.display()
                    ),
                },
            })
            .to_string(),
        )
        .unwrap();

        run(Path::new("/project"), "@scope/pkg", "1.0.0", &package, None).unwrap();

        assert_eq!(
            fs::read_to_string(&out).unwrap(),
            "from-dep\n@scope/pkg@1.0.0 postinstall /project\n"
        );
    }

    #[test]
    fn fails_when_a_script_fails() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("node_modules/pkg");
        fs::create_dir_all(&package).unwrap();
        fs::write(
            package.join("package.json"),
            r#"{ "name": "pkg", "scripts": { "postinstall": "exit 3" } }"#,
        )
        .unwrap();

        let error = run(root.path(), "pkg", "1.0.0", &package, None).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("postinstall script of pkg failed")
        );
    }
}
