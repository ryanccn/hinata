// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! What resolving changed, compared against the lockfile it replaces.

use std::collections::BTreeMap;

use log::{info, warn};
use owo_colors::OwoColorize as _;
use owo_colors::colors::Yellow;

use crate::lock::{Lock, Package};
use crate::logging::{LogDisplay as _, plural};

/// How many packages are listed before the rest are only counted.
const LISTED: usize = 20;

/// Tarball sources keyed by `name@version`, taken before the lockfile is resolved again.
pub struct Snapshot(BTreeMap<String, Source>);

struct Source {
    name: String,
    version: String,
    url: String,
    integrity: String,
}

pub fn snapshot(lock: Option<&Lock>) -> Snapshot {
    Snapshot(
        lock.into_iter()
            .flat_map(|lock| lock.packages.values())
            .map(|package| {
                let source = Source {
                    name: package.name.clone(),
                    version: package.version.clone(),
                    url: package.url.clone(),
                    integrity: package.integrity.clone(),
                };
                (key(package), source)
            })
            .collect(),
    )
}

pub fn report(before: &Snapshot, after: &Lock) {
    // A first resolution would list every package, which the count already says.
    if before.0.is_empty() {
        info!(
            "resolved {}",
            plural(after.packages.len(), "package", "packages")
        );
        return;
    }

    let changes = changes(before, after);
    let counts = [
        (changes.added.len(), format!("+{}", changes.added.len())),
        (changes.removed.len(), format!("-{}", changes.removed.len())),
        (
            changes.updated.len(),
            format!("{} updated", changes.updated.len()),
        ),
    ];
    let counts: Vec<String> = counts
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(_, part)| part)
        .collect();
    info!(
        "resolved {}{}",
        plural(after.packages.len(), "package", "packages"),
        if counts.is_empty() {
            String::new()
        } else {
            format!(" ({})", counts.join(", ")).dimmed().to_string()
        }
    );

    let added = changes.added.iter().map(|entry| {
        let scripts = if entry.scripts {
            " (install scripts)"
        } else {
            ""
        };
        format!("{} {} {}{scripts}", "+".green(), entry.name, entry.version)
    });
    let removed = changes
        .removed
        .iter()
        .map(|entry| format!("{} {} {}", "-".red(), entry.name, entry.version));
    let updated = changes
        .updated
        .iter()
        .map(|(name, from, to)| format!("{} {name} {from} → {to}", "~".yellow()));

    let total = changes.added.len() + changes.removed.len() + changes.updated.len();
    for line in added.chain(removed).chain(updated).take(LISTED) {
        info!("  {line}");
    }
    if total > LISTED {
        info!("  {}", format!("and {} more", total - LISTED).dimmed());
    }

    for entry in &changes.resupplied {
        warn!(
            "{} is the version the lockfile recorded, but no longer the tarball",
            format_args!("{}@{}", entry.name, entry.version).log_display::<Yellow>()
        );
    }
}

struct Changes {
    added: Vec<Entry>,
    removed: Vec<Entry>,
    /// The name, then the version before and after.
    updated: Vec<(String, String, String)>,
    /// Packages whose tarball changed while their version stayed the same.
    resupplied: Vec<Entry>,
}

struct Entry {
    name: String,
    version: String,
    /// Install scripts that will not run until the package is in `allowBuilds`.
    scripts: bool,
}

/// One version of a package leaving as another arrives is an update, not both.
fn changes(before: &Snapshot, after: &Lock) -> Changes {
    let current: BTreeMap<String, &Package> = after
        .packages
        .values()
        .map(|package| (key(package), package))
        .collect();

    let mut added: Vec<Entry> = current
        .iter()
        .filter(|(key, _)| !before.0.contains_key(*key))
        .map(|(_, package)| Entry {
            name: package.name.clone(),
            version: package.version.clone(),
            scripts: package.install_script && !package.build && !package.impure_build,
        })
        .collect();
    let mut removed: Vec<Entry> = before
        .0
        .iter()
        .filter(|(key, _)| !current.contains_key(*key))
        .map(|(_, source)| Entry {
            name: source.name.clone(),
            version: source.version.clone(),
            scripts: false,
        })
        .collect();

    let (added_once, removed_once) = (count_by_name(&added), count_by_name(&removed));
    let paired =
        |name: &str| added_once.get(name) == Some(&1) && removed_once.get(name) == Some(&1);

    let mut from: BTreeMap<String, String> = BTreeMap::new();
    removed.retain(|entry| {
        let pair = paired(&entry.name);
        if pair {
            from.insert(entry.name.clone(), entry.version.clone());
        }
        !pair
    });
    let mut updated = Vec::new();
    added.retain(|entry| match from.remove(&entry.name) {
        Some(version) => {
            updated.push((entry.name.clone(), version, entry.version.clone()));
            false
        }
        None => true,
    });

    let resupplied = current
        .iter()
        .filter(|(key, package)| {
            before.0.get(*key).is_some_and(|source| {
                source.url != package.url || source.integrity != package.integrity
            })
        })
        .map(|(_, package)| Entry {
            name: package.name.clone(),
            version: package.version.clone(),
            scripts: false,
        })
        .collect();

    Changes {
        added,
        removed,
        updated,
        resupplied,
    }
}

fn count_by_name(entries: &[Entry]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for entry in entries {
        *counts.entry(entry.name.clone()).or_default() += 1;
    }
    counts
}

fn key(package: &Package) -> String {
    format!("{}@{}", package.name, package.version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn lock(packages: &Value) -> Lock {
        serde_json::from_value(json!({
            "version": 1,
            "packages": packages,
            "sccs": [],
            "importers": { ".": {} },
        }))
        .unwrap()
    }

    fn package(version: &str) -> Value {
        json!({ "name": "ignored", "version": version, "url": "u", "integrity": "sha512-i" })
    }

    fn names(entries: &[Entry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| format!("{} {}", entry.name, entry.version))
            .collect()
    }

    #[test]
    fn pairs_a_removal_with_an_addition_of_the_same_package() {
        let before = lock(&json!({
            "vite@5.4.2": { "name": "vite", "version": "5.4.2", "url": "u", "integrity": "sha512-i" },
            "lodash@4.17.20": { "name": "lodash", "version": "4.17.20", "url": "u", "integrity": "sha512-i" },
        }));
        let after = lock(&json!({
            "vite@5.4.11": { "name": "vite", "version": "5.4.11", "url": "u", "integrity": "sha512-i" },
            "semver@7.6.3": { "name": "semver", "version": "7.6.3", "url": "u", "integrity": "sha512-i" },
        }));

        let changes = changes(&snapshot(Some(&before)), &after);
        assert_eq!(names(&changes.added), ["semver 7.6.3"]);
        assert_eq!(names(&changes.removed), ["lodash 4.17.20"]);
        assert_eq!(
            changes.updated,
            [(
                "vite".to_string(),
                "5.4.2".to_string(),
                "5.4.11".to_string()
            )]
        );
    }

    #[test]
    fn keeps_packages_apart_when_several_versions_change() {
        let before = lock(&json!({ "a@1.0.0": package("1.0.0"), "a@2.0.0": package("2.0.0") }));
        let after = lock(&json!({ "a@3.0.0": package("3.0.0") }));

        let changes = changes(&snapshot(Some(&before)), &after);
        assert_eq!(names(&changes.added), ["ignored 3.0.0"]);
        assert_eq!(names(&changes.removed), ["ignored 1.0.0", "ignored 2.0.0"]);
        assert!(changes.updated.is_empty());
    }

    #[test]
    fn reports_a_version_that_changed_tarball() {
        let before = lock(&json!({
            "a@1.0.0": { "name": "a", "version": "1.0.0", "url": "u", "integrity": "sha512-i" },
        }));
        let after = lock(&json!({
            "a@1.0.0": { "name": "a", "version": "1.0.0", "url": "u", "integrity": "sha512-other" },
        }));

        let changes = changes(&snapshot(Some(&before)), &after);
        assert_eq!(names(&changes.resupplied), ["a 1.0.0"]);
        assert!(changes.added.is_empty() && changes.removed.is_empty());
    }

    #[test]
    fn flags_new_packages_whose_install_scripts_are_skipped() {
        let after = lock(&json!({
            "esbuild@0.25.0": {
                "name": "esbuild", "version": "0.25.0", "url": "u", "integrity": "sha512-i",
                "installScript": true,
            },
            "sharp@0.33.0": {
                "name": "sharp", "version": "0.33.0", "url": "u", "integrity": "sha512-i",
                "installScript": true, "build": true,
            },
        }));

        let changes = changes(&snapshot(None), &after);
        let flagged: Vec<&str> = changes
            .added
            .iter()
            .filter(|entry| entry.scripts)
            .map(|entry| entry.name.as_str())
            .collect();
        assert_eq!(flagged, ["esbuild"]);
    }
}
