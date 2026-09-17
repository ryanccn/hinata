// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io::{ErrorKind, Write as _};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use eyre::{Result, WrapErr, bail, eyre};
use log::{debug, info, warn};
use node_semver::{Range, Version};
use owo_colors::OwoColorize as _;
use owo_colors::colors::{Blue, Yellow};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::lock::{self, Lock, Patch};
use crate::logging::{LogDisplay as _, plural};
use crate::manifest::{BuildMode, Manifest};
use crate::registry::{DEFAULT_REGISTRY, HttpRegistry};
use crate::resolve::Project;
use crate::{impure, link, manifest, nix, pnpm, resolve};

const LOCKFILE: &str = "hinata.lock";
/// Installs from pnpm-lock.yaml have no hinata.lock to record Nixpkgs and Node.js in.
const COMPAT: &str = "node_modules/.hinata.compat.json";
/// Locked when neither hinata.nixpkgs nor flake.lock chooses Nixpkgs.
const DEFAULT_NIXPKGS: &str = "github:NixOS/nixpkgs/nixpkgs-unstable";

#[derive(Default, Serialize, Deserialize)]
struct Compat {
    nixpkgs: Option<lock::Nixpkgs>,
    node: Option<lock::Node>,
}

struct NixpkgsSource<'a> {
    /// Lock again even if Nixpkgs is already locked.
    update: bool,
    compat: Compat,
    lock_nixpkgs: &'a dyn Fn(&str) -> Result<lock::Nixpkgs>,
    node_versions: &'a dyn Fn(&lock::Nixpkgs) -> Result<BTreeMap<String, String>>,
}

impl NixpkgsSource<'static> {
    fn new(root: &Path, update: bool) -> Self {
        NixpkgsSource {
            update,
            compat: fs::read_to_string(root.join(COMPAT))
                .ok()
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default(),
            lock_nixpkgs: &nix::lock_nixpkgs,
            node_versions: &nix::node_versions,
        }
    }
}

pub struct Options {
    pub dir: PathBuf,
    pub dev: bool,
    pub refresh: bool,
    pub update: Update,
    pub update_nixpkgs: bool,
    pub lockfile: Lockfile,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lockfile {
    /// Fail rather than resolve or change hinata.lock.
    Frozen,
    /// Write hinata.lock, unless installing from a matching pnpm-lock.yaml.
    Default,
    /// Write hinata.lock even when installing from pnpm-lock.yaml.
    Save,
}

pub enum Update {
    Keep,
    All,
    Only(BTreeSet<String>),
}

impl Update {
    fn includes(&self, name: &str) -> bool {
        match self {
            Update::Keep => false,
            Update::All => true,
            Update::Only(names) => names.contains(name),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImporterRoots {
    #[serde(default)]
    dependencies: BTreeMap<String, PathBuf>,
    #[serde(default)]
    dev_dependencies: BTreeMap<String, PathBuf>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, PathBuf>,
}

pub fn run(options: &Options) -> Result<()> {
    let root = fs::canonicalize(&options.dir)
        .wrap_err_with(|| format!("opening {}", options.dir.display()))?;
    let manifest = manifest::read(&root)?;
    prune_projects(&cache_dir()?.join("projects"));
    let project = project_dir(&root)?;

    let (lock, lock_json, lock_file) = update_lock(
        &root,
        &manifest,
        &root.join(LOCKFILE),
        &options.update,
        options.lockfile,
        &NixpkgsSource::new(&root, options.update_nixpkgs),
    )?;

    write_atomically(&project.join("path"), root.as_os_str().as_bytes())?;
    let gcroot = project.join("gcroot");
    let key_path = project.join("key");

    let node_major = native_addon_node(&root, &lock);
    let key = cache_key(&lock_json, options.dev, node_major);

    let (workspace, built) = match cached_workspace(&key_path, &gcroot, &key) {
        Some(workspace) if !options.refresh => {
            info!(
                "{} is unchanged since the last install, skipping the Nix build {}",
                lock_file.log_display::<Blue>(),
                "(pass --refresh to rebuild)".dimmed()
            );
            (workspace, false)
        }
        _ => {
            info!(
                "building {} with Nix {}",
                plural(lock.packages.len(), "package", "packages"),
                if options.dev {
                    ""
                } else {
                    "(without devDependencies)"
                }
                .dimmed()
            );
            // A key left behind would vouch for whichever workspace the GC root points to next.
            if let Err(error) = fs::remove_file(&key_path)
                && error.kind() != ErrorKind::NotFound
            {
                return Err(error).wrap_err_with(|| format!("removing {}", key_path.display()));
            }
            let workspace = nix::build_workspace(
                &root,
                &lock,
                &lock_json,
                options.dev,
                node_major,
                manifest.substituters(),
                &gcroot,
            )?;
            debug!(
                "built workspace {}",
                workspace.display().log_display::<Blue>()
            );
            (workspace, true)
        }
    };

    let node = lock
        .node
        .as_ref()
        .map(|_| fs::canonicalize(workspace.join("node")))
        .transpose()
        .wrap_err("finding Node.js in the Nix build")?;

    link_importers(&root, &lock, &workspace, node.as_deref())?;

    if lock_file == pnpm::LOCKFILE {
        let compat = Compat {
            nixpkgs: lock.nixpkgs.clone(),
            node: lock.node.clone(),
        };
        write_atomically(&root.join(COMPAT), &serde_json::to_vec(&compat)?)?;
    }

    if built {
        let impure_builds: BTreeMap<String, PathBuf> =
            serde_json::from_str(&fs::read_to_string(workspace.join("impure-builds.json"))?)?;
        for (id, dir) in &impure_builds {
            let package = &lock.packages[id];
            impure::run(&root, &package.name, &package.version, dir, node.as_deref())?;
        }
        // Written last, so that failed impure builds run again on the next install.
        write_atomically(&key_path, key.as_bytes())?;
    }

    info!(
        "installed {}{}",
        plural(lock.packages.len(), "package", "packages"),
        if lock.importers.len() == 1 {
            String::new()
        } else {
            format!(
                " into {}",
                plural(lock.importers.len(), "workspace", "workspaces")
            )
        }
    );
    Ok(())
}

/// The workspace of the last install, if it still matches the project.
pub fn current_workspace(root: &Path) -> Result<Option<PathBuf>> {
    let manifest = manifest::read(root)?;
    let project = project_dir(root)?;
    let (lock, lock_json, _) = update_lock(
        root,
        &manifest,
        &root.join(LOCKFILE),
        &Update::Keep,
        Lockfile::Frozen,
        &NixpkgsSource::new(root, false),
    )?;

    let node_major = native_addon_node(root, &lock);

    Ok([true, false].into_iter().find_map(|dev| {
        cached_workspace(
            &project.join("key"),
            &project.join("gcroot"),
            &cache_key(&lock_json, dev, node_major),
        )
    }))
}

fn project_dir(root: &Path) -> Result<PathBuf> {
    let hash = hex(Sha256::digest(root.as_os_str().as_bytes()));
    Ok(cache_dir()?.join("projects").join(hash))
}

fn native_addon_node(root: &Path, lock: &Lock) -> Option<u32> {
    if lock.node.is_some() || !lock.packages.values().any(|package| package.build) {
        return None;
    }
    // Native addons only load in the Node.js major version they were built for.
    let node = node_major(root);
    match node {
        Some(major) => debug!(
            "building native addons for Node.js {}",
            major.log_display::<Blue>()
        ),
        None => warn!(
            "found no working {} on PATH, so native addons will be built for nixpkgs' default Node.js",
            "node".log_display::<Yellow>()
        ),
    }
    node
}

fn link_importers(root: &Path, lock: &Lock, workspace: &Path, node: Option<&Path>) -> Result<()> {
    let importer_roots: BTreeMap<String, ImporterRoots> =
        serde_json::from_str(&fs::read_to_string(workspace.join("importers.json"))?)?;
    for (path, importer) in &lock.importers {
        let dir = root.join(path);
        let roots = importer_roots
            .get(path)
            .ok_or_else(|| eyre!("the Nix build has no packages for importer {path}"))?;
        let mut packages = roots.optional_dependencies.clone();
        packages.extend(roots.dev_dependencies.clone());
        packages.extend(roots.dependencies.clone());
        packages.extend(
            importer
                .links
                .iter()
                .map(|(alias, target)| (alias.clone(), dir.join(target))),
        );
        let node_modules = dir.join("node_modules");
        debug!(
            "linking {} into {}",
            plural(packages.len(), "direct dependency", "direct dependencies"),
            node_modules.display().log_display::<Blue>()
        );
        link::sync(&node_modules, &packages, node)
            .wrap_err_with(|| format!("linking {}", node_modules.display()))?;
    }
    Ok(())
}

fn update_lock(
    root: &Path,
    manifest: &Manifest,
    lock_path: &Path,
    update: &Update,
    lockfile: Lockfile,
    source: &NixpkgsSource,
) -> Result<(Lock, String, &'static str)> {
    let build_inputs = manifest.build_inputs()?;
    let patches = manifest.patches(root)?;
    let projects = read_projects(root, manifest)?;
    let existing = manifest::read_if_exists(lock_path)
        .wrap_err("reading hinata.lock")?
        .map(|json| serde_json::from_str::<Lock>(&json))
        .transpose()
        .wrap_err("parsing hinata.lock")?;
    let (pnpm_lock, pinned, pinned_node) = match &existing {
        Some(lock) => (None, lock.nixpkgs.clone(), lock.node.clone()),
        None => (
            pnpm::read(root, &patches)?,
            source.compat.nixpkgs.clone(),
            source.compat.node.clone(),
        ),
    };
    let from_pnpm = pnpm_lock.is_some();
    if let Update::Only(names) = update
        && let Some(lock) = existing.as_ref().or(pnpm_lock.as_ref())
    {
        warn_missing_updates(names, lock);
    }

    let current = |lock: &Lock| {
        lock.version == lock::VERSION
            && lock.importers.len() == projects.len()
            && projects.iter().all(|(path, project)| {
                lock.importers.get(path).is_some_and(|importer| {
                    importer.specifiers == pnpm::without_runtimes(&project.specifiers)
                })
            })
    };
    let keep = matches!(update, Update::Keep);
    let (mut lock, pnpm_only) = match (existing, pnpm_lock) {
        (Some(lock), _) if keep && current(&lock) => {
            debug!(
                "{} matches package.json, not resolving again",
                LOCKFILE.log_display::<Blue>()
            );
            (lock, false)
        }
        (None, Some(mut lock)) if keep && current(&lock) => {
            if lockfile == Lockfile::Save {
                info!(
                    "looking up install scripts in {}",
                    DEFAULT_REGISTRY.log_display::<Blue>()
                );
                pnpm::fill_install_scripts(&mut lock, &HttpRegistry::new(DEFAULT_REGISTRY)?)?;
                (lock, false)
            } else {
                debug!(
                    "{} matches package.json, installing from it",
                    pnpm::LOCKFILE.log_display::<Blue>()
                );
                (lock, true)
            }
        }
        (None, Some(_)) if lockfile != Lockfile::Save => bail!(
            "{} does not match package.json; run `pnpm install` to update it, or `hinata install --save-lock` to resolve dependencies into {} instead",
            pnpm::LOCKFILE,
            LOCKFILE
        ),
        _ if lockfile == Lockfile::Frozen => return Err(outdated_lock()),
        (existing, pnpm_lock) => {
            info!(
                "resolving dependencies from {}",
                DEFAULT_REGISTRY.log_display::<Blue>()
            );
            let preferred: Vec<(String, String)> = existing
                .or(pnpm_lock)
                .map(|lock| {
                    lock.packages
                        .into_values()
                        .filter(|package| !update.includes(&package.name))
                        .map(|package| (package.name, package.version))
                        .collect()
                })
                .unwrap_or_default();
            let registry = HttpRegistry::new(DEFAULT_REGISTRY)?;
            let lock = resolve::resolve(&projects, &registry, &preferred)?;
            info!(
                "resolved {}",
                plural(lock.packages.len(), "package", "packages")
            );
            (lock, false)
        }
    };

    mark_builds(
        &mut lock,
        &manifest.allow_builds(),
        &build_inputs,
        pnpm_only,
    );
    mark_patches(&mut lock, &patches);

    let nixpkgs = pick_nixpkgs(root, pinned.clone(), manifest, lockfile, source)?;
    lock.node = match (manifest.node(), pinned_node) {
        (None, _) => None,
        (Some(range), Some(node)) if node.from == range && pinned.as_ref() == Some(&nixpkgs) => {
            Some(node)
        }
        _ if lockfile == Lockfile::Frozen => return Err(outdated_lock()),
        (Some(range), _) => {
            info!(
                "choosing Node.js {} from Nixpkgs",
                range.log_display::<Blue>()
            );
            Some(pick_node(range, &(source.node_versions)(&nixpkgs)?)?)
        }
    };
    lock.nixpkgs = Some(nixpkgs);

    let json = lock::to_json(&lock)?;
    if pnpm_only {
        return Ok((lock, json, pnpm::LOCKFILE));
    }
    if lockfile != Lockfile::Frozen {
        write_lock(lock_path, &json, from_pnpm)?;
    } else if fs::read_to_string(lock_path).ok().as_deref() != Some(json.as_str()) {
        return Err(outdated_lock());
    }
    Ok((lock, json, LOCKFILE))
}

/// flake.lock is followed whenever it changes, while other sources stay locked until updated.
fn pick_nixpkgs(
    root: &Path,
    locked: Option<lock::Nixpkgs>,
    manifest: &Manifest,
    lockfile: Lockfile,
    source: &NixpkgsSource,
) -> Result<lock::Nixpkgs> {
    if manifest.nixpkgs().is_none()
        && let Some(flake) = nix::flake_lock_nixpkgs(root)?
    {
        return match locked {
            Some(locked) if locked == flake => Ok(locked),
            _ if lockfile == Lockfile::Frozen => Err(outdated_lock()),
            _ => {
                info!(
                    "locking Nixpkgs from {}",
                    nix::FLAKE_LOCK.log_display::<Blue>()
                );
                Ok(flake)
            }
        };
    }

    let flake_ref = manifest.nixpkgs().unwrap_or(DEFAULT_NIXPKGS);
    match locked {
        Some(locked) if !source.update && locked.from == flake_ref => Ok(locked),
        _ if lockfile == Lockfile::Frozen => Err(outdated_lock()),
        _ => {
            info!("locking Nixpkgs from {}", flake_ref.log_display::<Blue>());
            (source.lock_nixpkgs)(flake_ref)
        }
    }
}

/// Nixpkgs has few releases of each major version, so the newest release of a major version that
/// the range allows is used when no release satisfies the range.
fn pick_node(range: &str, available: &BTreeMap<String, String>) -> Result<lock::Node> {
    let parsed = Range::parse(range)
        .map_err(|error| eyre!("parsing the Node.js version {range} in devEngines: {error}"))?;
    let mut versions: Vec<(Version, &String)> = available
        .iter()
        .filter_map(|(attr, version)| Some((Version::parse(version).ok()?, attr)))
        .collect();
    versions.sort();

    let newest = |allowed: &dyn Fn(&Version) -> bool| {
        versions.iter().rev().find(|(version, _)| allowed(version))
    };
    let node = |version: &Version, attr: &String| lock::Node {
        from: range.to_string(),
        attr: attr.clone(),
        version: version.to_string(),
    };

    if let Some((version, attr)) = newest(&|version| parsed.satisfies(version)) {
        return Ok(node(version, attr));
    }

    let (version, attr) = newest(&|version| {
        Range::parse(format!("{}.x", version.major)).is_ok_and(|major| parsed.allows_any(&major))
    })
    .ok_or_else(|| {
        let offered: Vec<String> = versions
            .iter()
            .map(|(version, _)| version.to_string())
            .collect();
        eyre!(
            "the locked Nixpkgs has no Node.js for {range} (it has {}); run `hinata update --nixpkgs`, or set hinata.nixpkgs to a revision that has one",
            offered.join(", ")
        )
    })?;
    warn!(
        "the locked Nixpkgs has no Node.js that satisfies {}, using {} instead",
        range.log_display::<Yellow>(),
        version.log_display::<Yellow>()
    );
    Ok(node(version, attr))
}

fn outdated_lock() -> eyre::Report {
    eyre!("{LOCKFILE} is missing or out of date; run `hinata install` to update it")
}

fn read_projects(root: &Path, manifest: &Manifest) -> Result<BTreeMap<String, Project>> {
    let mut projects = BTreeMap::from([(".".to_string(), manifest.project())]);
    for path in manifest.workspace_packages(root)? {
        let project = manifest::read(&root.join(&path))?.project();
        projects.insert(path, project);
    }
    Ok(projects)
}

fn warn_missing_updates(names: &BTreeSet<String>, lock: &Lock) {
    for name in names {
        if !lock.packages.values().any(|package| &package.name == name) {
            warn!(
                "{} is not installed, so there is nothing to update",
                name.log_display::<Yellow>()
            );
        }
    }
}

fn mark_builds(
    lock: &mut Lock,
    allow_builds: &BTreeMap<String, BuildMode>,
    build_inputs: &BTreeMap<String, Vec<String>>,
    pnpm_only: bool,
) {
    let mut skipped = BTreeSet::new();
    for package in lock.packages.values_mut() {
        // pnpm lockfiles do not record which packages have install scripts.
        let mode = allow_builds
            .get(&package.name)
            .filter(|_| pnpm_only || package.install_script);
        package.build = mode == Some(&BuildMode::Sandboxed);
        package.impure_build = mode == Some(&BuildMode::Impure);
        package.build_inputs = build_inputs
            .get(&package.name)
            .filter(|_| package.build)
            .cloned()
            .unwrap_or_default();

        if package.install_script && mode.is_none() {
            skipped.insert(package.name.clone());
        }
    }
    if skipped.is_empty() {
        return;
    }
    let names: Vec<_> = skipped
        .iter()
        .map(|name| name.log_display::<Yellow>().to_string())
        .collect();
    warn!(
        "skipped install scripts of {}; add them to {} in package.json to run them",
        names.join(", "),
        "hinata.allowBuilds".log_display::<Blue>()
    );
}

fn mark_patches(lock: &mut Lock, patches: &BTreeMap<String, Patch>) {
    let mut unused: BTreeSet<&String> = patches.keys().collect();

    for package in lock.packages.values_mut() {
        let exact = format!("{}@{}", package.name, package.version);
        package.patch = [exact, package.name.clone()]
            .into_iter()
            .find_map(|key| patches.get_key_value(&key))
            .map(|(key, patch)| {
                unused.remove(key);
                patch.clone()
            });
    }

    for key in unused {
        warn!(
            "no installed package matches {} in patchedDependencies",
            key.log_display::<Yellow>()
        );
    }
}

fn write_lock(lock_path: &Path, json: &str, from_pnpm: bool) -> Result<()> {
    let previous = fs::read_to_string(lock_path).ok();
    if previous.as_deref() == Some(json) {
        return Ok(());
    }
    fs::write(lock_path, json).wrap_err("writing hinata.lock")?;
    info!(
        "{} {}{}",
        if previous.is_some() {
            "updated"
        } else {
            "created"
        },
        LOCKFILE.log_display::<Blue>(),
        if from_pnpm {
            format!(
                ", which takes precedence over {} from now on",
                pnpm::LOCKFILE.log_display::<Blue>()
            )
        } else {
            String::new()
        }
    );
    Ok(())
}

fn cache_key(lock_json: &str, dev: bool, node_major: Option<u32>) -> String {
    let inputs = serde_json::to_vec(&(lock_json, dev, node_major, nix::LIBRARY))
        .expect("strings, booleans and numbers serialize");
    hex(Sha256::digest(inputs))
}

/// Runs from the project directory so that version managers select the project's Node.js.
fn node_major(root: &Path) -> Option<u32> {
    let output = Command::new("node")
        .arg("--version")
        .current_dir(root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .strip_prefix('v')?
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn cached_workspace(key_path: &Path, gcroot: &Path, key: &str) -> Option<PathBuf> {
    let workspace = fs::read_link(gcroot).ok()?;
    (fs::read_to_string(key_path).ok()? == key && workspace.exists()).then_some(workspace)
}

/// GC roots are kept outside projects, which the Nix daemon may not be permitted to read.
pub(crate) fn cache_dir() -> Result<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| eyre!("neither XDG_CACHE_HOME nor HOME is set"))?;
    Ok(cache.join("hinata"))
}

pub(crate) fn hex(bytes: impl IntoIterator<Item = u8>) -> String {
    let mut hex = String::new();
    for byte in bytes {
        write!(hex, "{byte:02x}").expect("writing to a string succeeds");
    }
    hex
}

/// Readers never see a partially written file, even when installs run concurrently.
pub(crate) fn write_atomically(path: &Path, contents: &[u8]) -> Result<()> {
    let write = || -> Result<()> {
        let dir = path.parent().expect("written files are inside a directory");
        fs::create_dir_all(dir)?;
        let mut file = tempfile::NamedTempFile::new_in(dir)?;
        file.write_all(contents)?;
        file.persist(path)?;
        Ok(())
    };
    write().wrap_err_with(|| format!("writing {}", path.display()))
}

/// Entries without a `path` file may belong to an install that is still setting them up.
pub(crate) fn prune_projects(projects: &Path) -> usize {
    let Ok(entries) = fs::read_dir(projects) else {
        return 0;
    };

    let mut removed = 0;
    for entry in entries.flatten() {
        let Ok(project) = fs::read(entry.path().join("path")) else {
            continue;
        };
        let project = PathBuf::from(OsString::from_vec(project));
        if !project.exists() && fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
            debug!(
                "removed {} of {}, which no longer exists",
                entry.path().display().log_display::<Blue>(),
                project.display().log_display::<Blue>()
            );
        }
    }

    removed
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn locked_nixpkgs(from: &str) -> lock::Nixpkgs {
        lock::Nixpkgs {
            from: from.to_string(),
            locked: BTreeMap::from([("rev".to_string(), "abc".into())]),
        }
    }

    fn source() -> NixpkgsSource<'static> {
        NixpkgsSource {
            update: false,
            compat: Compat::default(),
            lock_nixpkgs: &|from| Ok(locked_nixpkgs(from)),
            node_versions: &|_| Ok(BTreeMap::new()),
        }
    }

    #[test]
    fn installs_from_a_matching_pnpm_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = |range: &str| {
            format!(
                r#"{{ "dependencies": {{ "a": "{range}", "b": "^1.0.0" }}, "devDependencies": {{ "node": "runtime:22.18.0" }}, "hinata": {{ "allowBuilds": {{ "a": true, "b": "impure" }}, "buildInputs": {{ "a": ["cairo"] }}, "patchedDependencies": {{ "a@1.0.0": "a.patch", "a": "b.patch", "b": "b.patch" }} }} }}"#
            )
        };
        fs::write(dir.path().join("package.json"), manifest("^1.0.0")).unwrap();
        fs::write(dir.path().join("a.patch"), "a").unwrap();
        fs::write(dir.path().join("b.patch"), "b").unwrap();
        fs::write(
            dir.path().join(pnpm::LOCKFILE),
            "lockfileVersion: '9.0'
importers:
  .:
    dependencies:
      a:
        specifier: ^1.0.0
        version: 1.0.0
      b:
        specifier: ^1.0.0
        version: 1.0.0
    devDependencies:
      node:
        specifier: runtime:22.18.0
        version: runtime:22.18.0
packages:
  a@1.0.0:
    resolution: {integrity: sha512-a}
  b@1.0.0:
    resolution: {integrity: sha512-b}
  node@runtime:22.18.0:
    resolution: {type: variations, variants: []}
snapshots:
  a@1.0.0: {}
  b@1.0.0: {}
  node@runtime:22.18.0: {}
",
        )
        .unwrap();
        let lock_path = dir.path().join(LOCKFILE);

        let (lock, _, file) = update_lock(
            dir.path(),
            &manifest::read(dir.path()).unwrap(),
            &lock_path,
            &Update::Keep,
            Lockfile::Default,
            &NixpkgsSource {
                compat: Compat {
                    nixpkgs: Some(locked_nixpkgs(DEFAULT_NIXPKGS)),
                    node: None,
                },
                lock_nixpkgs: &|_| unreachable!("Nixpkgs from a compat install is locked again"),
                ..source()
            },
        )
        .unwrap();
        assert_eq!(file, pnpm::LOCKFILE);
        assert_eq!(lock.nixpkgs, Some(locked_nixpkgs(DEFAULT_NIXPKGS)));
        assert!(lock.packages["a@1.0.0"].build);
        assert!(!lock.packages["a@1.0.0"].impure_build);
        assert_eq!(lock.packages["a@1.0.0"].build_inputs, ["cairo"]);
        assert!(!lock.packages["b@1.0.0"].build);
        assert!(lock.packages["b@1.0.0"].impure_build);
        assert!(!lock_path.exists());

        let patch = |id: &str| lock.packages[id].patch.as_ref().unwrap().path.as_str();
        assert_eq!(patch("a@1.0.0"), "a.patch");
        assert_eq!(patch("b@1.0.0"), "b.patch");

        fs::write(dir.path().join("package.json"), manifest("^2.0.0")).unwrap();
        let error = update_lock(
            dir.path(),
            &manifest::read(dir.path()).unwrap(),
            &lock_path,
            &Update::Keep,
            Lockfile::Default,
            &source(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("pnpm install"));
        assert!(!lock_path.exists());
    }

    #[test]
    fn checks_every_workspace_importer_against_the_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{ "devDependencies": { "shared": "workspace:*" } }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - packages/*\n",
        )
        .unwrap();
        let shared = dir.path().join("packages/shared");
        fs::create_dir_all(&shared).unwrap();
        fs::write(
            shared.join("package.json"),
            r#"{ "name": "shared", "dependencies": { "a": "^1.0.0" } }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join(pnpm::LOCKFILE),
            "lockfileVersion: '9.0'
importers:
  .:
    devDependencies:
      shared:
        specifier: workspace:*
        version: link:packages/shared
  packages/shared:
    dependencies:
      a:
        specifier: ^1.0.0
        version: 1.0.0
packages:
  a@1.0.0:
    resolution: {integrity: sha512-a}
snapshots:
  a@1.0.0: {}
",
        )
        .unwrap();
        let lock_path = dir.path().join(LOCKFILE);

        let (lock, _, file) = update_lock(
            dir.path(),
            &manifest::read(dir.path()).unwrap(),
            &lock_path,
            &Update::Keep,
            Lockfile::Default,
            &source(),
        )
        .unwrap();
        assert_eq!(file, pnpm::LOCKFILE);
        assert_eq!(lock.importers.len(), 2);

        fs::write(
            shared.join("package.json"),
            r#"{ "name": "shared", "dependencies": { "a": "^2.0.0" } }"#,
        )
        .unwrap();
        let error = update_lock(
            dir.path(),
            &manifest::read(dir.path()).unwrap(),
            &lock_path,
            &Update::Keep,
            Lockfile::Default,
            &source(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("pnpm install"));

        fs::remove_file(shared.join("package.json")).unwrap();
        let error = update_lock(
            dir.path(),
            &manifest::read(dir.path()).unwrap(),
            &lock_path,
            &Update::Keep,
            Lockfile::Default,
            &source(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("pnpm install"));
    }

    #[test]
    fn frozen_installs_fail_instead_of_changing_the_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        let lock_path = dir.path().join(LOCKFILE);
        let frozen = || {
            update_lock(
                dir.path(),
                &manifest::read(dir.path()).unwrap(),
                &lock_path,
                &Update::Keep,
                Lockfile::Frozen,
                &source(),
            )
        };

        let error = frozen().unwrap_err();
        assert!(error.to_string().contains("hinata install"), "{error}");
        assert!(!lock_path.exists());

        let compact = r#"{"version":1,"nixpkgs":{"from":"github:NixOS/nixpkgs/nixpkgs-unstable","locked":{"rev":"abc"}},"packages":{},"sccs":[],"importers":{".":{}}}"#;
        fs::write(&lock_path, compact).unwrap();
        assert!(frozen().is_err());
        assert_eq!(fs::read_to_string(&lock_path).unwrap(), compact);

        let pretty = lock::to_json(&serde_json::from_str(compact).unwrap()).unwrap();
        fs::write(&lock_path, pretty).unwrap();
        assert!(frozen().is_ok());
    }

    #[test]
    fn keeps_nixpkgs_locked_until_it_is_updated() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        let lock_path = dir.path().join(LOCKFILE);
        write_unpinned_lock(&lock_path);

        let calls = Cell::new(0);
        let lock_nixpkgs = |from: &str| {
            calls.set(calls.get() + 1);
            Ok(lock::Nixpkgs {
                from: from.to_string(),
                locked: BTreeMap::from([("rev".to_string(), calls.get().into())]),
            })
        };
        let install = |update, lockfile| {
            update_lock(
                dir.path(),
                &manifest::read(dir.path()).unwrap(),
                &lock_path,
                &Update::Keep,
                lockfile,
                &NixpkgsSource {
                    update,
                    lock_nixpkgs: &lock_nixpkgs,
                    ..source()
                },
            )
            .map(|(lock, _, _)| lock.nixpkgs.unwrap().locked["rev"].clone())
        };

        assert!(install(false, Lockfile::Frozen).is_err());
        assert_eq!(install(false, Lockfile::Default).unwrap(), 1);
        assert_eq!(install(false, Lockfile::Frozen).unwrap(), 1);
        assert_eq!(install(true, Lockfile::Default).unwrap(), 2);

        fs::write(
            dir.path().join("package.json"),
            r#"{ "hinata": { "nixpkgs": "github:NixOS/nixpkgs/nixos-25.05" } }"#,
        )
        .unwrap();
        assert!(install(false, Lockfile::Frozen).is_err());
        assert_eq!(install(false, Lockfile::Default).unwrap(), 3);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn follows_nixpkgs_in_flake_lock() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("package.json"), "{}").unwrap();
        let lock_path = dir.path().join(LOCKFILE);
        write_unpinned_lock(&lock_path);

        let flake_lock = |rev: &str| {
            fs::write(
                dir.path().join(nix::FLAKE_LOCK),
                format!(
                    r#"{{ "nodes": {{ "root": {{ "inputs": {{ "nixpkgs": "nixpkgs_2" }} }}, "nixpkgs_2": {{ "locked": {{ "type": "github", "owner": "NixOS", "repo": "nixpkgs", "rev": "{rev}", "narHash": "sha256-x" }} }} }}, "root": "root", "version": 7 }}"#
                ),
            )
            .unwrap();
        };
        let install = |lockfile| {
            update_lock(
                dir.path(),
                &manifest::read(dir.path()).unwrap(),
                &lock_path,
                &Update::Keep,
                lockfile,
                &NixpkgsSource {
                    update: true,
                    ..source()
                },
            )
            .map(|(lock, _, _)| lock.nixpkgs.unwrap().locked["rev"].clone())
        };

        flake_lock("a");
        assert_eq!(install(Lockfile::Default).unwrap(), "a");
        assert_eq!(install(Lockfile::Frozen).unwrap(), "a");

        flake_lock("b");
        assert!(install(Lockfile::Frozen).is_err());
        assert_eq!(install(Lockfile::Default).unwrap(), "b");

        fs::write(
            dir.path().join("package.json"),
            r#"{ "hinata": { "nixpkgs": "github:NixOS/nixpkgs/nixos-25.05" } }"#,
        )
        .unwrap();
        assert_eq!(install(Lockfile::Default).unwrap(), "abc");
    }

    #[test]
    fn picks_node_generously_from_nixpkgs() {
        let available = BTreeMap::from([
            ("nodejs_20".to_string(), "20.19.5".to_string()),
            ("nodejs_22".to_string(), "22.17.1".to_string()),
            ("nodejs_24".to_string(), "24.7.0".to_string()),
        ]);
        let pick = |range: &str| pick_node(range, &available).map(|node| (node.attr, node.version));

        assert_eq!(pick("22").unwrap(), ("nodejs_22".into(), "22.17.1".into()));
        assert_eq!(pick(">=20").unwrap(), ("nodejs_24".into(), "24.7.0".into()));
        assert_eq!(
            pick("^22.18.0").unwrap(),
            ("nodejs_22".into(), "22.17.1".into())
        );

        let error = pick("^18.0.0").unwrap_err();
        assert!(
            error.to_string().contains("20.19.5, 22.17.1, 24.7.0"),
            "{error}"
        );
    }

    #[test]
    fn keeps_node_locked_until_its_range_or_nixpkgs_changes() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join(LOCKFILE);
        let dev_engines = |range: &str, nixpkgs: &str| {
            fs::write(
                dir.path().join("package.json"),
                format!(
                    r#"{{ "devEngines": {{ "runtime": {{ "name": "node", "version": "{range}" }} }}, "hinata": {{ "nixpkgs": "{nixpkgs}" }} }}"#
                ),
            )
            .unwrap();
        };

        let calls = Cell::new(0);
        let node_versions = |_: &lock::Nixpkgs| {
            calls.set(calls.get() + 1);
            Ok(BTreeMap::from([
                ("nodejs_22".to_string(), "22.17.1".to_string()),
                ("nodejs_24".to_string(), "24.7.0".to_string()),
            ]))
        };
        let install = |lockfile| {
            update_lock(
                dir.path(),
                &manifest::read(dir.path()).unwrap(),
                &lock_path,
                &Update::Keep,
                lockfile,
                &NixpkgsSource {
                    node_versions: &node_versions,
                    ..source()
                },
            )
            .map(|(lock, _, _)| lock.node.map(|node| node.attr))
        };

        dev_engines("22", DEFAULT_NIXPKGS);
        assert_eq!(
            install(Lockfile::Default).unwrap().as_deref(),
            Some("nodejs_22")
        );
        assert_eq!(
            install(Lockfile::Frozen).unwrap().as_deref(),
            Some("nodejs_22")
        );
        assert_eq!(calls.get(), 1);

        dev_engines("24", DEFAULT_NIXPKGS);
        assert!(install(Lockfile::Frozen).is_err());
        assert_eq!(
            install(Lockfile::Default).unwrap().as_deref(),
            Some("nodejs_24")
        );
        assert_eq!(calls.get(), 2);

        dev_engines("24", "github:NixOS/nixpkgs/nixos-25.05");
        assert_eq!(
            install(Lockfile::Default).unwrap().as_deref(),
            Some("nodejs_24")
        );
        assert_eq!(calls.get(), 3);
    }

    fn write_unpinned_lock(path: &Path) {
        let unpinned = r#"{"version":1,"packages":{},"sccs":[],"importers":{".":{}}}"#;
        fs::write(
            path,
            lock::to_json(&serde_json::from_str(unpinned).unwrap()).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn prunes_projects_that_no_longer_exist() {
        let cache = tempfile::tempdir().unwrap();
        let live = tempfile::tempdir().unwrap();
        let projects = cache.path().join("projects");
        for (name, path) in [
            ("live", live.path().as_os_str().as_bytes()),
            ("gone", b"/nonexistent/hinata-project".as_slice()),
        ] {
            fs::create_dir_all(projects.join(name)).unwrap();
            fs::write(projects.join(name).join("path"), path).unwrap();
        }
        fs::create_dir_all(projects.join("pending")).unwrap();

        assert_eq!(prune_projects(&projects), 1);
        assert!(projects.join("live").exists());
        assert!(!projects.join("gone").exists());
        assert!(projects.join("pending").exists());
    }
}
