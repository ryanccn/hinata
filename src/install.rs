// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use eyre::{Result, WrapErr, bail, eyre};
use log::{debug, info, warn};
use owo_colors::OwoColorize as _;
use owo_colors::colors::{Blue, Yellow};
use serde::Deserialize;

use crate::lock::{self, Lock};
use crate::logging::{LogDisplay as _, plural};
use crate::manifest::{BuildMode, Manifest};
use crate::registry::{DEFAULT_REGISTRY, HttpRegistry};
use crate::resolve::Project;
use crate::{impure, link, manifest, nix, pnpm, resolve};

const LOCKFILE: &str = "hinata.lock";

pub struct Options {
    pub dir: PathBuf,
    pub dev: bool,
    pub refresh: bool,
    pub update: Update,
    pub save_lock: bool,
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
    let lock_path = root.join(LOCKFILE);
    let (lock, lock_json, lock_file) = update_lock(&root, &lock_path, options)?;

    let cache = cache_dir()?;
    let gcroots = cache.join("gcroots");
    let keys = cache.join("keys");
    for dir in [&gcroots, &keys] {
        fs::create_dir_all(dir).wrap_err_with(|| format!("creating {}", dir.display()))?;
        prune_cache(dir);
    }
    let name = escape_path(&root);
    let gcroot = gcroots.join(&name);
    let key_path = keys.join(&name);

    let node = native_addon_node(&root, &lock);
    let key = cache_key(&lock_json, options.dev, node);

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
            let workspace = nix::build_workspace(&lock_json, options.dev, node, &gcroot)?;
            debug!(
                "built workspace {}",
                workspace.display().log_display::<Blue>()
            );
            (workspace, true)
        }
    };

    link_importers(&root, &lock, &workspace)?;

    if built {
        let impure_builds: BTreeMap<String, PathBuf> =
            serde_json::from_str(&fs::read_to_string(workspace.join("impure-builds.json"))?)?;
        for (id, dir) in &impure_builds {
            let package = &lock.packages[id];
            impure::run(&root, &package.name, &package.version, dir)?;
        }
        // Written last, so that failed impure builds run again on the next install.
        fs::write(&key_path, &key).wrap_err_with(|| format!("writing {}", key_path.display()))?;
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

fn native_addon_node(root: &Path, lock: &Lock) -> Option<u32> {
    if !lock.packages.values().any(|package| package.build) {
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

fn link_importers(root: &Path, lock: &Lock, workspace: &Path) -> Result<()> {
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
        link::sync(&node_modules, &packages)
            .wrap_err_with(|| format!("linking {}", node_modules.display()))?;
    }
    Ok(())
}

fn update_lock(
    root: &Path,
    lock_path: &Path,
    options: &Options,
) -> Result<(Lock, String, &'static str)> {
    let manifest = manifest::read(root)?;
    let projects = read_projects(root, &manifest)?;
    let existing = manifest::read_if_exists(lock_path)
        .wrap_err("reading hinata.lock")?
        .map(|json| serde_json::from_str::<Lock>(&json))
        .transpose()
        .wrap_err("parsing hinata.lock")?;
    let pnpm_lock = match existing {
        Some(_) => None,
        None => pnpm::read(root)?,
    };
    let from_pnpm = pnpm_lock.is_some();
    if let Update::Only(names) = &options.update
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
    let keep = matches!(options.update, Update::Keep);
    let (mut lock, pnpm_only) = match (existing, pnpm_lock) {
        (Some(lock), _) if keep && current(&lock) => {
            debug!(
                "{} matches package.json, not resolving again",
                LOCKFILE.log_display::<Blue>()
            );
            (lock, false)
        }
        (None, Some(mut lock)) if keep && current(&lock) => {
            if options.save_lock {
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
        (None, Some(_)) if !options.save_lock => bail!(
            "{} does not match package.json; run `pnpm install` to update it, or `hinata install --save-lock` to resolve dependencies into {} instead",
            pnpm::LOCKFILE,
            LOCKFILE
        ),
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
                        .filter(|package| !options.update.includes(&package.name))
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

    mark_builds(&mut lock, &manifest.allow_builds(), pnpm_only);
    let json = lock::to_json(&lock)?;
    if pnpm_only {
        return Ok((lock, json, pnpm::LOCKFILE));
    }
    write_lock(lock_path, &json, from_pnpm)?;
    Ok((lock, json, LOCKFILE))
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

fn mark_builds(lock: &mut Lock, allow_builds: &BTreeMap<String, BuildMode>, pnpm_only: bool) {
    let mut skipped = BTreeSet::new();
    for package in lock.packages.values_mut() {
        // pnpm lockfiles do not record which packages have install scripts.
        let mode = allow_builds
            .get(&package.name)
            .filter(|_| pnpm_only || package.install_script);
        package.build = mode == Some(&BuildMode::Sandboxed);
        package.impure_build = mode == Some(&BuildMode::Impure);
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

/// `DefaultHasher` output is only stable within one build of hinata.
fn cache_key(lock_json: &str, dev: bool, node_major: Option<u32>) -> String {
    let mut hasher = DefaultHasher::new();
    (lock_json, dev, node_major, nix::LIBRARY).hash(&mut hasher);
    format!("{:016x}", hasher.finish())
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

fn escape_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('%', "%25")
        .replace('/', "%2F")
}

fn unescape_path(name: &str) -> PathBuf {
    PathBuf::from(name.replace("%2F", "/").replace("%25", "%"))
}

fn prune_cache(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let project = unescape_path(&entry.file_name().to_string_lossy());
        if !project.exists() && fs::remove_file(entry.path()).is_ok() {
            debug!(
                "removed {} of {}, which no longer exists",
                entry.path().display().log_display::<Blue>(),
                project.display().log_display::<Blue>()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installs_from_a_matching_pnpm_lockfile() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = |range: &str| {
            format!(
                r#"{{ "dependencies": {{ "a": "{range}", "b": "^1.0.0" }}, "devDependencies": {{ "node": "runtime:22.18.0" }}, "hinata": {{ "allowBuilds": {{ "a": true, "b": "impure" }} }} }}"#
            )
        };
        fs::write(dir.path().join("package.json"), manifest("^1.0.0")).unwrap();
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
        let options = Options {
            dir: dir.path().to_path_buf(),
            dev: true,
            refresh: false,
            update: Update::Keep,
            save_lock: false,
        };
        let lock_path = dir.path().join(LOCKFILE);

        let (lock, _, file) = update_lock(dir.path(), &lock_path, &options).unwrap();
        assert_eq!(file, pnpm::LOCKFILE);
        assert!(lock.packages["a@1.0.0"].build);
        assert!(!lock.packages["a@1.0.0"].impure_build);
        assert!(!lock.packages["b@1.0.0"].build);
        assert!(lock.packages["b@1.0.0"].impure_build);
        assert!(!lock_path.exists());

        fs::write(dir.path().join("package.json"), manifest("^2.0.0")).unwrap();
        let error = update_lock(dir.path(), &lock_path, &options).unwrap_err();
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
        let options = Options {
            dir: dir.path().to_path_buf(),
            dev: true,
            refresh: false,
            update: Update::Keep,
            save_lock: false,
        };
        let lock_path = dir.path().join(LOCKFILE);

        let (lock, _, file) = update_lock(dir.path(), &lock_path, &options).unwrap();
        assert_eq!(file, pnpm::LOCKFILE);
        assert_eq!(lock.importers.len(), 2);

        fs::write(
            shared.join("package.json"),
            r#"{ "name": "shared", "dependencies": { "a": "^2.0.0" } }"#,
        )
        .unwrap();
        let error = update_lock(dir.path(), &lock_path, &options).unwrap_err();
        assert!(error.to_string().contains("pnpm install"));

        fs::remove_file(shared.join("package.json")).unwrap();
        let error = update_lock(dir.path(), &lock_path, &options).unwrap_err();
        assert!(error.to_string().contains("pnpm install"));
    }

    #[test]
    fn escapes_paths_reversibly() {
        for path in ["/Users/me/app", "/tmp/100%/a%2Fb", "/"] {
            let name = escape_path(Path::new(path));
            assert!(!name.contains('/'));
            assert_eq!(unescape_path(&name), PathBuf::from(path));
        }
    }
}
