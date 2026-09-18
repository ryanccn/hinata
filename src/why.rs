// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! The routes by which importers reach a package.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::Path;

use eyre::{Result, WrapErr, bail, eyre};

use crate::lock::{LOCKFILE, Lock};
use crate::{manifest, pnpm, resolve};

pub fn run(dir: &Path, package: &str) -> Result<()> {
    let root = fs::canonicalize(dir).wrap_err_with(|| format!("opening {}", dir.display()))?;
    let lock = read(&root)?;
    let (name, version) = match resolve::split_version(package) {
        Some((name, version)) => (name, Some(version)),
        None => (package, None),
    };

    let routes = routes(&lock, name, version);
    if routes.is_empty() {
        bail!("nothing depends on {package}");
    }
    for route in routes {
        println!("{route}");
    }
    Ok(())
}

fn read(root: &Path) -> Result<Lock> {
    let path = root.join(LOCKFILE);
    if let Some(json) =
        manifest::read_if_exists(&path).wrap_err_with(|| format!("reading {LOCKFILE}"))?
    {
        return serde_json::from_str(&json).wrap_err_with(|| format!("parsing {LOCKFILE}"));
    }

    let manifest = manifest::read(root)?;
    pnpm::read(root, &manifest.patches(root)?)?.ok_or_else(|| {
        eyre!(
            "this project has no {LOCKFILE} or {}; run `hinata install` first",
            pnpm::LOCKFILE
        )
    })
}

/// One line per importer, dependency group and matching instance, along a shortest route.
fn routes(lock: &Lock, name: &str, version: Option<&str>) -> Vec<String> {
    let mut parents: HashMap<&str, Vec<&str>> = HashMap::new();
    for (id, package) in &lock.packages {
        for child in package.deps.values().chain(package.optional_deps.values()) {
            parents.entry(child).or_default().push(id);
        }
    }

    let targets: Vec<Steps> = lock
        .packages
        .iter()
        .filter(|(_, package)| {
            package.name == name && version.is_none_or(|version| package.version == version)
        })
        .map(|(id, _)| steps_towards(&parents, id))
        .collect();

    let mut routes = Vec::new();
    for (path, importer) in &lock.importers {
        let groups = [
            ("dependencies", &importer.dependencies),
            ("devDependencies", &importer.dev_dependencies),
            ("optionalDependencies", &importer.optional_dependencies),
        ];
        for (group, roots) in groups {
            for steps in &targets {
                let nearest = roots
                    .values()
                    .filter_map(|root| Some((steps.get(root.as_str())?.0, root)))
                    .min();
                let Some((_, root)) = nearest else {
                    continue;
                };

                let mut chain = vec![root.as_str()];
                let mut at = root.as_str();
                while let Some(step) = steps[at].1 {
                    chain.push(step);
                    at = step;
                }
                routes.push(format!("{path} ({group}) → {}", chain.join(" → ")));
            }
        }
    }
    routes
}

/// How far each package is from one instance, and which package to go to next to reach it.
type Steps<'l> = HashMap<&'l str, (usize, Option<&'l str>)>;

fn steps_towards<'l>(parents: &HashMap<&'l str, Vec<&'l str>>, target: &'l str) -> Steps<'l> {
    let mut steps = HashMap::from([(target, (0, None))]);
    let mut queue = VecDeque::from([target]);
    while let Some(id) = queue.pop_front() {
        let distance = steps[id].0 + 1;
        for parent in parents.get(id).into_iter().flatten() {
            if !steps.contains_key(parent) {
                steps.insert(parent, (distance, Some(id)));
                queue.push_back(parent);
            }
        }
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn lock() -> Lock {
        serde_json::from_value(json!({
            "version": 1,
            "packages": {
                "react@18.3.1": { "name": "react", "version": "18.3.1", "url": "u", "integrity": "sha512-i" },
                "react-dom@18.3.1": {
                    "name": "react-dom", "version": "18.3.1", "url": "u", "integrity": "sha512-i",
                    "deps": { "react": "react@18.3.1" },
                },
                "react@17.0.2": { "name": "react", "version": "17.0.2", "url": "u", "integrity": "sha512-i" },
                "legacy@1.0.0": {
                    "name": "legacy", "version": "1.0.0", "url": "u", "integrity": "sha512-i",
                    "deps": { "react": "react@17.0.2", "loop": "loop@1.0.0" },
                },
                "loop@1.0.0": {
                    "name": "loop", "version": "1.0.0", "url": "u", "integrity": "sha512-i",
                    "deps": { "legacy": "legacy@1.0.0" },
                },
                "unused@1.0.0": { "name": "unused", "version": "1.0.0", "url": "u", "integrity": "sha512-i" },
            },
            "sccs": [],
            "importers": {
                ".": {
                    "dependencies": { "react-dom": "react-dom@18.3.1" },
                    "devDependencies": { "react": "react@18.3.1" },
                },
                "apps/old": { "dependencies": { "legacy": "legacy@1.0.0" } },
            },
        }))
        .unwrap()
    }

    #[test]
    fn reports_a_route_for_each_importer_and_group() {
        assert_eq!(
            routes(&lock(), "react", None),
            [
                ". (dependencies) → react-dom@18.3.1 → react@18.3.1",
                ". (devDependencies) → react@18.3.1",
                "apps/old (dependencies) → legacy@1.0.0 → react@17.0.2",
            ]
        );
    }

    #[test]
    fn reports_only_the_instances_a_version_matches() {
        assert_eq!(
            routes(&lock(), "react", Some("17.0.2")),
            ["apps/old (dependencies) → legacy@1.0.0 → react@17.0.2"]
        );
        assert!(routes(&lock(), "react", Some("16.0.0")).is_empty());
    }

    #[test]
    fn walks_out_of_cycles() {
        assert_eq!(
            routes(&lock(), "loop", None),
            ["apps/old (dependencies) → legacy@1.0.0 → loop@1.0.0"]
        );
    }

    #[test]
    fn reports_nothing_for_a_package_no_importer_reaches() {
        assert!(routes(&lock(), "unused", None).is_empty());
        assert!(routes(&lock(), "absent", None).is_empty());
    }
}
