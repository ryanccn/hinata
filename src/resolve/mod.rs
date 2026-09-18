// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Versions are chosen breadth-first, reusing an already chosen version wherever it satisfies a
//! range. Peers are then resolved from ancestors, and a package gets one instance per distinct set
//! of resolved peers.

mod choose;
mod overrides;
mod peers;

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, TimeDelta, Utc};
use eyre::{Result, WrapErr, bail, eyre};
use log::warn;
use node_semver::{Range, Version};
use owo_colors::colors::Yellow;

use crate::lock::{self, Lock, Specifiers};
use crate::logging::LogDisplay as _;
use crate::registry::{Registry, VersionManifest};

use choose::Chooser;
use overrides::Overrides;
use peers::build_packages;

pub use choose::Fetched;

const WORKSPACE: &str = "workspace:";

pub struct Project {
    pub name: Option<String>,
    pub version: Option<String>,
    pub specifiers: Specifiers,
}

type Groups = [BTreeMap<String, String>; 3];

type Workspace<'p> = HashMap<&'p str, (&'p str, &'p Project)>;

/// The time that newly chosen versions must have been published before, if any.
pub fn release_cutoff(minutes: u64) -> Option<DateTime<Utc>> {
    (minutes > 0).then(|| {
        i64::try_from(minutes)
            .ok()
            .and_then(TimeDelta::try_minutes)
            .and_then(|age| Utc::now().checked_sub_signed(age))
            .unwrap_or(DateTime::<Utc>::MIN_UTC)
    })
}

/// Projects are keyed by their `/`-separated path relative to the workspace root, which is `.`.
/// Versions in `preferred` are used even if published after `cutoff`.
pub fn resolve(
    projects: &BTreeMap<String, Project>,
    registry: &dyn Registry,
    preferred: &[(String, String)],
    cutoff: Option<DateTime<Utc>>,
    overrides: &BTreeMap<String, String>,
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

    let parsed = Overrides::new(overrides)?;
    let mut chooser = Chooser::new(registry, preferred, cutoff, &parsed);
    let groups = chooser.choose_all(&registry_specifiers)?;
    for key in parsed.unused() {
        warn!(
            "nothing depends on {}, so its override was not applied",
            key.log_display::<Yellow>()
        );
    }
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
        nixpkgs: None,
        node: None,
        overrides: overrides.clone(),
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

pub fn parse_spec(alias: &str, spec: &str) -> Result<(String, String)> {
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
pub fn split_version(spec: &str) -> Option<(&str, &str)> {
    let at = spec.get(1..)?.find('@')? + 1;
    Some((&spec[..at], &spec[at + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Packument;
    use serde_json::{Value, json};
    use std::cell::RefCell;
    use std::collections::BTreeSet;

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
        resolve_overridden(specifiers, registry, preferred, &BTreeMap::new())
    }

    fn resolve_overridden(
        specifiers: &Specifiers,
        registry: &dyn Registry,
        preferred: &[(String, String)],
        overrides: &BTreeMap<String, String>,
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
            None,
            overrides,
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

    fn override_registry() -> MemoryRegistry {
        registry(vec![
            (
                "old",
                vec![("1.0.0", json!({ "dependencies": { "dep": "^1" } }))],
            ),
            (
                "dep",
                vec![
                    ("1.0.0", json!({})),
                    ("2.0.0", json!({})),
                    ("3.0.0", json!({})),
                ],
            ),
        ])
    }

    #[test]
    fn overrides_what_dependencies_ask_for() {
        let overrides = deps(&[("dep", "^2"), ("absent", "^1")]);
        let lock = resolve_overridden(
            &specifiers(&[("old", "^1")]),
            &override_registry(),
            &[],
            &overrides,
        )
        .unwrap();

        assert_eq!(lock.packages["old@1.0.0"].deps["dep"], "dep@2.0.0");
        assert!(!lock.packages.contains_key("dep@1.0.0"));
        // An override nothing depends on is reported, not fatal.
        assert_eq!(lock.overrides, overrides);
    }

    #[test]
    fn overrides_only_the_ranges_a_selector_matches() {
        let lock = resolve_overridden(
            &specifiers(&[("old", "^1"), ("dep", "^3")]),
            &override_registry(),
            &[],
            &deps(&[("dep@^1", "^2")]),
        )
        .unwrap();

        assert_eq!(lock.packages["old@1.0.0"].deps["dep"], "dep@2.0.0");
        assert_eq!(lock.importers["."].dependencies["dep"], "dep@3.0.0");
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
    fn skips_versions_published_after_the_cutoff() {
        let cutoff = DateTime::parse_from_rfc3339("2026-01-02T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut registry = registry(vec![(
            "a",
            vec![
                ("1.0.0", json!({})),
                ("1.1.0-rc.1", json!({})),
                ("1.1.0", json!({})),
            ],
        )]);
        let a = registry.packuments.get_mut("a").unwrap();
        a["modified"] = json!("2026-01-03T00:00:00Z");
        a["time"] = json!({
            "1.0.0": "2025-12-01T00:00:00Z",
            "1.1.0-rc.1": "2025-12-15T00:00:00Z",
            "1.1.0": "2026-01-03T00:00:00Z",
        });
        let resolve = |spec: &str, locked: &[(String, String)]| {
            let project = Project {
                name: None,
                version: None,
                specifiers: specifiers(&[("a", spec)]),
            };
            super::resolve(
                &BTreeMap::from([(".".to_string(), project)]),
                &registry,
                locked,
                Some(cutoff),
                &BTreeMap::new(),
            )
            .map(|lock| lock.importers["."].dependencies["a"].clone())
        };

        assert_eq!(resolve("^1", &[]).unwrap(), "a@1.0.0");
        assert_eq!(resolve("latest", &[]).unwrap(), "a@1.0.0");
        let locked = [("a".to_string(), "1.1.0".to_string())];
        assert_eq!(resolve("^1", &locked).unwrap(), "a@1.1.0");

        let error = resolve("1.1.0", &[]).unwrap_err();
        assert!(
            format!("{error:?}").contains("minimumReleaseAge"),
            "{error:?}"
        );
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
        let lock = super::resolve(&projects, &react_registry(), &[], None, &BTreeMap::new()).unwrap();

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
            let error =
                super::resolve(&projects, &registry, &[], None, &BTreeMap::new()).unwrap_err();
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
