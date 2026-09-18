// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Versions are chosen breadth-first, reusing an already chosen version wherever it satisfies a
//! range.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr, bail, eyre};
use log::{debug, warn};
use node_semver::{Range, Version};
use owo_colors::colors::Yellow;

use crate::lock::Specifiers;
use crate::logging::{LogDisplay as _, plural};
use crate::registry::{Packument, Registry, VersionManifest};

use super::{Groups, Node, parse_spec};

pub(crate) struct Fetched {
    packument: Packument,
    versions: BTreeMap<Version, String>,
    /// Cached packuments may predate versions and tags published since.
    verified: bool,
}

impl Fetched {
    pub(crate) fn new(packument: Packument, verified: bool) -> Self {
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

    fn released_before(&self, raw: &str, cutoff: Option<DateTime<Utc>>) -> bool {
        cutoff.is_none_or(|cutoff| {
            !self.packument.changed_since(cutoff)
                || self
                    .packument
                    .time
                    .get(raw)
                    .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
                    .is_some_and(|time| time < cutoff)
        })
    }

    /// The version `tag` points to, or the highest one below it published before `cutoff`.
    pub(crate) fn tagged(
        &self,
        name: &str,
        tag: &str,
        cutoff: Option<DateTime<Utc>>,
    ) -> Result<String> {
        if !self.packument.dist_tags.contains_key(tag) {
            bail!("{name} has no version tagged {tag}");
        }

        pick_version(self, tag, None, &|raw| self.released_before(raw, cutoff))
            .map(|(_, raw)| raw)
            .ok_or_else(|| eyre!("every version of {name} up to {tag} was published too recently for hinata.minimumReleaseAge"))
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

pub(crate) struct Chooser<'r> {
    registry: &'r dyn Registry,
    cutoff: Option<DateTime<Utc>>,
    packuments: HashMap<String, Fetched>,
    chosen: HashMap<String, BTreeSet<Version>>,
    pub(crate) nodes: BTreeMap<String, Node>,
}

impl<'r> Chooser<'r> {
    pub(crate) fn new(
        registry: &'r dyn Registry,
        preferred: &[(String, String)],
        cutoff: Option<DateTime<Utc>>,
    ) -> Self {
        let mut chosen: HashMap<String, BTreeSet<Version>> = HashMap::new();
        for (name, version) in preferred {
            if let Ok(version) = Version::parse(version) {
                chosen.entry(name.clone()).or_default().insert(version);
            }
        }
        Chooser {
            registry,
            cutoff,
            packuments: HashMap::new(),
            chosen,
            nodes: BTreeMap::new(),
        }
    }

    pub(crate) fn choose_all(&mut self, importers: &[Specifiers]) -> Result<Vec<Groups>> {
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
        let chosen = self.chosen.get(&request.name);
        let released = |raw: &str| fetched.released_before(raw, self.cutoff);
        match pick_version(fetched, &request.range, chosen, &released) {
            Some((version, raw)) => {
                self.chosen
                    .entry(request.name.clone())
                    .or_default()
                    .insert(version);
                Ok(Some(format!("{}@{raw}", request.name)))
            }
            None if request.edge == Edge::Dependency
                && pick_version(fetched, &request.range, chosen, &|_| true).is_some() =>
            {
                bail!(
                    "every version of {} matching {} was published too recently for hinata.minimumReleaseAge",
                    request.name,
                    request.range
                )
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

/// Versions not yet `chosen` must be `released`. A tag whose version is not falls back to the
/// highest released version below it, and only to prereleases if the tagged version is one.
fn pick_version(
    fetched: &Fetched,
    range: &str,
    chosen: Option<&BTreeSet<Version>>,
    released: &dyn Fn(&str) -> bool,
) -> Option<(Version, String)> {
    let available = |version: &Version| {
        fetched
            .versions
            .get_key_value(version)
            .map(|(version, raw)| (version.clone(), raw.clone()))
    };
    if let Some(tagged) = fetched.packument.dist_tags.get(range) {
        let (tagged, _) = available(&Version::parse(tagged).ok()?)?;
        return fetched
            .versions
            .range(..=&tagged)
            .rev()
            .find(|(version, raw)| {
                (tagged.is_prerelease() || !version.is_prerelease()) && released(raw)
            })
            .map(|(version, raw)| (version.clone(), raw.clone()));
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
            .filter(|(_, raw)| released(raw))
    };
    let highest = || {
        fetched
            .versions
            .iter()
            .rev()
            .find(|(version, raw)| range.satisfies(version) && released(raw))
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
