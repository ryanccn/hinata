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
