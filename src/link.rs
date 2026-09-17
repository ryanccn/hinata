// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use serde_json::Value;

/// Makes `node_modules` a real directory of links to `packages` (alias to package directory) and
/// their binaries. Links that already point at the right target are left in place, and
/// dot-directories that are not links are kept.
pub fn sync(
    node_modules: &Path,
    packages: &BTreeMap<String, PathBuf>,
    node: Option<&Path>,
) -> Result<()> {
    let mut wanted = bins(packages)?;
    if let Some(node) = node {
        wanted.insert(".bin/node".to_string(), node.to_path_buf());
    }
    wanted.extend(
        packages
            .iter()
            .map(|(alias, root)| (alias.clone(), root.clone())),
    );

    match fs::symlink_metadata(node_modules) {
        Ok(metadata) if metadata.is_dir() => prune(node_modules, "", &wanted)?,
        Ok(_) => {
            fs::remove_file(node_modules)?;
            fs::create_dir(node_modules)?;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => fs::create_dir_all(node_modules)?,
        Err(error) => {
            return Err(error).wrap_err_with(|| format!("reading {}", node_modules.display()));
        }
    }

    for (name, target) in &wanted {
        let link = node_modules.join(name);
        if fs::read_link(&link).is_ok_and(|current| current == *target) {
            continue;
        }
        if let Some(parent) = link.parent() {
            fs::create_dir_all(parent)?;
        }
        symlink(target, &link).wrap_err_with(|| format!("linking {}", link.display()))?;
    }
    Ok(())
}

fn bins(packages: &BTreeMap<String, PathBuf>) -> Result<BTreeMap<String, PathBuf>> {
    let mut bins = BTreeMap::new();
    for (alias, root) in packages {
        let path = root.join("package.json");
        let Ok(source) = fs::read_to_string(&path) else {
            continue;
        };
        let manifest: Value = serde_json::from_str(&source)
            .wrap_err_with(|| format!("parsing {}", path.display()))?;
        let name = manifest
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(alias.as_str());
        let entries: Vec<(&str, &str)> = match manifest.get("bin") {
            Some(Value::String(file)) => vec![(name, file.as_str())],
            Some(Value::Object(files)) => files
                .iter()
                .filter_map(|(bin, file)| Some((bin.as_str(), file.as_str()?)))
                .collect(),
            _ => Vec::new(),
        };
        for (bin, file) in entries {
            let bin = bin.rsplit('/').next().unwrap_or(bin);
            bins.insert(
                format!(".bin/{bin}"),
                root.join(file.trim_start_matches("./")),
            );
        }
    }
    Ok(bins)
}

fn is_container(name: &str) -> bool {
    name.starts_with('@') || name == ".bin"
}

fn prune(dir: &Path, prefix: &str, wanted: &BTreeMap<String, PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let key = format!("{prefix}{name}");
        let path = entry.path();
        let file_type = entry.file_type()?;

        if file_type.is_symlink() {
            if wanted.get(&key) != fs::read_link(&path).ok().as_ref() {
                fs::remove_file(&path)?;
            }
        } else if file_type.is_dir() && prefix.is_empty() && is_container(&name) {
            prune(&path, &format!("{key}/"), wanted)?;
            if fs::read_dir(&path)?.next().is_none() {
                fs::remove_dir(&path)?;
            }
        } else if wanted.contains_key(&key) || !name.starts_with('.') {
            if file_type.is_dir() {
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    fn package(root: &Path, name: &str, manifest: &str) -> PathBuf {
        let dir = root
            .join("store")
            .join(name)
            .join("node_modules")
            .join(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("package.json"), manifest).unwrap();
        dir
    }

    fn packages(entries: &[(&str, &PathBuf)]) -> BTreeMap<String, PathBuf> {
        entries
            .iter()
            .map(|(alias, root)| (alias.to_string(), (*root).clone()))
            .collect()
    }

    #[test]
    fn links_packages_scopes_and_bins() {
        let root = tempfile::tempdir().unwrap();
        let react = package(root.path(), "react", r#"{ "name": "react" }"#);
        let core = package(root.path(), "@babel/core", r#"{ "name": "@babel/core" }"#);
        let vite = package(
            root.path(),
            "vite",
            r#"{ "name": "vite", "bin": { "vite": "./bin/vite.js" } }"#,
        );
        let tsc = package(
            root.path(),
            "typescript",
            r#"{ "name": "typescript", "bin": "bin/tsc" }"#,
        );
        let dest = root.path().join("app/node_modules");

        let node = root.path().join("nodejs/bin/node");
        sync(
            &dest,
            &packages(&[
                ("react", &react),
                ("@babel/core", &core),
                ("vite", &vite),
                ("typescript", &tsc),
            ]),
            Some(&node),
        )
        .unwrap();

        assert_eq!(fs::read_link(dest.join("react")).unwrap(), react);
        assert!(fs::symlink_metadata(dest.join("@babel")).unwrap().is_dir());
        assert_eq!(fs::read_link(dest.join("@babel/core")).unwrap(), core);
        assert_eq!(
            fs::read_link(dest.join(".bin/vite")).unwrap(),
            vite.join("bin/vite.js")
        );
        assert_eq!(
            fs::read_link(dest.join(".bin/typescript")).unwrap(),
            tsc.join("bin/tsc")
        );
        assert_eq!(fs::read_link(dest.join(".bin/node")).unwrap(), node);
    }

    #[test]
    fn leaves_unchanged_links_in_place() {
        let root = tempfile::tempdir().unwrap();
        let react = package(root.path(), "react", r#"{ "name": "react" }"#);
        let vite = package(
            root.path(),
            "vite",
            r#"{ "name": "vite", "bin": { "vite": "bin/vite.js" } }"#,
        );
        let zod = package(root.path(), "zod", r#"{ "name": "zod" }"#);
        let dest = root.path().join("node_modules");
        let inode = |name: &str| fs::symlink_metadata(dest.join(name)).unwrap().ino();

        sync(
            &dest,
            &packages(&[("react", &react), ("vite", &vite)]),
            None,
        )
        .unwrap();
        let (react_inode, bin_inode) = (inode("react"), inode(".bin/vite"));
        sync(
            &dest,
            &packages(&[("react", &react), ("vite", &vite), ("zod", &zod)]),
            None,
        )
        .unwrap();

        assert_eq!(inode("react"), react_inode);
        assert_eq!(inode(".bin/vite"), bin_inode);
        assert_eq!(fs::read_link(dest.join("zod")).unwrap(), zod);
    }

    #[test]
    fn keeps_caches_and_removes_stale_entries() {
        let root = tempfile::tempdir().unwrap();
        let react = package(root.path(), "react", r#"{ "name": "react" }"#);
        let dest = root.path().join("node_modules");
        fs::create_dir_all(dest.join(".vite/deps")).unwrap();
        fs::create_dir_all(dest.join("lodash")).unwrap();
        fs::create_dir_all(dest.join("@old")).unwrap();
        symlink("/nix/store/old", dest.join("@old/pkg")).unwrap();
        symlink("/nix/store/old", dest.join("left-pad")).unwrap();
        symlink("/nix/store/wrong", dest.join("react")).unwrap();

        sync(&dest, &packages(&[("react", &react)]), None).unwrap();

        assert!(dest.join(".vite/deps").is_dir());
        assert!(!dest.join("lodash").exists());
        assert!(fs::symlink_metadata(dest.join("@old")).is_err());
        assert!(fs::symlink_metadata(dest.join("left-pad")).is_err());
        assert_eq!(fs::read_link(dest.join("react")).unwrap(), react);
    }

    #[test]
    fn replaces_a_symlinked_node_modules() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = root.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        let dest = root.path().join("node_modules");
        symlink(&elsewhere, &dest).unwrap();

        sync(&dest, &BTreeMap::new(), None).unwrap();

        assert!(fs::symlink_metadata(&dest).unwrap().is_dir());
        assert!(elsewhere.is_dir());
    }
}
