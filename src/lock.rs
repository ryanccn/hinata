// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{BTreeMap, HashMap};

use petgraph::{algo::tarjan_scc, graph::DiGraph};
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct Lock {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nixpkgs: Option<Nixpkgs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<Node>,
    /// The overrides these packages were resolved with, so that changing one resolves again.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub overrides: BTreeMap<String, String>,
    pub packages: BTreeMap<String, Package>,
    /// Nix derivations cannot depend on each other cyclically, so each of these groups is built as one.
    pub sccs: Vec<Vec<String>>,
    pub importers: BTreeMap<String, Importer>,
}

#[expect(clippy::struct_excessive_bools)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Package {
    pub name: String,
    pub version: String,
    pub url: String,
    pub integrity: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub deps: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub optional_deps: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub libc: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub has_bin: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub install_script: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub build: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub impure_build: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub build_inputs: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<Patch>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Nixpkgs {
    /// The flake reference this was locked from.
    pub from: String,
    /// Attributes for `builtins.fetchTree`.
    pub locked: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    /// The version range this was locked for.
    pub from: String,
    /// The Nixpkgs attribute that provides it.
    pub attr: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Patch {
    /// Relative to the lockfile.
    pub path: String,
    /// Hexadecimal SHA-256 of the patch file.
    pub hash: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Importer {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dev_dependencies: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub optional_dependencies: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub links: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Specifiers::is_empty")]
    pub specifiers: Specifiers,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Specifiers {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub dev_dependencies: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub optional_dependencies: BTreeMap<String, String>,
}

impl Specifiers {
    pub fn is_empty(&self) -> bool {
        self.groups().into_iter().all(BTreeMap::is_empty)
    }

    pub fn groups(&self) -> [&BTreeMap<String, String>; 3] {
        [
            &self.dependencies,
            &self.dev_dependencies,
            &self.optional_dependencies,
        ]
    }

    pub fn groups_mut(&mut self) -> [&mut BTreeMap<String, String>; 3] {
        [
            &mut self.dependencies,
            &mut self.dev_dependencies,
            &mut self.optional_dependencies,
        ]
    }
}

#[expect(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !value
}

pub fn to_json(lock: &Lock) -> eyre::Result<String> {
    let mut json = serde_json::to_string_pretty(lock)?;
    json.push('\n');
    Ok(json)
}

/// Names and aliases end up in paths and build scripts, so ones that npm would not accept are
/// rejected, as are integrity hashes weaker than SHA-256.
pub fn validate(lock: &Lock) -> eyre::Result<()> {
    for (id, package) in &lock.packages {
        if !is_valid_name(&package.name) {
            eyre::bail!(
                "{id} is named {:?}, which is not a valid package name",
                package.name
            );
        }
        if let Some(name) = package
            .deps
            .keys()
            .chain(package.optional_deps.keys())
            .find(|name| !is_valid_name(name))
        {
            eyre::bail!("{id} depends on {name:?}, which is not a valid package name");
        }

        if !["sha512-", "sha256-"]
            .iter()
            .any(|algorithm| package.integrity.starts_with(algorithm))
        {
            eyre::bail!("{id} has no SHA-512 or SHA-256 integrity hash");
        }
    }

    for (path, importer) in &lock.importers {
        if let Some(name) = importer
            .dependencies
            .keys()
            .chain(importer.dev_dependencies.keys())
            .chain(importer.optional_dependencies.keys())
            .chain(importer.links.keys())
            .find(|name| !is_valid_name(name))
        {
            eyre::bail!("importer {path} depends on {name:?}, which is not a valid package name");
        }
    }

    Ok(())
}

/// Includes the uppercase letters that npm still allows in old package names.
pub fn is_valid_name(name: &str) -> bool {
    let segment = |segment: &str| {
        !segment.is_empty()
            && !segment.starts_with(['.', '_'])
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._~".contains(&byte))
    };

    name.len() <= 214
        && match name.strip_prefix('@') {
            Some(scoped) => scoped
                .split_once('/')
                .is_some_and(|(scope, name)| segment(scope) && segment(name)),
            None => segment(name),
        }
}

pub fn find_cycles(packages: &BTreeMap<String, Package>) -> Vec<Vec<String>> {
    let mut graph = DiGraph::<&str, ()>::new();
    let nodes: HashMap<&str, _> = packages
        .keys()
        .map(|id| (id.as_str(), graph.add_node(id.as_str())))
        .collect();
    for (id, package) in packages {
        for dep in package.deps.values().chain(package.optional_deps.values()) {
            if let Some(&to) = nodes.get(dep.as_str()) {
                graph.add_edge(nodes[id.as_str()], to, ());
            }
        }
    }

    let mut cycles: Vec<Vec<String>> = tarjan_scc(&graph)
        .into_iter()
        .filter(|component| component.len() > 1)
        .map(|component| {
            let mut ids: Vec<String> = component
                .into_iter()
                .map(|n| graph[n].to_string())
                .collect();
            ids.sort();
            ids
        })
        .collect();
    cycles.sort();
    cycles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_npm_package_names() {
        for name in [
            "react",
            "@babel/core",
            "JSONStream",
            "lodash.merge",
            "a-b_c~d",
        ] {
            assert!(is_valid_name(name), "{name}");
        }
        for name in [
            "",
            "..",
            "../x",
            ".bin",
            "_private",
            "@scope",
            "@scope/",
            "@scope/a/b",
            "a b",
            "a\"$(id)",
        ] {
            assert!(!is_valid_name(name), "{name}");
        }
    }

    #[test]
    fn rejects_unsafe_aliases_and_weak_integrity() {
        let lock = |alias: &str, integrity: &str| -> Lock {
            serde_json::from_value(serde_json::json!({
                "version": VERSION,
                "packages": {
                    "a@1.0.0": {
                        "name": "a",
                        "version": "1.0.0",
                        "url": "https://registry.test/a.tgz",
                        "integrity": integrity,
                        "deps": { alias: "b@1.0.0" },
                    },
                },
                "sccs": [],
                "importers": { ".": { "dependencies": { "a": "a@1.0.0" } } },
            }))
            .unwrap()
        };

        assert!(validate(&lock("b", "sha512-x")).is_ok());
        assert!(validate(&lock("../b", "sha512-x")).is_err());
        assert!(validate(&lock("b", "sha1-x")).is_err());
    }
}
