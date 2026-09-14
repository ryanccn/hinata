// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Versions are chosen breadth-first, reusing an already chosen version wherever it satisfies a
//! range. Peers are then resolved from ancestors, and a package gets one instance per distinct set
//! of resolved peers.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use eyre::{Result, WrapErr, bail, eyre};
use log::{debug, warn};
use node_semver::{Range, Version};
use owo_colors::colors::Yellow;

use crate::lock::{self, Lock, Specifiers};
use crate::logging::{LogDisplay as _, plural};
use crate::registry::{Packument, Registry, VersionManifest};

const WORKSPACE: &str = "workspace:";

pub struct Project {
    pub name: Option<String>,
    pub version: Option<String>,
    pub specifiers: Specifiers,
}

type Groups = [BTreeMap<String, String>; 3];

type Workspace<'p> = HashMap<&'p str, (&'p str, &'p Project)>;

/// Projects are keyed by their `/`-separated path relative to the workspace root, which is `.`.
pub fn resolve(
    projects: &BTreeMap<String, Project>,
    registry: &dyn Registry,
    preferred: &[(String, String)],
) -> Result<Lock> {
    let workspace = workspace_names(projects)?;
    let mut registry_specifiers = Vec::new();
    let mut links = Vec::new();
    for (path, project) in projects {
        let (specifiers, project_links) =
            split_workspace_links(&workspace, path, &project.specifiers)
                .wrap_err_with(|| format!("in {path}"))?;
        registry_specifiers.push(specifiers);
        links.push(project_links);
    }

    let mut chooser = Chooser::new(registry, preferred);
    let groups = chooser.choose_all(&registry_specifiers)?;
    let (packages, root_ids) = build_packages(&chooser.nodes, &groups)?;
    let importers = projects
        .iter()
        .zip(links)
        .zip(root_ids)
        .map(|(((path, project), links), ids)| {
            let [dependencies, dev_dependencies, optional_dependencies] = ids;
            let importer = lock::Importer {
                dependencies,
                dev_dependencies,
                optional_dependencies,
                links,
                specifiers: project.specifiers.clone(),
            };
            (path.clone(), importer)
        })
        .collect();
    Ok(Lock {
        version: lock::VERSION,
        sccs: lock::find_cycles(&packages),
        packages,
        importers,
    })
}

fn workspace_names(projects: &BTreeMap<String, Project>) -> Result<Workspace<'_>> {
    let mut workspace = Workspace::new();
    for (path, project) in projects {
        if let Some(name) = &project.name
            && let Some((other, _)) = workspace.insert(name.as_str(), (path.as_str(), project))
        {
            bail!("{other} and {path} are both named {name}");
        }
    }
    Ok(workspace)
}

fn split_workspace_links(
    workspace: &Workspace<'_>,
    from: &str,
    specifiers: &Specifiers,
) -> Result<(Specifiers, BTreeMap<String, String>)> {
    let mut registry_specifiers = Specifiers::default();
    let mut links = BTreeMap::new();
    let groups = specifiers
        .groups()
        .into_iter()
        .zip(registry_specifiers.groups_mut());
    for (deps, registry_deps) in groups {
        for (alias, spec) in deps {
            match spec.strip_prefix(WORKSPACE) {
                Some(spec) => {
                    links.insert(alias.clone(), workspace_link(workspace, from, alias, spec)?);
                }
                None => {
                    registry_deps.insert(alias.clone(), spec.clone());
                }
            }
        }
    }
    Ok((registry_specifiers, links))
}

fn workspace_link(
    workspace: &Workspace<'_>,
    from: &str,
    alias: &str,
    spec: &str,
) -> Result<String> {
    let (name, range) = split_version(spec).unwrap_or((alias, spec));
    let (to, project) = workspace
        .get(name)
        .ok_or_else(|| eyre!("{alias}: no workspace package is named {name}"))?;
    if !matches!(range, "" | "*" | "^" | "~") {
        let parsed = Range::parse(range)
            .map_err(|error| eyre!("{alias}: {range} is not a valid range: {error}"))?;
        let version = project
            .version
            .as_deref()
            .and_then(|version| Version::parse(version).ok());
        if !version.is_some_and(|version| parsed.satisfies(&version)) {
            bail!("{alias}: the workspace package {name} in {to} does not match {range}");
        }
    }
    Ok(relative_path(from, to))
}

fn relative_path(from: &str, to: &str) -> String {
    fn components(path: &str) -> Vec<&str> {
        path.split('/')
            .filter(|component| !component.is_empty() && *component != ".")
            .collect()
    }
    let (from, to) = (components(from), components(to));
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let parts: Vec<&str> = std::iter::repeat_n("..", from.len() - common)
        .chain(to[common..].iter().copied())
        .collect();
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

struct Node {
    name: String,
    version: String,
    manifest: VersionManifest,
    deps: BTreeMap<String, String>,
    optional_deps: BTreeMap<String, String>,
    /// Peer name to whether it is optional.
    peers: BTreeMap<String, bool>,
    auto_peers: BTreeMap<String, String>,
}

struct Fetched {
    packument: Packument,
    versions: BTreeMap<Version, String>,
    /// Cached packuments may predate versions and tags published since.
    verified: bool,
}

impl Fetched {
    fn new(packument: Packument, verified: bool) -> Self {
        let versions = packument
            .versions
            .keys()
            .filter_map(|raw| Some((Version::parse(raw).ok()?, raw.clone())))
            .collect();
        Self {
            packument,
            versions,
            verified,
        }
    }

    fn has_pinned_version(&self, range: &str, chosen: Option<&BTreeSet<Version>>) -> bool {
        !self.packument.dist_tags.contains_key(range)
            && pinned_version(range, chosen)
                .is_some_and(|version| self.versions.contains_key(&version))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    Dependency,
    Optional,
    Peer,
}

enum Origin {
    Importer { index: usize, group: usize },
    Node(String),
}

struct Request {
    from: Origin,
    edge: Edge,
    alias: String,
    name: String,
    range: String,
}

impl Request {
    fn new(from: Origin, edge: Edge, alias: &str, spec: &str) -> Result<Option<Request>> {
        match parse_spec(alias, spec) {
            Ok((name, range)) => Ok(Some(Request {
                from,
                edge,
                alias: alias.to_string(),
                name,
                range,
            })),
            Err(error) if edge == Edge::Dependency => Err(error),
            Err(error) => {
                warn!("skipping {}: {error}", alias.log_display::<Yellow>());
                Ok(None)
            }
        }
    }
}

struct Chooser<'r> {
    registry: &'r dyn Registry,
    packuments: HashMap<String, Fetched>,
    chosen: HashMap<String, BTreeSet<Version>>,
    nodes: BTreeMap<String, Node>,
}

impl<'r> Chooser<'r> {
    fn new(registry: &'r dyn Registry, preferred: &[(String, String)]) -> Self {
        let mut chosen: HashMap<String, BTreeSet<Version>> = HashMap::new();
        for (name, version) in preferred {
            if let Ok(version) = Version::parse(version) {
                chosen.entry(name.clone()).or_default().insert(version);
            }
        }
        Chooser {
            registry,
            packuments: HashMap::new(),
            chosen,
            nodes: BTreeMap::new(),
        }
    }

    fn choose_all(&mut self, importers: &[Specifiers]) -> Result<Vec<Groups>> {
        let edges = [Edge::Dependency, Edge::Dependency, Edge::Optional];
        let mut requests = Vec::new();
        for (index, specifiers) in importers.iter().enumerate() {
            for (group, (deps, edge)) in specifiers.groups().into_iter().zip(edges).enumerate() {
                for (alias, spec) in deps {
                    let from = Origin::Importer { index, group };
                    requests.extend(Request::new(from, edge, alias, spec)?);
                }
            }
        }

        let mut groups: Vec<Groups> = importers.iter().map(|_| Groups::default()).collect();
        while !requests.is_empty() {
            self.fetch(&requests)?;
            let mut added = Vec::new();
            for request in requests {
                let Some(key) = self.choose(&request)? else {
                    continue;
                };
                if !self.nodes.contains_key(&key) {
                    self.add_node(&key)?;
                    added.push(key.clone());
                }
                match request.from {
                    Origin::Importer { index, group } => {
                        groups[index][group].insert(request.alias, key);
                    }
                    Origin::Node(from) => {
                        let node = self
                            .nodes
                            .get_mut(&from)
                            .expect("requests come from known nodes");
                        let edges = match request.edge {
                            Edge::Dependency => &mut node.deps,
                            Edge::Optional => &mut node.optional_deps,
                            Edge::Peer => &mut node.auto_peers,
                        };
                        edges.insert(request.alias, key);
                    }
                }
            }
            requests = added
                .iter()
                .map(|key| self.requests_for(key))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect();
        }
        Ok(groups)
    }

    fn fetch(&mut self, requests: &[Request]) -> Result<()> {
        let cacheable = unique_names(requests.iter().filter(|request| {
            !self.packuments.contains_key(&request.name)
                && pinned_version(&request.range, self.chosen.get(&request.name)).is_some()
        }));
        for (name, packument) in cacheable.iter().zip(self.registry.cached(&cacheable)) {
            if let Some(packument) = packument {
                self.packuments
                    .insert(name.clone(), Fetched::new(packument, false));
            }
        }

        let names = unique_names(requests.iter().filter(|request| {
            self.packuments.get(&request.name).is_none_or(|fetched| {
                !fetched.verified
                    && !fetched.has_pinned_version(&request.range, self.chosen.get(&request.name))
            })
        }));
        if names.is_empty() {
            return Ok(());
        }
        debug!(
            "fetching metadata for {} from the registry",
            plural(names.len(), "package", "packages")
        );
        for (name, packument) in names.iter().zip(self.registry.fetch(&names)?) {
            self.packuments
                .insert(name.clone(), Fetched::new(packument, true));
        }
        Ok(())
    }

    fn choose(&mut self, request: &Request) -> Result<Option<String>> {
        let fetched = &self.packuments[&request.name];
        match pick_version(fetched, &request.range, self.chosen.get(&request.name)) {
            Some((version, raw)) => {
                self.chosen
                    .entry(request.name.clone())
                    .or_default()
                    .insert(version);
                Ok(Some(format!("{}@{raw}", request.name)))
            }
            None if request.edge == Edge::Dependency => {
                bail!("no version of {} matches {}", request.name, request.range)
            }
            None => {
                warn!(
                    "skipping {} {}, since no version matches",
                    if request.edge == Edge::Peer {
                        "peer dependency"
                    } else {
                        "optional dependency"
                    },
                    format_args!("{}@{}", request.name, request.range).log_display::<Yellow>()
                );
                Ok(None)
            }
        }
    }

    fn add_node(&mut self, key: &str) -> Result<()> {
        let (name, version) = key.rsplit_once('@').expect("node keys are name@version");
        let raw = &self.packuments[name].packument.versions[version];
        let manifest: VersionManifest = serde_json::from_str(raw.get())
            .wrap_err_with(|| format!("reading the registry metadata of {key}"))?;
        let peers = manifest
            .peer_dependencies
            .keys()
            .filter(|peer| {
                !manifest.dependencies.contains_key(*peer)
                    && !manifest.optional_dependencies.contains_key(*peer)
            })
            .map(|peer| {
                let optional = manifest
                    .peer_dependencies_meta
                    .get(peer)
                    .is_some_and(|meta| meta.optional);
                (peer.clone(), optional)
            })
            .collect();
        self.nodes.insert(
            key.to_string(),
            Node {
                name: name.to_string(),
                version: version.to_string(),
                manifest,
                deps: BTreeMap::new(),
                optional_deps: BTreeMap::new(),
                peers,
                auto_peers: BTreeMap::new(),
            },
        );
        Ok(())
    }

    fn requests_for(&self, key: &str) -> Result<Vec<Request>> {
        let node = &self.nodes[key];
        let manifest = &node.manifest;
        let deps = manifest
            .dependencies
            .iter()
            .filter(|(alias, _)| !manifest.optional_dependencies.contains_key(*alias))
            .map(|(alias, spec)| (Edge::Dependency, alias, spec));
        let optional = manifest
            .optional_dependencies
            .iter()
            .map(|(alias, spec)| (Edge::Optional, alias, spec));
        let peers = node
            .peers
            .iter()
            .filter(|(_, optional)| !**optional)
            .map(|(name, _)| (Edge::Peer, name, &manifest.peer_dependencies[name]));

        let mut requests = Vec::new();
        for (edge, alias, spec) in deps.chain(optional).chain(peers) {
            requests.extend(
                Request::new(Origin::Node(key.to_string()), edge, alias, spec)
                    .wrap_err_with(|| format!("in {key}"))?,
            );
        }
        Ok(requests)
    }
}

pub(crate) fn parse_spec(alias: &str, spec: &str) -> Result<(String, String)> {
    let (name, range) = match spec.strip_prefix("npm:") {
        Some(target) => split_version(target).unwrap_or((target, "")),
        None => (alias, spec),
    };
    let range = range.trim();
    if range.contains(':') || range.contains('/') {
        bail!("{alias}: {spec} is not supported yet; only registry versions, ranges and tags are");
    }
    let range = if range.is_empty() { "*" } else { range };
    Ok((name.to_string(), range.to_string()))
}

/// Splits `name@version`, where `name` may be scoped.
pub(crate) fn split_version(spec: &str) -> Option<(&str, &str)> {
    let at = spec.get(1..)?.find('@')? + 1;
    Some((&spec[..at], &spec[at + 1..]))
}

fn unique_names<'a>(requests: impl Iterator<Item = &'a Request>) -> Vec<String> {
    requests
        .map(|request| request.name.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The version [`pick_version`] picks for `range` from any packument that has it, since published
/// versions never change.
fn pinned_version(range: &str, chosen: Option<&BTreeSet<Version>>) -> Option<Version> {
    if let Ok(version) = Version::parse(range)
        && version.to_string() == range
    {
        return Some(version);
    }
    let range = Range::parse(range).ok()?;
    chosen?
        .iter()
        .rev()
        .find(|version| range.satisfies(version))
        .cloned()
}

fn pick_version(
    fetched: &Fetched,
    range: &str,
    chosen: Option<&BTreeSet<Version>>,
) -> Option<(Version, String)> {
    let available = |version: &Version| {
        fetched
            .versions
            .get_key_value(version)
            .map(|(version, raw)| (version.clone(), raw.clone()))
    };
    if let Some(tagged) = fetched.packument.dist_tags.get(range) {
        return available(&Version::parse(tagged).ok()?);
    }

    let range = Range::parse(range).ok()?;
    let latest = || {
        fetched
            .packument
            .dist_tags
            .get("latest")
            .and_then(|latest| Version::parse(latest).ok())
            .filter(|latest| range.satisfies(latest))
            .and_then(|latest| available(&latest))
    };
    let highest = || {
        fetched
            .versions
            .iter()
            .rev()
            .find(|(version, _)| range.satisfies(version))
            .map(|(version, raw)| (version.clone(), raw.clone()))
    };
    chosen
        .into_iter()
        .flatten()
        .rev()
        .filter(|version| range.satisfies(version))
        .find_map(available)
        .or_else(latest)
        .or_else(highest)
}

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

fn build_packages(
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
            },
        );
    }

    Ok((packages, root_ids))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::cell::RefCell;

    #[derive(Default)]
    struct MemoryRegistry {
        packuments: HashMap<String, Value>,
        cache: HashMap<String, Value>,
        fetched: RefCell<BTreeSet<String>>,
    }

    impl Registry for MemoryRegistry {
        fn fetch(&self, names: &[String]) -> Result<Vec<Packument>> {
            self.fetched.borrow_mut().extend(names.iter().cloned());
            names
                .iter()
                .map(|name| {
                    let packument = self
                        .packuments
                        .get(name)
                        .ok_or_else(|| eyre!("{name} is not in the registry"))?;
                    Ok(serde_json::from_str(&packument.to_string())?)
                })
                .collect()
        }

        fn cached(&self, names: &[String]) -> Vec<Option<Packument>> {
            names
                .iter()
                .map(|name| serde_json::from_str(&self.cache.get(name)?.to_string()).ok())
                .collect()
        }
    }

    /// The last version is tagged latest.
    fn packument(name: &str, versions: Vec<(&str, Value)>) -> Value {
        let latest = versions.last().map(|(version, _)| version.to_string());
        let versions: serde_json::Map<String, Value> = versions
            .into_iter()
            .map(|(version, mut manifest)| {
                manifest["dist"] = json!({
                    "tarball": format!("https://registry.test/{name}/-/{version}.tgz"),
                    "integrity": format!("sha512-{name}-{version}"),
                });
                (version.to_string(), manifest)
            })
            .collect();
        json!({ "dist-tags": { "latest": latest }, "versions": versions })
    }

    fn registry(packages: Vec<(&str, Vec<(&str, Value)>)>) -> MemoryRegistry {
        MemoryRegistry {
            packuments: packages
                .into_iter()
                .map(|(name, versions)| (name.to_string(), packument(name, versions)))
                .collect(),
            ..Default::default()
        }
    }

    fn resolve(
        specifiers: &Specifiers,
        registry: &dyn Registry,
        preferred: &[(String, String)],
    ) -> Result<Lock> {
        let project = Project {
            name: None,
            version: None,
            specifiers: specifiers.clone(),
        };
        super::resolve(
            &BTreeMap::from([(".".to_string(), project)]),
            registry,
            preferred,
        )
    }

    fn deps(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(alias, spec)| (alias.to_string(), spec.to_string()))
            .collect()
    }

    fn specifiers(dependencies: &[(&str, &str)]) -> Specifiers {
        Specifiers {
            dependencies: deps(dependencies),
            ..Default::default()
        }
    }

    fn react_registry() -> MemoryRegistry {
        registry(vec![
            ("react", vec![("17.0.2", json!({})), ("18.3.1", json!({}))]),
            (
                "react-dom",
                vec![(
                    "18.3.1",
                    json!({ "peerDependencies": { "react": "^18.3.1" } }),
                )],
            ),
            (
                "hooks",
                vec![("1.0.0", json!({ "peerDependencies": { "react": "*" } }))],
            ),
            (
                "legacy",
                vec![(
                    "1.0.0",
                    json!({ "dependencies": { "react": "^17", "hooks": "^1" } }),
                )],
            ),
            (
                "plugin",
                vec![("1.0.0", json!({ "dependencies": { "hooks": "^1" } }))],
            ),
        ])
    }

    #[test]
    fn picks_highest_matching_versions() {
        let registry = registry(vec![
            (
                "a",
                vec![
                    ("1.0.0", json!({ "dependencies": { "b": "^1" } })),
                    (
                        "1.2.0",
                        json!({ "dependencies": { "b": "^1" }, "bin": { "a": "cli.js" } }),
                    ),
                    ("2.0.0", json!({})),
                ],
            ),
            (
                "b",
                vec![
                    ("1.0.0", json!({})),
                    ("1.5.0", json!({ "hasInstallScript": true })),
                ],
            ),
        ]);
        let lock = resolve(&specifiers(&[("a", "^1")]), &registry, &[]).unwrap();

        assert_eq!(lock.importers["."].dependencies["a"], "a@1.2.0");
        let a = &lock.packages["a@1.2.0"];
        assert_eq!(a.deps["b"], "b@1.5.0");
        assert_eq!(a.url, "https://registry.test/a/-/1.2.0.tgz");
        assert_eq!(a.integrity, "sha512-a-1.2.0");
        assert!(a.has_bin);
        assert!(lock.packages["b@1.5.0"].install_script);
        assert_eq!(lock.packages.len(), 2);
    }

    #[test]
    fn reuses_chosen_versions() {
        let registry = registry(vec![
            (
                "a",
                vec![("1.0.0", json!({ "dependencies": { "c": "~1.0.0" } }))],
            ),
            (
                "b",
                vec![("1.0.0", json!({ "dependencies": { "c": "^1.0.0" } }))],
            ),
            (
                "c",
                vec![
                    ("1.0.0", json!({})),
                    ("1.1.0", json!({})),
                    ("2.0.0", json!({})),
                ],
            ),
        ]);
        let lock = resolve(&specifiers(&[("a", "^1"), ("b", "^1")]), &registry, &[]).unwrap();

        assert_eq!(lock.packages["b@1.0.0"].deps["c"], "c@1.0.0");
        assert!(!lock.packages.contains_key("c@1.1.0"));
    }

    #[test]
    fn prefers_previously_locked_versions() {
        let registry = registry(vec![(
            "c",
            vec![("1.0.0", json!({})), ("1.1.0", json!({}))],
        )]);
        let preferred = [("c".to_string(), "1.0.0".to_string())];

        let lock = resolve(&specifiers(&[("c", "^1")]), &registry, &preferred).unwrap();
        assert_eq!(lock.importers["."].dependencies["c"], "c@1.0.0");

        let lock = resolve(&specifiers(&[("c", "^1")]), &registry, &[]).unwrap();
        assert_eq!(lock.importers["."].dependencies["c"], "c@1.1.0");
    }

    #[test]
    fn uses_cached_metadata_only_when_it_cannot_change_the_result() {
        let mut registry = registry(vec![(
            "c",
            vec![("1.0.0", json!({})), ("1.1.0", json!({}))],
        )]);
        registry
            .cache
            .insert("c".to_string(), packument("c", vec![("1.0.0", json!({}))]));
        let preferred = |locked: Option<&str>| -> Vec<(String, String)> {
            locked
                .map(|version| ("c".to_string(), version.to_string()))
                .into_iter()
                .collect()
        };

        for (spec, locked, expected, fetched) in [
            ("^1", Some("1.0.0"), "c@1.0.0", false),
            ("1.0.0", None, "c@1.0.0", false),
            ("^1", Some("1.1.0"), "c@1.1.0", true),
            ("^1", None, "c@1.1.0", true),
            ("latest", Some("1.0.0"), "c@1.1.0", true),
        ] {
            registry.fetched.borrow_mut().clear();
            let lock = resolve(&specifiers(&[("c", spec)]), &registry, &preferred(locked)).unwrap();
            assert_eq!(
                lock.importers["."].dependencies["c"], expected,
                "{spec} {locked:?}"
            );
            assert_eq!(
                registry.fetched.borrow().contains("c"),
                fetched,
                "{spec} {locked:?}"
            );
        }
    }

    #[test]
    fn resolves_peers_from_siblings() {
        let lock = resolve(
            &specifiers(&[("react", "^18"), ("react-dom", "^18")]),
            &react_registry(),
            &[],
        )
        .unwrap();

        let id = &lock.importers["."].dependencies["react-dom"];
        assert_eq!(id, "react-dom@18.3.1(react@18.3.1)");
        assert_eq!(lock.packages[id].deps["react"], "react@18.3.1");
    }

    #[test]
    fn separates_instances_by_peer_context() {
        let lock = resolve(
            &specifiers(&[
                ("react", "^18"),
                ("hooks", "^1"),
                ("legacy", "^1"),
                ("plugin", "^1"),
            ]),
            &react_registry(),
            &[],
        )
        .unwrap();
        let root = &lock.importers["."];

        assert_eq!(root.dependencies["hooks"], "hooks@1.0.0(react@18.3.1)");
        assert_eq!(root.dependencies["legacy"], "legacy@1.0.0");
        assert_eq!(
            lock.packages["legacy@1.0.0"].deps["hooks"],
            "hooks@1.0.0(react@17.0.2)"
        );
        assert_eq!(root.dependencies["plugin"], "plugin@1.0.0(react@18.3.1)");
        assert_eq!(
            lock.packages["plugin@1.0.0(react@18.3.1)"].deps["hooks"],
            "hooks@1.0.0(react@18.3.1)"
        );
    }

    #[test]
    fn installs_missing_peers() {
        let lock = resolve(&specifiers(&[("react-dom", "^18")]), &react_registry(), &[]).unwrap();

        assert_eq!(
            lock.importers["."].dependencies["react-dom"],
            "react-dom@18.3.1"
        );
        assert_eq!(
            lock.packages["react-dom@18.3.1"].deps["react"],
            "react@18.3.1"
        );
    }

    #[test]
    fn satisfies_optional_peers_from_anywhere_in_the_graph() {
        let registry = registry(vec![
            (
                "chalk",
                vec![(
                    "4.1.2",
                    json!({ "dependencies": { "supports-color": "^7" } }),
                )],
            ),
            ("supports-color", vec![("7.2.0", json!({}))]),
            (
                "debug",
                vec![(
                    "4.4.3",
                    json!({
                        "peerDependencies": { "supports-color": "*" },
                        "peerDependenciesMeta": { "supports-color": { "optional": true } },
                    }),
                )],
            ),
            (
                "app",
                vec![("1.0.0", json!({ "dependencies": { "debug": "^4" } }))],
            ),
        ]);
        let lock = resolve(
            &specifiers(&[("chalk", "^4"), ("app", "^1")]),
            &registry,
            &[],
        )
        .unwrap();

        assert_eq!(
            lock.importers["."].dependencies["app"],
            "app@1.0.0(supports-color@7.2.0)"
        );
        assert_eq!(
            lock.packages["debug@4.4.3(supports-color@7.2.0)"].deps["supports-color"],
            "supports-color@7.2.0"
        );
    }

    #[test]
    fn follows_aliases_and_tags_and_skips_unsatisfiable_optionals() {
        let registry = registry(vec![
            (
                "strip-ansi",
                vec![("6.0.1", json!({})), ("7.1.0", json!({}))],
            ),
            ("@scope/pkg", vec![("1.0.0", json!({}))]),
            ("gone", vec![("1.0.0", json!({}))]),
        ]);
        let specifiers = Specifiers {
            dependencies: deps(&[
                ("strip", "npm:strip-ansi@^6"),
                ("scoped", "npm:@scope/pkg"),
                ("strip-ansi", "latest"),
            ]),
            optional_dependencies: deps(&[("gone", "^9")]),
            ..Default::default()
        };
        let lock = resolve(&specifiers, &registry, &[]).unwrap();
        let root = &lock.importers["."];

        assert_eq!(root.dependencies["strip"], "strip-ansi@6.0.1");
        assert_eq!(root.dependencies["scoped"], "@scope/pkg@1.0.0");
        assert_eq!(root.dependencies["strip-ansi"], "strip-ansi@7.1.0");
        assert!(root.optional_dependencies.is_empty());
        assert_eq!(lock.importers["."].specifiers, specifiers);
    }

    #[test]
    fn handles_cycles() {
        let registry = registry(vec![
            (
                "a",
                vec![("1.0.0", json!({ "dependencies": { "b": "^1" } }))],
            ),
            (
                "b",
                vec![("1.0.0", json!({ "dependencies": { "a": "^1" } }))],
            ),
        ]);
        let lock = resolve(&specifiers(&[("a", "^1")]), &registry, &[]).unwrap();

        assert_eq!(lock.packages["b@1.0.0"].deps["a"], "a@1.0.0");
        assert_eq!(
            lock.sccs,
            vec![vec!["a@1.0.0".to_string(), "b@1.0.0".to_string()]]
        );
    }

    fn project(name: &str, version: &str, dependencies: &[(&str, &str)]) -> Project {
        Project {
            name: Some(name.to_string()),
            version: Some(version.to_string()),
            specifiers: specifiers(dependencies),
        }
    }

    #[test]
    fn resolves_workspace_importers_and_links() {
        let projects = BTreeMap::from([
            (
                ".".to_string(),
                project(
                    "root",
                    "0.0.0",
                    &[("react", "^18"), ("hooks", "^1"), ("shared", "workspace:^")],
                ),
            ),
            (
                "apps/legacy".to_string(),
                project(
                    "legacy-app",
                    "1.0.0",
                    &[
                        ("react", "^17"),
                        ("hooks", "^1"),
                        ("common", "workspace:shared@^1.2.0"),
                    ],
                ),
            ),
            (
                "packages/shared".to_string(),
                project("shared", "1.2.3", &[("react-dom", "^18")]),
            ),
        ]);
        let lock = super::resolve(&projects, &react_registry(), &[]).unwrap();

        let root = &lock.importers["."];
        assert_eq!(root.dependencies["hooks"], "hooks@1.0.0(react@18.3.1)");
        assert_eq!(root.links["shared"], "packages/shared");
        assert!(!root.dependencies.contains_key("shared"));
        assert_eq!(root.specifiers.dependencies["shared"], "workspace:^");

        let legacy = &lock.importers["apps/legacy"];
        assert_eq!(legacy.dependencies["react"], "react@17.0.2");
        assert_eq!(legacy.dependencies["hooks"], "hooks@1.0.0(react@17.0.2)");
        assert_eq!(legacy.links["common"], "../../packages/shared");

        assert_eq!(
            lock.importers["packages/shared"].dependencies["react-dom"],
            "react-dom@18.3.1"
        );
        assert_eq!(lock.importers.len(), 3);
    }

    #[test]
    fn rejects_mismatched_workspace_links() {
        let registry = registry(vec![]);
        for (spec, message) in [
            ("workspace:^2", "does not match ^2"),
            (
                "workspace:missing@*",
                "no workspace package is named missing",
            ),
        ] {
            let projects = BTreeMap::from([
                (
                    ".".to_string(),
                    project("root", "0.0.0", &[("shared", spec)]),
                ),
                (
                    "packages/shared".to_string(),
                    project("shared", "1.0.0", &[]),
                ),
            ]);
            let error = super::resolve(&projects, &registry, &[]).unwrap_err();
            assert!(format!("{error:?}").contains(message), "{spec}: {error:?}");
        }
    }

    #[test]
    fn computes_relative_paths() {
        assert_eq!(relative_path(".", "packages/a"), "packages/a");
        assert_eq!(relative_path("packages/a", "packages/b"), "../b");
        assert_eq!(relative_path("apps/web", "."), "../..");
        assert_eq!(relative_path("packages/a", "packages/a"), ".");
    }

    #[test]
    fn rejects_unsupported_specifiers() {
        let registry = registry(vec![]);
        let error = resolve(&specifiers(&[("x", "github:user/repo")]), &registry, &[]).unwrap_err();
        assert!(format!("{error:?}").contains("not supported yet"));
    }
}
