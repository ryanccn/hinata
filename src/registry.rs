// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use eyre::{Result, WrapErr};
use log::debug;
use reqwest::StatusCode;
use reqwest::header::{ACCEPT, ETAG, HeaderName, IF_MODIFIED_SINCE, IF_NONE_MATCH, LAST_MODIFIED};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::install;

pub const DEFAULT_REGISTRY: &str = "https://registry.npmjs.org";

const MAX_CONCURRENT_REQUESTS: usize = 32;

const ABBREVIATED: &str =
    "application/vnd.npm.install-v1+json; q=1.0, application/json; q=0.8, */*";

pub trait Registry {
    /// Returns packuments in the order of `names`.
    fn fetch(&self, names: &[String]) -> Result<Vec<Packument>>;

    /// Returns previously fetched packuments in the order of `names`, which may be outdated.
    fn cached(&self, names: &[String]) -> Vec<Option<Packument>> {
        names.iter().map(|_| None).collect()
    }
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
    cache: PathBuf,
    client: reqwest::Client,
    runtime: tokio::runtime::Runtime,
}

impl HttpRegistry {
    pub fn new(url: &str) -> Result<Self> {
        Ok(Self {
            url: url.trim_end_matches('/').to_string(),
            cache: install::cache_dir()?.join("metadata"),
            client: reqwest::Client::builder()
                .user_agent(concat!("hinata/", env!("CARGO_PKG_VERSION")))
                .build()?,
            runtime: tokio::runtime::Runtime::new()?,
        })
    }

    fn packument_url(&self, name: &str) -> String {
        format!("{}/{}", self.url, name.replace('/', "%2f"))
    }

    /// Named by a hash, since package names that differ only in case collide on case-insensitive
    /// file systems.
    fn cache_path(&self, name: &str) -> PathBuf {
        let digest = Sha256::new()
            .chain_update(ABBREVIATED)
            .chain_update("\n")
            .chain_update(self.packument_url(name))
            .finalize();
        let mut file = String::new();
        for byte in digest {
            write!(file, "{byte:02x}").expect("writing to a string succeeds");
        }
        file.push_str(".json");
        self.cache.join(file)
    }
}

impl Registry for HttpRegistry {
    fn fetch(&self, names: &[String]) -> Result<Vec<Packument>> {
        self.runtime.block_on(async {
            let limit = Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS));
            let mut tasks = JoinSet::new();
            for (index, name) in names.iter().enumerate() {
                let client = self.client.clone();
                let url = self.packument_url(name);
                let path = self.cache_path(name);
                let limit = limit.clone();
                let name = name.clone();
                tasks.spawn(async move {
                    let _permit = limit.acquire_owned().await?;
                    let packument = fetch_packument(&client, &url, &path)
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

    fn cached(&self, names: &[String]) -> Vec<Option<Packument>> {
        let paths: Vec<PathBuf> = names.iter().map(|name| self.cache_path(name)).collect();
        let workers = std::thread::available_parallelism()
            .map_or(1, NonZero::get)
            .min(paths.len());
        let next = AtomicUsize::new(0);
        let loaded: Vec<(usize, Packument)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    scope.spawn(|| {
                        let mut loaded = Vec::new();
                        loop {
                            let index = next.fetch_add(1, Ordering::Relaxed);
                            let Some(path) = paths.get(index) else {
                                break loaded;
                            };
                            let packument = read_entry(path)
                                .and_then(|(_, body)| serde_json::from_slice(&body).ok());
                            if let Some(packument) = packument {
                                loaded.push((index, packument));
                            }
                        }
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|handle| handle.join().expect("reading the cache does not panic"))
                .collect()
        });

        let mut packuments: Vec<Option<Packument>> = names.iter().map(|_| None).collect();
        for (index, packument) in loaded {
            packuments[index] = Some(packument);
        }
        packuments
    }
}

async fn fetch_packument(client: &reqwest::Client, url: &str, path: &Path) -> Result<Packument> {
    let cached = read_entry(path);
    let mut request = client.get(url).header(ACCEPT, ABBREVIATED);
    if let Some((validators, _)) = &cached {
        if let Some(etag) = &validators.etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = &validators.last_modified {
            request = request.header(IF_MODIFIED_SINCE, last_modified);
        }
    }

    let response = request.send().await?.error_for_status()?;
    if let Some((_, body)) = cached.filter(|_| response.status() == StatusCode::NOT_MODIFIED) {
        return Ok(serde_json::from_slice(&body)?);
    }

    let header = |name: HeaderName| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    };
    let validators = Validators {
        etag: header(ETAG),
        last_modified: header(LAST_MODIFIED),
    };
    let body = response.bytes().await?;
    let packument = serde_json::from_slice(&body)?;
    if let Err(error) = write_entry(path, &validators, &body) {
        debug!("could not cache {url} at {}: {error:?}", path.display());
    }
    Ok(packument)
}

#[derive(Serialize, Deserialize)]
struct Validators {
    etag: Option<String>,
    last_modified: Option<String>,
}

/// Entries are a line of [`Validators`] followed by the packument as the registry sent it.
fn read_entry(path: &Path) -> Option<(Validators, Vec<u8>)> {
    let mut bytes = fs::read(path).ok()?;
    let newline = bytes.iter().position(|&byte| byte == b'\n')?;
    let validators = serde_json::from_slice(&bytes[..newline]).ok()?;
    bytes.drain(..=newline);
    Some((validators, bytes))
}

fn write_entry(path: &Path, validators: &Validators, body: &[u8]) -> Result<()> {
    let dir = path.parent().expect("entries are inside the cache");
    fs::create_dir_all(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    serde_json::to_writer(&mut file, validators)?;
    file.write_all(b"\n")?;
    file.write_all(body)?;
    file.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_cache_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metadata").join("entry.json");
        let validators = Validators {
            etag: Some("W/\"a\nb\"".to_string()),
            last_modified: None,
        };
        write_entry(&path, &validators, b"{\n}").unwrap();

        let (read, body) = read_entry(&path).unwrap();
        assert_eq!(read.etag, validators.etag);
        assert_eq!(read.last_modified, None);
        assert_eq!(body, b"{\n}");
    }

    #[test]
    fn names_cache_entries_by_hash() {
        let registry = HttpRegistry::new(DEFAULT_REGISTRY).unwrap();
        let upper = registry.cache_path("@Scope/Pkg");
        let lower = registry.cache_path("@scope/pkg");

        assert_ne!(upper, lower);
        for path in [upper, lower] {
            assert_eq!(path.parent(), Some(registry.cache.as_path()));
            let name = path.file_name().unwrap().to_str().unwrap();
            assert!(
                name.strip_suffix(".json")
                    .unwrap()
                    .chars()
                    .all(|char| matches!(char, '0'..='9' | 'a'..='f'))
            );
        }
    }
}
