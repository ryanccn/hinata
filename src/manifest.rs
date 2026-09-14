// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use eyre::{Result, WrapErr, bail};
use serde::{Deserialize, Serialize};
use serde_json::ser::PrettyFormatter;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::install::hex;
use crate::lock::{Patch, Specifiers};
use crate::resolve::Project;

#[derive(Deserialize)]
pub struct Manifest {
    pub name: Option<String>,
    pub version: Option<String>,
    #[serde(flatten)]
    pub specifiers: Specifiers,
    #[serde(default)]
    pub scripts: BTreeMap<String, String>,
    #[serde(default)]
    hinata: HinataConfig,
    #[serde(skip)]
    workspace: PnpmWorkspace,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct HinataConfig {
    #[serde(default)]
    allow_builds: AllowBuilds,
    #[serde(default)]
    build_inputs: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    patched_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    substituters: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AllowBuilds {
    Names(BTreeSet<String>),
    Map(BTreeMap<String, AllowBuild>),
}

impl Default for AllowBuilds {
    fn default() -> Self {
        AllowBuilds::Names(BTreeSet::new())
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AllowBuild {
    Allowed(bool),
    Mode(BuildMode),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuildMode {
    #[serde(skip)]
    Sandboxed,
    /// Runs after linking, outside the Nix sandbox, so that the scripts can download things.
    Impure,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PnpmWorkspace {
    #[serde(default)]
    packages: Vec<String>,
    #[serde(default)]
    allow_builds: BTreeMap<String, bool>,
    #[serde(default)]
    build_inputs: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    patched_dependencies: BTreeMap<String, String>,
}

impl Manifest {
    pub fn project(&self) -> Project {
        Project {
            name: self.name.clone(),
            version: self.version.clone(),
            specifiers: self.specifiers.clone(),
        }
    }

    /// Directories matched by `packages` in `pnpm-workspace.yaml`, as `/`-separated paths relative
    /// to `root`.
    pub fn workspace_packages(&self, root: &Path) -> Result<BTreeSet<String>> {
        let mut include = Vec::new();
        let mut exclude = Vec::new();
        for pattern in &self.workspace.packages {
            match pattern.strip_prefix('!') {
                Some(excluded) => exclude.push(pattern_segments(excluded)),
                None => include.push(pattern_segments(pattern)),
            }
        }

        let mut found = BTreeSet::new();
        if !include.is_empty() {
            find_packages(root, &mut Vec::new(), &include, &exclude, &mut found)?;
        }
        Ok(found)
    }

    pub fn allow_builds(&self) -> BTreeMap<String, BuildMode> {
        let mut builds: BTreeMap<String, BuildMode> = self
            .workspace
            .allow_builds
            .iter()
            .filter(|(_, allowed)| **allowed)
            .map(|(name, _)| (name.clone(), BuildMode::Sandboxed))
            .collect();
        match &self.hinata.allow_builds {
            AllowBuilds::Names(names) => {
                builds.extend(
                    names
                        .iter()
                        .map(|name| (name.clone(), BuildMode::Sandboxed)),
                );
            }
            AllowBuilds::Map(entries) => {
                for (name, entry) in entries {
                    match entry {
                        AllowBuild::Allowed(true) => {
                            builds.insert(name.clone(), BuildMode::Sandboxed);
                        }
                        AllowBuild::Allowed(false) => {
                            builds.remove(name);
                        }
                        AllowBuild::Mode(mode) => {
                            builds.insert(name.clone(), *mode);
                        }
                    }
                }
            }
        }
        builds
    }

    /// Nixpkgs attribute paths added to the sandboxed builds of each package.
    pub fn build_inputs(&self) -> Result<BTreeMap<String, Vec<String>>> {
        let mut inputs = self.workspace.build_inputs.clone();
        inputs.extend(self.hinata.build_inputs.clone());
        inputs.retain(|_, attrs| !attrs.is_empty());

        let allow_builds = self.allow_builds();
        for name in inputs.keys() {
            match allow_builds.get(name) {
                Some(BuildMode::Sandboxed) => {}
                Some(BuildMode::Impure) => bail!(
                    "{name} has build inputs, but its install scripts run outside the Nix sandbox, where build inputs do not apply"
                ),
                None => bail!(
                    "{name} has build inputs, but its install scripts are not allowed to run; add it to hinata.allowBuilds"
                ),
            }
        }

        Ok(inputs)
    }

    /// Keyed by `name@version` or `name`.
    pub fn patches(&self, root: &Path) -> Result<BTreeMap<String, Patch>> {
        let mut paths = self.workspace.patched_dependencies.clone();
        paths.extend(self.hinata.patched_dependencies.clone());

        paths
            .into_iter()
            .map(|(key, path)| {
                let inside = Path::new(&path)
                    .components()
                    .all(|component| matches!(component, Component::Normal(_) | Component::CurDir));
                if !inside {
                    bail!("the patch {path} for {key} must be a relative path inside the project");
                }

                let file = root.join(&path);
                let contents =
                    fs::read(&file).wrap_err_with(|| format!("reading {}", file.display()))?;
                let hash = hex(Sha256::digest(contents));

                Ok((key, Patch { path, hash }))
            })
            .collect()
    }

    /// Binary cache URLs and their public keys.
    pub fn substituters(&self) -> &BTreeMap<String, String> {
        &self.hinata.substituters
    }
}

fn pattern_segments(pattern: &str) -> Vec<&str> {
    pattern
        .trim()
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect()
}

fn find_packages(
    root: &Path,
    path: &mut Vec<String>,
    include: &[Vec<&str>],
    exclude: &[Vec<&str>],
    found: &mut BTreeSet<String>,
) -> Result<()> {
    let dir = root.join(path.join("/"));
    let entries = fs::read_dir(&dir).wrap_err_with(|| format!("reading {}", dir.display()))?;
    for entry in entries {
        let entry = entry.wrap_err_with(|| format!("reading {}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "node_modules" || name.starts_with('.') || !entry.file_type()?.is_dir() {
            continue;
        }
        path.push(name);
        let segments: Vec<&str> = path.iter().map(String::as_str).collect();
        let descend = any_matches(include, &segments, true);
        let matched = descend
            && any_matches(include, &segments, false)
            && !any_matches(exclude, &segments, false);
        if matched && entry.path().join("package.json").is_file() {
            found.insert(path.join("/"));
        }
        if descend {
            find_packages(root, path, include, exclude, found)?;
        }
        path.pop();
    }
    Ok(())
}

fn any_matches(patterns: &[Vec<&str>], path: &[&str], prefix: bool) -> bool {
    patterns
        .iter()
        .any(|pattern| matches_path(pattern, path, prefix))
}

/// With `prefix`, also matches paths that a longer path below them could match.
fn matches_path(pattern: &[&str], path: &[&str], prefix: bool) -> bool {
    match (pattern.split_first(), path.split_first()) {
        (Some((&"**", rest)), _) => {
            matches_path(rest, path, prefix)
                || path
                    .split_first()
                    .is_some_and(|(_, below)| matches_path(pattern, below, prefix))
        }
        (Some((segment, rest)), Some((name, below))) => {
            matches_segment(segment.as_bytes(), name.as_bytes())
                && matches_path(rest, below, prefix)
        }
        (Some(_), None) => prefix,
        (None, rest) => rest.is_none(),
    }
}

fn matches_segment(pattern: &[u8], name: &[u8]) -> bool {
    match pattern.split_first() {
        None => name.is_empty(),
        Some((b'*', rest)) => (0..=name.len()).any(|at| matches_segment(rest, &name[at..])),
        Some((b'?', rest)) => !name.is_empty() && matches_segment(rest, &name[1..]),
        Some((byte, rest)) => name.first() == Some(byte) && matches_segment(rest, &name[1..]),
    }
}

pub fn read(dir: &Path) -> Result<Manifest> {
    let path = dir.join("package.json");
    let source =
        fs::read_to_string(&path).wrap_err_with(|| format!("reading {}", path.display()))?;
    let mut manifest: Manifest =
        serde_json::from_str(&source).wrap_err_with(|| format!("parsing {}", path.display()))?;

    let path = dir.join("pnpm-workspace.yaml");
    if let Some(source) =
        read_if_exists(&path).wrap_err_with(|| format!("reading {}", path.display()))?
    {
        manifest.workspace = serde_yaml::from_str(&source)
            .wrap_err_with(|| format!("parsing {}", path.display()))?;
    }
    Ok(manifest)
}

pub fn read_if_exists(path: &Path) -> std::io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(source) => Ok(Some(source)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Prod,
    Dev,
    Optional,
}

impl Group {
    pub const ALL: [Group; 3] = [Group::Prod, Group::Dev, Group::Optional];

    pub fn key(self) -> &'static str {
        match self {
            Group::Prod => "dependencies",
            Group::Dev => "devDependencies",
            Group::Optional => "optionalDependencies",
        }
    }
}

pub struct Document {
    path: PathBuf,
    original: String,
    value: Value,
    indent: String,
}

impl Document {
    pub fn open(dir: &Path) -> Result<Document> {
        let path = dir.join("package.json");
        let original =
            fs::read_to_string(&path).wrap_err_with(|| format!("reading {}", path.display()))?;
        let value: Value = serde_json::from_str(&original)
            .wrap_err_with(|| format!("parsing {}", path.display()))?;
        if !value.is_object() {
            bail!("{} is not a JSON object", path.display());
        }
        Ok(Document {
            indent: detect_indent(&original),
            path,
            original,
            value,
        })
    }

    pub fn set_dependency(&mut self, group: Group, alias: &str, spec: &str) {
        self.remove_dependency(alias);
        let deps = self
            .object()
            .entry(group.key())
            .or_insert_with(|| Value::Object(Map::new()));
        if !deps.is_object() {
            *deps = Value::Object(Map::new());
        }
        let deps = deps.as_object_mut().expect("just made an object");
        deps.insert(alias.to_string(), Value::String(spec.to_string()));
        let mut entries: Vec<(String, Value)> = std::mem::take(deps).into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        *deps = entries.into_iter().collect();
    }

    pub fn remove_dependency(&mut self, alias: &str) -> bool {
        let object = self.object();
        let mut removed = false;
        for group in Group::ALL {
            if let Some(Value::Object(deps)) = object.get_mut(group.key()) {
                removed |= deps.shift_remove(alias).is_some();
            }
        }
        removed
    }

    pub fn save(&self) -> Result<()> {
        let mut out = Vec::new();
        let mut serializer = serde_json::Serializer::with_formatter(
            &mut out,
            PrettyFormatter::with_indent(self.indent.as_bytes()),
        );
        self.value.serialize(&mut serializer)?;
        out.push(b'\n');
        fs::write(&self.path, out).wrap_err_with(|| format!("writing {}", self.path.display()))
    }

    pub fn restore(&self) -> Result<()> {
        fs::write(&self.path, &self.original)
            .wrap_err_with(|| format!("restoring {}", self.path.display()))
    }

    fn object(&mut self) -> &mut Map<String, Value> {
        self.value.as_object_mut().expect("checked when opened")
    }
}

fn detect_indent(source: &str) -> String {
    source
        .lines()
        .map(|line| &line[..line.len() - line.trim_start().len()])
        .find(|indent| !indent.is_empty())
        .unwrap_or("  ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_dependencies_preserving_layout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.json");
        fs::write(
            &path,
            "{\n    \"name\": \"app\",\n    \"dependencies\": {\n        \"zod\": \"^3\",\n        \"left-pad\": \"^1\"\n    },\n    \"scripts\": {}\n}\n",
        )
        .unwrap();

        let mut document = Document::open(dir.path()).unwrap();
        document.set_dependency(Group::Dev, "left-pad", "^1.3.0");
        document.set_dependency(Group::Prod, "axios", "^1.7.0");
        assert!(document.remove_dependency("zod"));
        assert!(!document.remove_dependency("missing"));
        document.save().unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "{\n    \"name\": \"app\",\n    \"dependencies\": {\n        \"axios\": \"^1.7.0\"\n    },\n    \"scripts\": {},\n    \"devDependencies\": {\n        \"left-pad\": \"^1.3.0\"\n    }\n}\n"
        );

        document.restore().unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("zod"));
    }

    #[test]
    fn reads_build_allowlists_from_hinata_and_pnpm_workspace() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{ "name": "app", "dependencies": { "a": "^1" }, "hinata": { "allowBuilds": ["esbuild"] } }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "allowBuilds:\n  sharp: true\n  core-js: false\n",
        )
        .unwrap();

        let manifest = read(dir.path()).unwrap();
        assert_eq!(manifest.name.as_deref(), Some("app"));
        assert_eq!(manifest.specifiers.dependencies["a"], "^1");
        assert_eq!(
            manifest.allow_builds(),
            BTreeMap::from([
                ("esbuild".to_string(), BuildMode::Sandboxed),
                ("sharp".to_string(), BuildMode::Sandboxed),
            ])
        );
    }

    #[test]
    fn reads_impure_builds_and_overrides_pnpm_workspace() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{ "hinata": { "allowBuilds": { "esbuild": true, "puppeteer": "impure", "sharp": false } } }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "allowBuilds:\n  sharp: true\n  puppeteer: true\n",
        )
        .unwrap();

        assert_eq!(
            read(dir.path()).unwrap().allow_builds(),
            BTreeMap::from([
                ("esbuild".to_string(), BuildMode::Sandboxed),
                ("puppeteer".to_string(), BuildMode::Impure),
            ])
        );

        fs::write(
            dir.path().join("package.json"),
            r#"{ "hinata": { "allowBuilds": { "esbuild": "sandboxed" } } }"#,
        )
        .unwrap();
        assert!(read(dir.path()).is_err());
    }

    #[test]
    fn reads_build_inputs_for_sandboxed_builds() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{ "hinata": { "allowBuilds": ["a", "b"], "buildInputs": { "a": ["cairo"], "b": [] } } }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "buildInputs:\n  a: [vips]\n  b: [vips]\n",
        )
        .unwrap();

        assert_eq!(
            read(dir.path()).unwrap().build_inputs().unwrap(),
            BTreeMap::from([("a".to_string(), vec!["cairo".to_string()])])
        );

        for (config, message) in [
            (
                r#"{ "hinata": { "buildInputs": { "a": ["cairo"] } } }"#,
                "not allowed to run",
            ),
            (
                r#"{ "hinata": { "allowBuilds": { "a": "impure" }, "buildInputs": { "a": ["cairo"] } } }"#,
                "outside the Nix sandbox",
            ),
        ] {
            fs::write(dir.path().join("package.json"), config).unwrap();
            let error = read(dir.path()).unwrap().build_inputs().unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
        }
    }

    #[test]
    fn reads_patches_from_hinata_and_pnpm_workspace() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("patches")).unwrap();
        fs::write(dir.path().join("patches/a.patch"), "a").unwrap();
        fs::write(dir.path().join("patches/b.patch"), "b").unwrap();
        fs::write(
            dir.path().join("package.json"),
            r#"{ "hinata": { "patchedDependencies": { "a@1.0.0": "patches/a.patch" } } }"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "patchedDependencies:\n  a@1.0.0: patches/b.patch\n  b: ./patches/b.patch\n",
        )
        .unwrap();

        assert_eq!(
            read(dir.path()).unwrap().patches(dir.path()).unwrap(),
            BTreeMap::from([
                (
                    "a@1.0.0".to_string(),
                    Patch {
                        path: "patches/a.patch".to_string(),
                        hash: hex(Sha256::digest("a")),
                    }
                ),
                (
                    "b".to_string(),
                    Patch {
                        path: "./patches/b.patch".to_string(),
                        hash: hex(Sha256::digest("b")),
                    }
                ),
            ])
        );

        fs::write(
            dir.path().join("package.json"),
            r#"{ "hinata": { "patchedDependencies": { "a": "../a.patch" } } }"#,
        )
        .unwrap();
        let error = read(dir.path()).unwrap().patches(dir.path()).unwrap_err();
        assert!(error.to_string().contains("inside the project"), "{error}");
    }

    #[test]
    fn finds_workspace_packages() {
        let dir = tempfile::tempdir().unwrap();
        for path in [
            ".",
            "packages/a",
            "packages/b",
            "packages/a/node_modules/x",
            "packages/.hidden",
            "tools/deep/nested",
            "other",
        ] {
            fs::create_dir_all(dir.path().join(path)).unwrap();
            fs::write(dir.path().join(path).join("package.json"), "{}").unwrap();
        }
        fs::create_dir_all(dir.path().join("packages/no-manifest")).unwrap();
        fs::write(
            dir.path().join("pnpm-workspace.yaml"),
            "packages:\n  - packages/*\n  - ./tools/**\n  - '!packages/b'\n",
        )
        .unwrap();

        let manifest = read(dir.path()).unwrap();
        assert_eq!(
            manifest.workspace_packages(dir.path()).unwrap(),
            BTreeSet::from(["packages/a".to_string(), "tools/deep/nested".to_string()])
        );
    }

    #[test]
    fn matches_glob_patterns() {
        assert!(matches_segment(b"*", b"anything"));
        assert!(matches_segment(b"app-?", b"app-1"));
        assert!(!matches_segment(b"app-*", b"lib-1"));
        assert!(matches_path(&["a", "**", "c"], &["a", "c"], false));
        assert!(matches_path(
            &["a", "**", "c"],
            &["a", "b", "b", "c"],
            false
        ));
        assert!(!matches_path(&["a", "*"], &["a"], false));
        assert!(matches_path(&["a", "*"], &["a"], true));
        assert!(!matches_path(&["a", "*"], &["a", "b", "c"], true));
    }
}
