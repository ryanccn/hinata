// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr, bail};
use serde::{Deserialize, Serialize};
use serde_json::ser::PrettyFormatter;
use serde_json::{Map, Value};

use crate::lock::Specifiers;

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
    allow_builds: BTreeSet<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PnpmWorkspace {
    #[serde(default)]
    allow_builds: BTreeMap<String, bool>,
}

impl Manifest {
    pub fn allow_builds(&self) -> BTreeSet<String> {
        let workspace = self
            .workspace
            .allow_builds
            .iter()
            .filter(|(_, allowed)| **allowed)
            .map(|(name, _)| name);
        self.hinata
            .allow_builds
            .iter()
            .chain(workspace)
            .cloned()
            .collect()
    }
}

pub fn read(dir: &Path) -> Result<Manifest> {
    let path = dir.join("package.json");
    let source =
        fs::read_to_string(&path).wrap_err_with(|| format!("reading {}", path.display()))?;
    let mut manifest: Manifest =
        serde_json::from_str(&source).wrap_err_with(|| format!("parsing {}", path.display()))?;

    let path = dir.join("pnpm-workspace.yaml");
    match fs::read_to_string(&path) {
        Ok(source) => {
            manifest.workspace = serde_yaml::from_str(&source)
                .wrap_err_with(|| format!("parsing {}", path.display()))?;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).wrap_err_with(|| format!("reading {}", path.display())),
    }
    Ok(manifest)
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
            BTreeSet::from(["esbuild".to_string(), "sharp".to_string()])
        );
    }
}
