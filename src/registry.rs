// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeMap;
use std::sync::Arc;

use eyre::{Result, WrapErr};
use reqwest::header::ACCEPT;
use serde::{Deserialize, Deserializer};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";

const MAX_CONCURRENT_REQUESTS: usize = 32;

const ABBREVIATED: &str =
    "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8, */*";

pub trait Registry {
    /// Returns packuments in the order of `names`.
    fn fetch(&self, names: &[String]) -> Result<Vec<Packument>>;
}

#[derive(Deserialize)]
pub struct Packument {
    #[serde(rename = "dist-tags", default)]
    pub dist_tags: BTreeMap<String, String>,
    /// Left unparsed: only chosen versions are read, and some old versions are malformed.
    #[serde(default)]
    pub versions: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionManifest {
    #[serde(default)]
    pub dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub optional_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub peer_dependencies: BTreeMap<String, String>,
    #[serde(default)]
    pub peer_dependencies_meta: BTreeMap<String, PeerMeta>,
    #[serde(default)]
    pub bin: serde_json::Value,
    #[serde(default)]
    pub directories: serde_json::Value,
    pub dist: Dist,
    #[serde(default, deserialize_with = "string_or_list")]
    pub os: Option<Vec<String>>,
    #[serde(default, deserialize_with = "string_or_list")]
    pub cpu: Option<Vec<String>>,
    #[serde(default, deserialize_with = "string_or_list")]
    pub libc: Option<Vec<String>>,
    #[serde(default)]
    pub has_install_script: bool,
}

impl VersionManifest {
    pub fn has_bin(&self) -> bool {
        let declared = match &self.bin {
            serde_json::Value::String(path) => !path.is_empty(),
            serde_json::Value::Object(bins) => !bins.is_empty(),
            _ => false,
        };
        declared || self.directories.get("bin").is_some()
    }
}

#[derive(Deserialize, Default)]
pub struct PeerMeta {
    #[serde(default)]
    pub optional: bool,
}

#[derive(Deserialize)]
pub struct Dist {
    pub tarball: String,
    pub integrity: Option<String>,
}

fn string_or_list<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(
        Option::<OneOrMany>::deserialize(deserializer)?.map(|value| match value {
            OneOrMany::One(item) => vec![item],
            OneOrMany::Many(items) => items,
        }),
    )
}

pub struct HttpRegistry {
    url: String,
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
}

impl HttpRegistry {
    pub fn new(url: &str) -> Result<Self> {
        Ok(Self {
            url: url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .user_agent(concat!("hinata/", env!("CARGO_PKG_VERSION")))
                .build()?,
            runtime: tokio::runtime::Runtime::new()?,
        })
    }
}

impl Registry for HttpRegistry {
    fn fetch(&self, names: &[String]) -> Result<Vec<Packument>> {
        self.runtime.block_on(async {
            let limit = Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS));
            let mut tasks = JoinSet::new();
            for (index, name) in names.iter().enumerate() {
                let request = self
                    .client
                    .get(format!("{}/{}", self.url, name.replace('/', "%2f")))
                    .header(ACCEPT, ABBREVIATED);
                let limit = limit.clone();
                let name = name.clone();
                tasks.spawn(async move {
                    let _permit = limit.acquire_owned().await?;
                    let packument = async {
                        request
                            .send()
                            .await?
                            .error_for_status()?
                            .json::<Packument>()
                            .await
                    }
                    .await
                    .wrap_err_with(|| format!("fetching {name} from the registry"))?;
                    Ok::<_, eyre::Report>((index, packument))
                });
            }

            let mut packuments: Vec<Option<Packument>> = names.iter().map(|_| None).collect();
            while let Some(joined) = tasks.join_next().await {
                let (index, packument) = joined??;
                packuments[index] = Some(packument);
            }
            Ok::<_, eyre::Report>(
                packuments
                    .into_iter()
                    .map(|packument| packument.expect("every fetch reports back"))
                    .collect(),
            )
        })
    }
}
