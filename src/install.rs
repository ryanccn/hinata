// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

use eyre::{Result, WrapErr, eyre};
use log::{debug, info, warn};
use owo_colors::OwoColorize as _;
use owo_colors::colors::{Blue, Yellow};
use serde::Deserialize;

use crate::lock::{self, Lock};
use crate::logging::{LogDisplay as _, plural};
use crate::registry::{DEFAULT_REGISTRY, HttpRegistry};
use crate::{link, manifest, nix, resolve};

pub struct Options {
    pub dir: PathBuf,
    pub dev: bool,
    pub refresh: bool,
    pub update: Update,
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
    let lock_path = root.join("hinata.lock");
    let (lock, lock_json) = update_lock(&root, &lock_path, options)?;

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

    // Native addons only load in the Node.js major version they were built for.
    let builds = lock.packages.values().any(|package| package.build);
    let node = builds.then(|| node_major(&root)).flatten();
    match node {
        Some(major) => debug!(
            "building native addons for Node.js {}",
            major.log_display::<Blue>()
        ),
        None if builds => warn!(
            "found no working {} on PATH, so native addons will be built for nixpkgs' default Node.js",
            "node".log_display::<Yellow>()
        ),
        None => {}
    }
    let key = cache_key(&lock_json, options.dev, node);

    let workspace = match cached_workspace(&key_path, &gcroot, &key) {
        Some(workspace) if !options.refresh => {
            info!(
                "{} is unchanged since the last install, skipping the Nix build {}",
                "hinata.lock".log_display::<Blue>(),
                "(pass --refresh to rebuild)".dimmed()
            );
            workspace
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
            match fs::remove_file(&key_path) {
                Err(error) if error.kind() != ErrorKind::NotFound => {
                    return Err(error).wrap_err_with(|| format!("removing {}", key_path.display()));
                }
                _ => {}
            }
            let workspace = nix::build_workspace(&lock_json, options.dev, node, &gcroot)?;
            debug!(
                "built workspace {}",
                workspace.display().log_display::<Blue>()
            );
            fs::write(&key_path, &key)
                .wrap_err_with(|| format!("writing {}", key_path.display()))?;
            workspace
        }
    };

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

fn update_lock(root: &Path, lock_path: &Path, options: &Options) -> Result<(Lock, String)> {
    let manifest = manifest::read(root)?;
    let existing = match fs::read_to_string(lock_path) {
        Ok(json) => Some(serde_json::from_str::<Lock>(&json).wrap_err("parsing hinata.lock")?),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error).wrap_err("reading hinata.lock"),
    };

    if let (Update::Only(names), Some(lock)) = (&options.update, &existing) {
        for name in names {
            if !lock.packages.values().any(|package| &package.name == name) {
                warn!(
                    "{} is not installed, so there is nothing to update",
                    name.log_display::<Yellow>()
                );
            }
        }
    }

    let current = |lock: &Lock| {
        lock.version == lock::VERSION
            && lock
                .importers
                .get(".")
                .is_some_and(|root| root.specifiers == manifest.specifiers)
    };
    let mut lock = match existing {
        Some(lock) if matches!(options.update, Update::Keep) && current(&lock) => {
            debug!(
                "{} matches package.json, not resolving again",
                "hinata.lock".log_display::<Blue>()
            );
            lock
        }
        existing => {
            info!(
                "resolving dependencies from {}",
                DEFAULT_REGISTRY.log_display::<Blue>()
            );
            let preferred: Vec<(String, String)> = existing
                .map(|lock| {
                    lock.packages
                        .into_values()
                        .filter(|package| !options.update.includes(&package.name))
                        .map(|package| (package.name, package.version))
                        .collect()
                })
                .unwrap_or_default();
            let registry = HttpRegistry::new(DEFAULT_REGISTRY)?;
            let lock = resolve::resolve(&manifest.specifiers, &registry, &preferred)?;
            info!(
                "resolved {}",
                plural(lock.packages.len(), "package", "packages")
            );
            lock
        }
    };

    let allow_builds = manifest.allow_builds();
    let mut skipped = BTreeSet::new();
    for package in lock.packages.values_mut() {
        package.build = package.install_script && allow_builds.contains(&package.name);
        if package.install_script && !package.build {
            skipped.insert(package.name.clone());
        }
    }
    if !skipped.is_empty() {
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

    let json = lock::to_json(&lock)?;
    let previous = fs::read_to_string(lock_path).ok();
    if previous.as_deref() != Some(json.as_str()) {
        fs::write(lock_path, &json).wrap_err("writing hinata.lock")?;
        info!(
            "{} {}",
            if previous.is_some() {
                "updated"
            } else {
                "created"
            },
            "hinata.lock".log_display::<Blue>()
        );
    }
    Ok((lock, json))
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
fn cache_dir() -> Result<PathBuf> {
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
    fn escapes_paths_reversibly() {
        for path in ["/Users/me/app", "/tmp/100%/a%2Fb", "/"] {
            let name = escape_path(Path::new(path));
            assert!(!name.contains('/'));
            assert_eq!(unescape_path(&name), PathBuf::from(path));
        }
    }
}
