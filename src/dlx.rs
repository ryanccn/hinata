// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::process::ExitStatus;

use eyre::{Result, WrapErr, bail, eyre};
use log::{info, warn};
use node_semver::Version;
use owo_colors::colors::{Blue, Yellow};

use crate::lock::{self, Lock, Specifiers};
use crate::logging::LogDisplay as _;
use crate::manifest::{self, BuildMode};
use crate::registry::{DEFAULT_REGISTRY, HttpRegistry};
use crate::resolve::{self, Project};
use crate::{link, nix, util};

pub fn run(
    dir: &Path,
    spec: &str,
    bin: Option<&str>,
    allow_builds: &BTreeSet<String>,
    nixpkgs: Option<&str>,
    args: &[String],
) -> Result<ExitStatus> {
    let cwd = fs::canonicalize(dir).wrap_err_with(|| format!("opening {}", dir.display()))?;
    let (name, range) = parse_spec(spec)?;

    util::prune_projects(&util::projects_dir()?);
    let lock = resolve_one(&name, &range, allow_builds, nixpkgs)?;

    let run_dir = tempfile::Builder::new()
        .prefix("hinata-dlx-")
        .tempdir()
        .wrap_err("creating a temporary directory")?;
    let project = util::project_dir(run_dir.path())?;
    util::write_atomically(&project.join("path"), run_dir.path().as_os_str().as_bytes())?;

    let status = execute(&cwd, run_dir.path(), &project, &lock, &name, bin, args);

    // The GC root has to outlive the process built against it.
    fs::remove_dir_all(&project).ok();

    status
}

fn resolve_one(
    name: &str,
    range: &str,
    allow_builds: &BTreeSet<String>,
    nixpkgs: Option<&str>,
) -> Result<Lock> {
    let specifiers = Specifiers {
        dependencies: BTreeMap::from([(name.to_string(), range.to_string())]),
        ..Specifiers::default()
    };
    let projects = BTreeMap::from([(
        ".".to_string(),
        Project {
            name: None,
            version: None,
            specifiers,
        },
    )]);

    info!(
        "resolving {} from {}",
        name.log_display::<Blue>(),
        DEFAULT_REGISTRY.log_display::<Blue>()
    );
    let cutoff = resolve::release_cutoff(manifest::DEFAULT_MINIMUM_RELEASE_AGE);
    let registry = HttpRegistry::new(DEFAULT_REGISTRY, cutoff)?;
    let mut lock = resolve::resolve(&projects, &registry, &[], cutoff, &BTreeMap::new())?;
    lock::validate(&lock)?;

    warn_unknown(allow_builds, &lock);
    let modes = allow_builds
        .iter()
        .map(|name| (name.clone(), BuildMode::Sandboxed))
        .collect();
    let skipped = lock::mark_builds(&mut lock, &modes, &BTreeMap::new(), false);
    warn_skipped(&skipped);

    let flake_ref = nixpkgs.unwrap_or(nix::DEFAULT_NIXPKGS);
    info!("locking Nixpkgs from {}", flake_ref.log_display::<Blue>());
    let nixpkgs = nix::lock_nixpkgs(flake_ref)?;
    lock.node = Some(newest_node(&nix::node_versions(&nixpkgs)?)?);
    lock.nixpkgs = Some(nixpkgs);

    Ok(lock)
}

fn execute(
    cwd: &Path,
    run_dir: &Path,
    project: &Path,
    lock: &Lock,
    name: &str,
    bin: Option<&str>,
    args: &[String],
) -> Result<ExitStatus> {
    let workspace = nix::build_workspace(
        run_dir,
        lock,
        &lock::to_json(lock)?,
        false,
        None,
        &BTreeMap::new(),
        &project.join("gcroot"),
    )?;

    let node =
        fs::canonicalize(workspace.join("node")).wrap_err("finding Node.js in the Nix build")?;
    let roots = nix::importer_roots(&workspace)?;
    let packages = &roots
        .get(".")
        .ok_or_else(|| eyre!("the Nix build has no packages"))?
        .dependencies;

    let node_modules = run_dir.join("node_modules");
    link::sync(&node_modules, packages, Some(node.as_path()))?;

    let bins = node_modules.join(".bin");
    let chosen = choose_bin(name, &list_bins(&bins)?, bin)?;
    info!(
        "running {} from {}",
        chosen.log_display::<Blue>(),
        resolved(lock, name).log_display::<Blue>()
    );

    crate::run::command(cwd, &bins, &chosen)?
        .args(args)
        .status()
        .wrap_err_with(|| format!("running {chosen}"))
}

fn resolved(lock: &Lock, name: &str) -> String {
    let version = lock
        .importers
        .get(".")
        .and_then(|importer| importer.dependencies.get(name))
        .and_then(|id| lock.packages.get(id))
        .map(|package| package.version.as_str());

    match version {
        Some(version) => format!("{name}@{version}"),
        None => name.to_string(),
    }
}

fn parse_spec(spec: &str) -> Result<(String, String)> {
    let (name, range) = resolve::split_version(spec).unwrap_or((spec, ""));
    if !lock::is_valid_name(name) {
        bail!("{spec:?} is not a package name, version, range or tag");
    }

    resolve::parse_spec(name, range)
}

fn choose_bin(package: &str, bins: &BTreeSet<String>, chosen: Option<&str>) -> Result<String> {
    if let Some(chosen) = chosen {
        if bins.contains(chosen) {
            return Ok(chosen.to_string());
        }
        bail!(
            "{package} has no binary named {chosen} (it has {})",
            offered(bins)
        );
    }

    let named = package.rsplit('/').next().unwrap_or(package);
    if bins.contains(named) {
        return Ok(named.to_string());
    }

    let mut entries = bins.iter();
    match (entries.next(), entries.next()) {
        (Some(only), None) => Ok(only.clone()),
        (None, _) => bail!("{package} has no binaries to run"),
        (Some(_), Some(_)) => bail!(
            "{package} has several binaries; choose one with --bin (it has {})",
            offered(bins)
        ),
    }
}

fn offered(bins: &BTreeSet<String>) -> String {
    bins.iter()
        .map(|bin| bin.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `.bin/node` is linked by hinata rather than provided by the package.
fn list_bins(dir: &Path) -> Result<BTreeSet<String>> {
    let mut bins = BTreeSet::new();
    for entry in fs::read_dir(dir).wrap_err_with(|| format!("reading {}", dir.display()))? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name != "node" {
            bins.insert(name);
        }
    }

    Ok(bins)
}

fn newest_node(versions: &BTreeMap<String, String>) -> Result<lock::Node> {
    let (attr, version) = versions
        .iter()
        .filter_map(|(attr, version)| Some((attr, Version::parse(version).ok()?)))
        .max_by(|(_, a), (_, b)| a.cmp(b))
        .ok_or_else(|| eyre!("the locked Nixpkgs has no Node.js"))?;

    Ok(lock::Node {
        from: "*".to_string(),
        attr: attr.clone(),
        version: version.to_string(),
    })
}

fn warn_unknown(allow_builds: &BTreeSet<String>, lock: &Lock) {
    for name in lock::missing_names(lock, allow_builds) {
        warn!(
            "{} is not installed, so there is nothing to build",
            name.log_display::<Yellow>()
        );
    }
}

fn warn_skipped(skipped: &BTreeSet<String>) {
    if skipped.is_empty() {
        return;
    }

    let names: Vec<_> = skipped
        .iter()
        .map(|name| name.log_display::<Yellow>().to_string())
        .collect();
    warn!(
        "skipped install scripts of {}; pass {} to run them",
        names.join(", "),
        "--allow-build".log_display::<Blue>()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bins(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn chooses_the_binary_to_run() {
        let choose = |package, names: &[&str], chosen| choose_bin(package, &bins(names), chosen);

        assert_eq!(
            choose("cowsay", &["cowsay", "cowthink"], None).unwrap(),
            "cowsay"
        );
        assert_eq!(
            choose("@scope/tool", &["tool", "other"], None).unwrap(),
            "tool"
        );
        assert_eq!(choose("@babel/cli", &["babel"], None).unwrap(), "babel");
        assert_eq!(
            choose("typescript", &["tsc", "tsserver"], Some("tsserver")).unwrap(),
            "tsserver"
        );
    }

    #[test]
    fn refuses_to_guess_a_binary() {
        let error = |package, names: &[&str], chosen| {
            choose_bin(package, &bins(names), chosen)
                .unwrap_err()
                .to_string()
        };

        let several = error("typescript", &["tsc", "tsserver"], None);
        assert!(
            several.contains("tsc") && several.contains("tsserver"),
            "{several}"
        );
        assert!(several.contains("--bin"), "{several}");

        let missing = error("typescript", &["tsc"], Some("tsx"));
        assert!(
            missing.contains("tsx") && missing.contains("tsc"),
            "{missing}"
        );

        assert!(error("left-pad", &[], None).contains("left-pad"));
    }

    #[test]
    fn parses_package_specs() {
        let parsed = |spec| parse_spec(spec).unwrap();

        assert_eq!(parsed("cowsay"), ("cowsay".to_string(), "*".to_string()));
        assert_eq!(
            parsed("cowsay@1.5.0"),
            ("cowsay".to_string(), "1.5.0".to_string())
        );
        assert_eq!(
            parsed("typescript@next"),
            ("typescript".to_string(), "next".to_string())
        );
        assert_eq!(
            parsed("@scope/pkg@^2"),
            ("@scope/pkg".to_string(), "^2".to_string())
        );

        for spec in ["github:user/repo", "x@github:user/repo", "./local", ""] {
            assert!(parse_spec(spec).is_err(), "{spec:?}");
        }
    }

    #[test]
    fn picks_the_newest_node_in_nixpkgs() {
        let versions = BTreeMap::from([
            ("nodejs_20".to_string(), "20.19.0".to_string()),
            ("nodejs_22".to_string(), "22.14.0".to_string()),
            ("nodejs_9".to_string(), "9.11.2".to_string()),
            ("nodejs_slim".to_string(), "unstable".to_string()),
        ]);

        let node = newest_node(&versions).unwrap();

        assert_eq!(node.attr, "nodejs_22");
        assert_eq!(node.version, "22.14.0");
        assert!(newest_node(&BTreeMap::new()).is_err());
    }
}
