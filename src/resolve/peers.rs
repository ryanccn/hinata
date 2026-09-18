// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Peers are resolved from ancestors, and a package gets one instance per distinct set of resolved
//! peers.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use eyre::{Result, eyre};
use node_semver::Version;

use crate::lock;

use super::{Groups, Node};

/// Peer names a node's subtree may resolve from outside it. Over-approximating only reduces sharing.
fn peer_closures(nodes: &BTreeMap<String, Node>) -> HashMap<String, BTreeSet<String>> {
    let mut closures: HashMap<String, BTreeSet<String>> = nodes
        .iter()
        .map(|(key, node)| (key.clone(), node.peers.keys().cloned().collect()))
        .collect();
    loop {
        let mut changed = false;
        for (key, node) in nodes {
            let mut additions = Vec::new();
            let children = node
                .deps
                .values()
                .chain(node.optional_deps.values())
                .chain(node.auto_peers.values());
            for child in children {
                for name in &closures[child] {
                    let provided =
                        node.deps.contains_key(name) || node.optional_deps.contains_key(name);
                    if !provided && !closures[key].contains(name) {
                        additions.push(name.clone());
                    }
                }
            }
            if !additions.is_empty() {
                changed = true;
                closures
                    .get_mut(key)
                    .expect("every node has a closure")
                    .extend(additions);
            }
        }
        if !changed {
            return closures;
        }
    }
}

/// Optional peers that no ancestor provides fall back to the highest version anywhere in the graph.
fn optional_peer_fallbacks(nodes: &BTreeMap<String, Node>) -> HashMap<String, String> {
    let optional: BTreeSet<&str> = nodes
        .values()
        .flat_map(|node| {
            node.peers
                .iter()
                .filter(|(_, optional)| **optional)
                .map(|(name, _)| name.as_str())
        })
        .collect();
    let mut best: HashMap<String, (Version, String)> = HashMap::new();
    for (key, node) in nodes {
        let Ok(version) = Version::parse(&node.version) else {
            continue;
        };
        if optional.contains(node.name.as_str())
            && best
                .get(&node.name)
                .is_none_or(|(current, _)| version > *current)
        {
            best.insert(node.name.clone(), (version, key.clone()));
        }
    }
    best.into_iter()
        .map(|(name, (_, key))| (name, key))
        .collect()
}

struct Frame {
    names: BTreeMap<String, String>,
    parent: Option<usize>,
}

#[derive(Clone)]
struct Instance {
    id: String,
    externals: BTreeSet<String>,
}

impl Instance {
    fn placeholder(key: &str) -> Instance {
        Instance {
            id: key.to_string(),
            externals: BTreeSet::new(),
        }
    }
}

struct InstanceData {
    key: String,
    deps: BTreeMap<String, String>,
    optional_deps: BTreeMap<String, String>,
}

type MemoKey = (String, Vec<Option<String>>);

struct PeerResolver<'n> {
    nodes: &'n BTreeMap<String, Node>,
    closures: HashMap<String, BTreeSet<String>>,
    fallbacks: HashMap<String, String>,
    frames: Vec<Frame>,
    root_frame: usize,
    memo: HashMap<MemoKey, Instance>,
    in_progress: HashSet<MemoKey>,
    visiting: HashSet<(String, usize)>,
    instances: BTreeMap<String, InstanceData>,
}

impl PeerResolver<'_> {
    fn lookup(&self, mut frame: usize, name: &str) -> Option<(String, usize)> {
        loop {
            if let Some(key) = self.frames[frame].names.get(name) {
                return Some((key.clone(), frame));
            }
            frame = self.frames[frame].parent?;
        }
    }

    fn instantiate_root(&mut self, groups: &Groups) -> Result<Groups> {
        // Regular dependencies take precedence over dev and optional ones.
        let mut names = BTreeMap::new();
        for group in [1, 2, 0] {
            names.extend(groups[group].clone());
        }
        self.frames.push(Frame {
            names,
            parent: None,
        });
        self.root_frame = self.frames.len() - 1;
        let mut ids = Groups::default();
        for (group, deps) in groups.iter().enumerate() {
            for (alias, key) in deps {
                ids[group].insert(alias.clone(), self.instantiate(key, self.root_frame)?.id);
            }
        }
        Ok(ids)
    }

    fn instantiate(&mut self, key: &str, frame: usize) -> Result<Instance> {
        let visit = (key.to_string(), frame);
        if !self.visiting.insert(visit.clone()) {
            return Ok(Instance::placeholder(key));
        }
        let instance = self.instantiate_uncached(key, frame);
        self.visiting.remove(&visit);
        instance
    }

    fn instantiate_uncached(&mut self, key: &str, frame: usize) -> Result<Instance> {
        let nodes = self.nodes;
        let node = &nodes[key];
        let closure = self.closures[key].clone();

        let mut visible = BTreeMap::new();
        let mut fell_back = BTreeSet::new();
        let root_frame = self.root_frame;
        for name in &closure {
            let found = self.lookup(frame, name).or_else(|| {
                let fallback = self
                    .fallbacks
                    .get(name)
                    .filter(|fallback| fallback.as_str() != key)?;
                fell_back.insert(name.clone());
                Some((fallback.clone(), root_frame))
            });
            if let Some((peer_key, peer_frame)) = found {
                visible.insert(name.clone(), self.instantiate(&peer_key, peer_frame)?.id);
            }
        }
        // Required peers are only taken from ancestors.
        let resolved_peer = |name: &String, optional: bool| {
            visible.contains_key(name) && (optional || !fell_back.contains(name))
        };
        let memo_key: MemoKey = (
            key.to_string(),
            closure
                .iter()
                .map(|name| visible.get(name).cloned())
                .collect(),
        );
        if let Some(instance) = self.memo.get(&memo_key) {
            return Ok(instance.clone());
        }
        if !self.in_progress.insert(memo_key.clone()) {
            return Ok(Instance::placeholder(key));
        }

        let mut children = node.deps.clone();
        children.extend(node.optional_deps.clone());
        for (name, &optional) in &node.peers {
            if !optional
                && !resolved_peer(name, optional)
                && let Some(auto) = node.auto_peers.get(name)
            {
                children.insert(name.clone(), auto.clone());
            }
        }
        self.frames.push(Frame {
            names: children.clone(),
            parent: Some(frame),
        });
        let child_frame = self.frames.len() - 1;

        let mut externals: BTreeSet<String> = node
            .peers
            .iter()
            .filter(|(name, optional)| resolved_peer(name, **optional))
            .map(|(name, _)| name.clone())
            .collect();
        let mut deps = BTreeMap::new();
        let mut optional_deps = BTreeMap::new();
        for (alias, child_key) in &children {
            let child = self.instantiate(child_key, child_frame)?;
            externals.extend(
                child
                    .externals
                    .into_iter()
                    .filter(|name| !children.contains_key(name) && visible.contains_key(name)),
            );
            let edges = if node.optional_deps.contains_key(alias) {
                &mut optional_deps
            } else {
                &mut deps
            };
            edges.insert(alias.clone(), child.id);
        }
        // Node resolves from the package's store path, so peers must be linked beside it.
        for (name, &optional) in &node.peers {
            if resolved_peer(name, optional) {
                deps.insert(name.clone(), visible[name].clone());
            }
        }

        let mut id = key.to_string();
        for name in &externals {
            let _ = write!(id, "({})", visible[name]);
        }
        self.instances.entry(id.clone()).or_insert(InstanceData {
            key: key.to_string(),
            deps,
            optional_deps,
        });

        let instance = Instance { id, externals };
        self.in_progress.remove(&memo_key);
        self.memo.insert(memo_key, instance.clone());
        Ok(instance)
    }
}

pub fn build_packages(
    nodes: &BTreeMap<String, Node>,
    roots: &[Groups],
) -> Result<(BTreeMap<String, lock::Package>, Vec<Groups>)> {
    let mut resolver = PeerResolver {
        nodes,
        closures: peer_closures(nodes),
        fallbacks: optional_peer_fallbacks(nodes),
        frames: Vec::new(),
        root_frame: 0,
        memo: HashMap::new(),
        in_progress: HashSet::new(),
        visiting: HashSet::new(),
        instances: BTreeMap::new(),
    };

    let mut root_ids = roots
        .iter()
        .map(|groups| resolver.instantiate_root(groups))
        .collect::<Result<Vec<_>>>()?;

    // Instances still being computed within a cycle are referenced by their node key.
    let known: HashSet<String> = resolver.instances.keys().cloned().collect();
    let mut first_instance: HashMap<String, String> = HashMap::new();
    for (id, instance) in &resolver.instances {
        first_instance
            .entry(instance.key.clone())
            .or_insert_with(|| id.clone());
    }
    let settle = |id: &mut String| {
        if !known.contains(id)
            && let Some(real) = first_instance.get(id)
        {
            *id = real.clone();
        }
    };
    for deps in root_ids.iter_mut().flatten() {
        deps.values_mut().for_each(settle);
    }

    let mut packages = BTreeMap::new();
    for (id, mut instance) in resolver.instances {
        instance
            .deps
            .values_mut()
            .chain(instance.optional_deps.values_mut())
            .for_each(settle);
        let node = &nodes[&instance.key];
        let manifest = &node.manifest;
        let integrity = manifest
            .dist
            .integrity
            .clone()
            .ok_or_else(|| eyre!("{id} has no integrity hash in the registry"))?;
        packages.insert(
            id,
            lock::Package {
                name: node.name.clone(),
                version: node.version.clone(),
                url: manifest.dist.tarball.clone(),
                integrity,
                deps: instance.deps,
                optional_deps: instance.optional_deps,
                os: manifest.os.clone(),
                cpu: manifest.cpu.clone(),
                libc: manifest.libc.clone(),
                has_bin: manifest.has_bin(),
                install_script: manifest.has_install_script,
                build: false,
                impure_build: false,
                build_inputs: Vec::new(),
                patch: None,
            },
        );
    }

    Ok((packages, root_ids))
}
