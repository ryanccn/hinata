// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use eyre::{Result, WrapErr, bail, eyre};
use log::warn;
use owo_colors::colors::Yellow;
use serde::Deserialize;

use crate::lock::{self, Lock, Patch, Specifiers};
use crate::logging::LogDisplay as _;
use crate::registry::{DEFAULT_REGISTRY, Registry};
use crate::{manifest, resolve};

pub const LOCKFILE: &str = "pnpm-lock.yaml";

const RUNTIME: &str = "runtime:";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PnpmLock {
    #[serde(default)]
    importers: BTreeMap<String, Importer>,
    #[serde(default)]
    packages: BTreeMap<String, Package>,
    #[serde(default)]
    snapshots: BTreeMap<String, Snapshot>,
    #[serde(default)]
    patched_dependencies: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Importer {
    #[serde(default)]
    dependencies: BTreeMap<String, Dependency>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, Dependency>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    specifier: String,
    version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Package {
    resolution: Resolution,
    os: Option<Vec<String>>,
    cpu: Option<Vec<String>>,
    libc: Option<Vec<String>>,
    #[serde(default)]
    has_bin: bool,
}

#[derive(Deserialize)]
struct Resolution {
    integrity: Option<String>,
    tarball: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, String>,
}

/// Leaves `install_script` unset, since pnpm lockfiles do not record it.
pub fn read(root: &Path, patches: &BTreeMap<String, Patch>) -> Result<Option<Lock>> {
    let path = root.join(LOCKFILE);
    let Some(source) =
        manifest::read_if_exists(&path).wrap_err_with(|| format!("reading {}", path.display()))?
    else {
        return Ok(None);
    };
    let pnpm = document(&source).wrap_err_with(|| format!("parsing {}", path.display()))?;

    // Patches configured only where hinata does not look would otherwise be silently skipped.
    if let Some(key) = pnpm
        .patched_dependencies
        .keys()
        .find(|key| !patches.contains_key(*key))
    {
        bail!(
            "{key} is patched in {LOCKFILE}, but not in patchedDependencies of pnpm-workspace.yaml or hinata.patchedDependencies in package.json"
        );
    }

    convert(pnpm)
        .map(Some)
        .wrap_err_with(|| format!("parsing {}", path.display()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstallScript {
    #[serde(default)]
    has_install_script: bool,
}

pub fn fill_install_scripts(lock: &mut Lock, registry: &dyn Registry) -> Result<()> {
    let mut versions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for package in lock.packages.values() {
        versions
            .entry(package.name.clone())
            .or_default()
            .push(package.version.clone());
    }
    let names: Vec<String> = versions.keys().cloned().collect();
    let cached = registry.cached(&names);

    let mut packuments = HashMap::new();
    let mut outdated = Vec::new();
    for (name, packument) in names.into_iter().zip(cached) {
        match packument {
            Some(packument)
                if versions[&name]
                    .iter()
                    .all(|version| packument.versions.contains_key(version)) =>
            {
                packuments.insert(name, packument);
            }
            _ => outdated.push(name),
        }
    }
    if !outdated.is_empty() {
        let fetched = registry.fetch(&outdated)?;
        packuments.extend(outdated.into_iter().zip(fetched));
    }
    for (id, package) in &mut lock.packages {
        let manifest = packuments[&package.name]
            .versions
            .get(&package.version)
            .ok_or_else(|| eyre!("{id} is not in the registry"))?;
        package.install_script = serde_json::from_str::<InstallScript>(manifest.get())
            .is_ok_and(|manifest| manifest.has_install_script);
    }
    Ok(())
}

fn document(source: &str) -> Result<PnpmLock> {
    // pnpm can write other documents before the one describing the project.
    let document = serde_yaml::Deserializer::from_str(source)
        .map(serde_yaml::Value::deserialize)
        .last()
        .ok_or_else(|| eyre!("the lockfile is empty"))??;
    let version = match document.get("lockfileVersion") {
        Some(serde_yaml::Value::String(version)) => version.clone(),
        Some(serde_yaml::Value::Number(version)) => version.to_string(),
        _ => bail!("the lockfile has no lockfileVersion"),
    };
    if version != "9.0" {
        bail!("lockfileVersion {version} is not supported; only 9.0 is");
    }
    Ok(serde_yaml::from_value(document)?)
}

fn convert(pnpm: PnpmLock) -> Result<Lock> {
    let mut packages = BTreeMap::new();
    for (id, snapshot) in pnpm.snapshots {
        let key = id.split_once('(').map_or(id.as_str(), |(key, _)| key);
        let (name, version) =
            split_key(key).ok_or_else(|| eyre!("{id} is not a valid package id"))?;
        if version.starts_with(RUNTIME) {
            warn!(
                "skipping {}, which is a runtime rather than a package",
                id.log_display::<Yellow>()
            );
            continue;
        }
        let entry = pnpm
            .packages
            .get(key)
            .ok_or_else(|| eyre!("{id} has no entry in packages"))?;
        let not_registry = || eyre!("{id} is not from a registry, which is not supported yet");
        let integrity = entry
            .resolution
            .integrity
            .clone()
            .ok_or_else(not_registry)?;
        let url = match &entry.resolution.tarball {
            None => format!(
                "{DEFAULT_REGISTRY}/{name}/-/{}-{version}.tgz",
                unscoped(name)
            ),
            Some(url) if url.starts_with("https://") || url.starts_with("http://") => url.clone(),
            Some(_) => return Err(not_registry()),
        };
        let package = lock::Package {
            name: name.to_string(),
            version: version.to_string(),
            url,
            integrity,
            deps: dep_paths(&id, snapshot.dependencies)?,
            optional_deps: dep_paths(&id, snapshot.optional_dependencies)?,
            os: entry.os.clone(),
            cpu: entry.cpu.clone(),
            libc: entry.libc.clone(),
            has_bin: entry.has_bin,
            install_script: false,
            build: false,
            impure_build: false,
            build_inputs: Vec::new(),
            patch: None,
        };
        packages.insert(id, package);
    }

    let mut importers = BTreeMap::new();
    for (path, importer) in pnpm.importers {
        let mut result = lock::Importer::default();
        let groups = [
            (importer.dependencies, &mut result.dependencies),
            (importer.dev_dependencies, &mut result.dev_dependencies),
            (
                importer.optional_dependencies,
                &mut result.optional_dependencies,
            ),
        ];
        for ((dependencies, ids), specifiers) in
            groups.into_iter().zip(result.specifiers.groups_mut())
        {
            for (alias, dependency) in dependencies {
                if let Some(target) = dependency.version.strip_prefix("link:") {
                    result.links.insert(alias.clone(), target.to_string());
                } else {
                    let path = dep_path(&alias, &dependency.version);
                    if is_runtime(&path) {
                        continue;
                    }
                    ids.insert(alias.clone(), path);
                }
                specifiers.insert(alias, dependency.specifier);
            }
        }
        importers.insert(path, result);
    }

    Ok(Lock {
        version: lock::VERSION,
        sccs: lock::find_cycles(&packages),
        packages,
        importers,
    })
}

fn dep_paths(id: &str, references: BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let mut paths = BTreeMap::new();
    for (alias, reference) in references {
        if reference.starts_with("link:") {
            bail!("{id} depends on {alias} through {reference}, which is not supported yet");
        }
        let path = dep_path(&alias, &reference);
        if !is_runtime(&path) {
            paths.insert(alias, path);
        }
    }
    Ok(paths)
}

/// Runtimes are skipped when converting, and pnpm records them even when package.json does not
/// list them as dependencies.
pub fn without_runtimes(specifiers: &Specifiers) -> Specifiers {
    let mut specifiers = specifiers.clone();
    for group in specifiers.groups_mut() {
        group.retain(|_, spec| !spec.starts_with(RUNTIME));
    }
    specifiers
}

fn is_runtime(id: &str) -> bool {
    split_key(id).is_some_and(|(_, version)| version.starts_with(RUNTIME))
}

/// References are versions with peer suffixes, or full ids when the dependency is aliased.
fn dep_path(alias: &str, reference: &str) -> String {
    let aliased = reference.starts_with('@')
        || reference.find('@').is_some_and(|at| {
            [':', '(']
                .into_iter()
                .all(|delimiter| reference.find(delimiter).is_none_or(|index| at < index))
        });
    if aliased {
        reference.to_string()
    } else {
        format!("{alias}@{reference}")
    }
}

fn split_key(key: &str) -> Option<(&str, &str)> {
    resolve::split_version(key).filter(|(_, version)| !version.is_empty())
}

fn unscoped(name: &str) -> &str {
    name.rsplit_once('/').map_or(name, |(_, name)| name)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::registry::Packument;
    use serde_json::json;

    fn parse(source: &str) -> Result<Lock> {
        convert(document(source)?)
    }

    const LOCKFILE_V9: &str = "lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:

  .:
    dependencies:
      '@scope/tool':
        specifier: ^2.0.0
        version: 2.0.0
      react-dom:
        specifier: ^18.3.1
        version: 18.3.1(react@18.3.1)
      strip:
        specifier: npm:strip-ansi@^6
        version: strip-ansi@6.0.1
    devDependencies:
      node:
        specifier: runtime:22.18.0
        version: runtime:22.18.0
      react:
        specifier: ^18.3.1
        version: 18.3.1
      shared:
        specifier: workspace:*
        version: link:packages/shared

packages:

  '@scope/tool@2.0.0':
    resolution: {integrity: sha512-tool}
    hasBin: true
    os: [darwin, linux]
    cpu: [arm64]

  a@1.0.0:
    resolution: {integrity: sha512-a, tarball: https://example.test/a-1.0.0.tgz}

  b@1.0.0:
    resolution: {integrity: sha512-b}

  node@runtime:22.18.0:
    hasBin: true
    resolution:
      type: variations
      variants:
        - resolution:
            type: binary
            archive: tarball
            url: https://nodejs.test/node-v22.18.0-darwin-arm64.tar.gz
            integrity: sha256-node
            bin: bin/node
          targets:
            - os: darwin
              cpu: arm64

  react-dom@18.3.1:
    resolution: {integrity: sha512-react-dom}
    peerDependencies:
      react: ^18.3.1

  react@18.3.1:
    resolution: {integrity: sha512-react}

  strip-ansi@6.0.1:
    resolution: {integrity: sha512-strip-ansi}

snapshots:

  '@scope/tool@2.0.0':
    dependencies:
      a: 1.0.0
    optionalDependencies:
      b: 1.0.0

  a@1.0.0:
    dependencies:
      b: 1.0.0

  b@1.0.0:
    dependencies:
      a: 1.0.0
      node: runtime:22.18.0

  node@runtime:22.18.0: {}

  react-dom@18.3.1(react@18.3.1):
    dependencies:
      react: 18.3.1

  react@18.3.1: {}

  strip-ansi@6.0.1: {}
";

    #[test]
    fn converts_importers() {
        let lock = parse(LOCKFILE_V9).unwrap();
        let root = &lock.importers["."];
        assert_eq!(
            root.dependencies["react-dom"],
            "react-dom@18.3.1(react@18.3.1)"
        );
        assert_eq!(root.dependencies["strip"], "strip-ansi@6.0.1");
        assert_eq!(root.dependencies["@scope/tool"], "@scope/tool@2.0.0");
        assert_eq!(root.dev_dependencies["react"], "react@18.3.1");
        assert!(!root.dev_dependencies.contains_key("shared"));
        assert!(!root.dev_dependencies.contains_key("node"));
        assert!(!root.specifiers.dev_dependencies.contains_key("node"));
        assert_eq!(root.links["shared"], "packages/shared");
        assert_eq!(root.specifiers.dependencies["strip"], "npm:strip-ansi@^6");
        assert_eq!(root.specifiers.dev_dependencies["shared"], "workspace:*");
    }

    #[test]
    fn converts_packages() {
        let lock = parse(LOCKFILE_V9).unwrap();
        assert_eq!(lock.packages.len(), 6);

        let tool = &lock.packages["@scope/tool@2.0.0"];
        assert_eq!(tool.name, "@scope/tool");
        assert_eq!(tool.version, "2.0.0");
        assert_eq!(
            tool.url,
            "https://registry.npmjs.org/@scope/tool/-/tool-2.0.0.tgz"
        );
        assert_eq!(tool.integrity, "sha512-tool");
        assert!(tool.has_bin);
        assert_eq!(
            tool.os,
            Some(vec!["darwin".to_string(), "linux".to_string()])
        );
        assert_eq!(tool.cpu, Some(vec!["arm64".to_string()]));
        assert_eq!(tool.deps["a"], "a@1.0.0");
        assert_eq!(tool.optional_deps["b"], "b@1.0.0");

        assert_eq!(
            lock.packages["a@1.0.0"].url,
            "https://example.test/a-1.0.0.tgz"
        );
        let react_dom = &lock.packages["react-dom@18.3.1(react@18.3.1)"];
        assert_eq!(react_dom.name, "react-dom");
        assert_eq!(react_dom.deps["react"], "react@18.3.1");
        assert!(!lock.packages["b@1.0.0"].deps.contains_key("node"));

        assert_eq!(
            lock.sccs,
            vec![vec!["a@1.0.0".to_string(), "b@1.0.0".to_string()]]
        );
    }

    #[test]
    fn reads_the_last_document() {
        let source = format!(
            "---\nlockfileVersion: '9.0'\nimporters:\n  .:\n    configDependencies: {{}}\n---\n{LOCKFILE_V9}"
        );
        assert_eq!(parse(&source).unwrap().packages.len(), 6);
    }

    #[test]
    fn rejects_unsupported_lockfiles() {
        let error = parse("lockfileVersion: '6.0'\n").unwrap_err();
        assert!(error.to_string().contains("6.0"));

        let git = "lockfileVersion: '9.0'
packages:
  x@1.0.0:
    resolution: {commit: abc, repo: https://git.test/x, type: git}
snapshots:
  x@1.0.0: {}
";
        assert!(
            parse(git)
                .unwrap_err()
                .to_string()
                .contains("not from a registry")
        );
    }

    #[test]
    fn requires_pnpm_patches_to_be_configured() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(LOCKFILE),
            format!("{LOCKFILE_V9}\npatchedDependencies:\n  react@18.3.1: abc\n"),
        )
        .unwrap();

        let error = read(dir.path(), &BTreeMap::new()).unwrap_err();
        assert!(error.to_string().contains("patchedDependencies"), "{error}");

        let patches = BTreeMap::from([(
            "react@18.3.1".to_string(),
            Patch {
                path: "react.patch".to_string(),
                hash: "abc".to_string(),
            },
        )]);
        assert!(read(dir.path(), &patches).unwrap().is_some());
    }

    struct MemoryRegistry;

    impl Registry for MemoryRegistry {
        fn fetch(&self, names: &[String]) -> Result<Vec<Packument>> {
            names
                .iter()
                .map(|name| {
                    let versions = if name == "a" {
                        json!({ "1.0.0": { "hasInstallScript": true } })
                    } else {
                        json!({ "1.0.0": {}, "2.0.0": {}, "6.0.1": {}, "18.3.1": {} })
                    };
                    Ok(serde_json::from_str(
                        &json!({ "versions": versions }).to_string(),
                    )?)
                })
                .collect()
        }
    }

    #[test]
    fn fills_install_scripts_from_the_registry() {
        let mut lock = parse(LOCKFILE_V9).unwrap();
        fill_install_scripts(&mut lock, &MemoryRegistry).unwrap();
        assert!(lock.packages["a@1.0.0"].install_script);
        assert!(!lock.packages["b@1.0.0"].install_script);
    }
}
