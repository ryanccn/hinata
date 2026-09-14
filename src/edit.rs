// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use eyre::{Result, bail, eyre};
use log::{debug, info, warn};
use node_semver::Range;
use owo_colors::colors::Blue;

use crate::install::{self, Update};
use crate::logging::{LogDisplay as _, plural};
use crate::manifest::{Document, Group};
use crate::registry::{DEFAULT_REGISTRY, HttpRegistry, Registry};
use crate::resolve;

#[derive(Debug, PartialEq)]
struct PackageArg {
    alias: String,
    name: String,
    range: Option<String>,
}

pub fn add(dir: &Path, packages: &[String], group: Group, exact: bool) -> Result<()> {
    let root = fs::canonicalize(dir)?;
    let mut document = Document::open(&root)?;
    let args = packages
        .iter()
        .map(|arg| parse_package_arg(arg))
        .collect::<Result<Vec<_>>>()?;

    let lookups: Vec<String> = args
        .iter()
        .filter(|arg| arg.range.as_deref().is_none_or(is_tag))
        .map(|arg| arg.name.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let packuments: HashMap<_, _> = if lookups.is_empty() {
        HashMap::new()
    } else {
        debug!(
            "looking up tagged versions of {} in the registry",
            plural(lookups.len(), "package", "packages")
        );
        let fetched = HttpRegistry::new(DEFAULT_REGISTRY)?.fetch(&lookups)?;
        lookups.into_iter().zip(fetched).collect()
    };

    for arg in &args {
        let mut spec = match &arg.range {
            Some(range) if !is_tag(range) => range.clone(),
            tag => {
                let tag = tag.as_deref().unwrap_or("latest");
                let version = packuments[&arg.name]
                    .dist_tags
                    .get(tag)
                    .ok_or_else(|| eyre!("{} has no version tagged {tag}", arg.name))?;
                if exact {
                    version.clone()
                } else {
                    format!("^{version}")
                }
            }
        };
        if arg.alias != arg.name {
            spec = format!("npm:{}@{spec}", arg.name);
        }
        resolve::parse_spec(&arg.alias, &spec)?;
        document.set_dependency(group, &arg.alias, &spec);
        info!(
            "adding {} to {}",
            format_args!("{}@{spec}", arg.alias).log_display::<Blue>(),
            group.key().log_display::<Blue>()
        );
    }

    document.save()?;
    install_or_restore(&root, &document)
}

pub fn remove(dir: &Path, packages: &[String]) -> Result<()> {
    let root = fs::canonicalize(dir)?;
    let mut document = Document::open(&root)?;
    for alias in packages {
        if !document.remove_dependency(alias) {
            bail!("{alias} is not a dependency in package.json");
        }
        info!("removing {}", alias.log_display::<Blue>());
    }
    document.save()?;
    install_or_restore(&root, &document)
}

fn install_or_restore(root: &Path, document: &Document) -> Result<()> {
    let result = install::run(&install::Options {
        dir: root.to_path_buf(),
        dev: true,
        refresh: false,
        update: Update::Keep,
        lockfile: install::Lockfile::Save,
    });
    if result.is_err() {
        document.restore()?;
        warn!("install failed, so package.json was restored to how it was");
    }
    result
}

fn is_tag(range: &str) -> bool {
    Range::parse(range).is_err()
}

fn parse_package_arg(arg: &str) -> Result<PackageArg> {
    let (alias, rest) = split_version(arg);
    let (name, range) = match rest.and_then(|rest| rest.strip_prefix("npm:")) {
        Some(target) => split_version(target),
        None => (alias, rest),
    };
    if alias.is_empty() || name.is_empty() {
        bail!("invalid package {arg:?}");
    }
    Ok(PackageArg {
        alias: alias.to_string(),
        name: name.to_string(),
        range: range.filter(|range| !range.is_empty()).map(str::to_string),
    })
}

fn split_version(arg: &str) -> (&str, Option<&str>) {
    resolve::split_version(arg).map_or((arg, None), |(name, version)| (name, Some(version)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arg(alias: &str, name: &str, range: Option<&str>) -> PackageArg {
        PackageArg {
            alias: alias.to_string(),
            name: name.to_string(),
            range: range.map(str::to_string),
        }
    }

    #[test]
    fn parses_package_arguments() {
        assert_eq!(
            parse_package_arg("react").unwrap(),
            arg("react", "react", None)
        );
        assert_eq!(
            parse_package_arg("react@^18").unwrap(),
            arg("react", "react", Some("^18"))
        );
        assert_eq!(
            parse_package_arg("@types/node").unwrap(),
            arg("@types/node", "@types/node", None)
        );
        assert_eq!(
            parse_package_arg("@types/node@20").unwrap(),
            arg("@types/node", "@types/node", Some("20"))
        );
        assert_eq!(
            parse_package_arg("strip@npm:strip-ansi@^6").unwrap(),
            arg("strip", "strip-ansi", Some("^6"))
        );
        assert_eq!(
            parse_package_arg("x@npm:@scope/y").unwrap(),
            arg("x", "@scope/y", None)
        );
        assert!(parse_package_arg("").is_err());
    }

    #[test]
    fn tells_tags_from_ranges() {
        assert!(is_tag("latest"));
        assert!(is_tag("next"));
        assert!(!is_tag("^1.2.0"));
        assert!(!is_tag("1.2.3"));
        assert!(!is_tag("*"));
    }
}
