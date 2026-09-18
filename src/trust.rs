// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, IsTerminal as _};
use std::path::Path;

use eyre::{Result, bail};
use log::{debug, info, warn};
use owo_colors::OwoColorize as _;
use owo_colors::colors::{Blue, Yellow};
use sha2::{Digest, Sha256};

use crate::util::{hex, write_atomically};
use crate::lock::Lock;
use crate::logging::{self, LogDisplay as _};

/// Impure install scripts run as the user, and binary caches can provide any store path, so both
/// affect more than the project and are only used once the user has approved them. Approvals are
/// remembered by hash in `project`, the project's cache directory.
pub fn check(
    project: &Path,
    lock: &Lock,
    substituters: &BTreeMap<String, String>,
    trusted: bool,
) -> Result<()> {
    approve(project, lock, substituters, || {
        if trusted {
            info!("approved with {}", "--trust".log_display::<Blue>());
            return Ok(true);
        }
        if !io::stdin().is_terminal() {
            bail!("there is no terminal to ask for approval on; pass --trust to approve these");
        }

        Ok(logging::confirm("approve these?")?)
    })
}

fn approve(
    project: &Path,
    lock: &Lock,
    substituters: &BTreeMap<String, String>,
    confirm: impl FnOnce() -> Result<bool>,
) -> Result<()> {
    let impure: Vec<(&str, &str, &str)> = lock
        .packages
        .values()
        .filter(|package| package.impure_build)
        .map(|package| {
            (
                package.name.as_str(),
                package.version.as_str(),
                package.integrity.as_str(),
            )
        })
        .collect();
    if impure.is_empty() && substituters.is_empty() {
        return Ok(());
    }

    let hash = hex(Sha256::digest(serde_json::to_vec(&(
        &impure,
        substituters,
    ))?));
    let path = project.join("trust");
    if fs::read_to_string(&path).is_ok_and(|approved| approved == hash) {
        debug!("the project's impure builds and binary caches are already approved");
        return Ok(());
    }

    warn!("this project needs approval before installing:");
    for (name, version, _) in &impure {
        warn!(
            "  {} runs install scripts with your permissions and network access",
            format_args!("{name}@{version}").log_display::<Yellow>()
        );
    }
    for (url, key) in substituters {
        warn!(
            "  {} can provide any store path, signed by {}",
            url.log_display::<Yellow>(),
            key.dimmed()
        );
    }

    if !confirm()? {
        bail!("not approved, so nothing was installed");
    }
    write_atomically(&path, hash.as_bytes())?;
    info!("remembering the approval until these change");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn lock(impure: &[(&str, &str)]) -> Lock {
        let packages: serde_json::Map<String, serde_json::Value> = impure
            .iter()
            .map(|(name, version)| {
                (
                    format!("{name}@{version}"),
                    serde_json::json!({
                        "name": name,
                        "version": version,
                        "url": "https://registry.test/a.tgz",
                        "integrity": "sha512-x",
                        "impureBuild": true,
                    }),
                )
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "version": 1,
            "packages": packages,
            "sccs": [],
            "importers": {},
        }))
        .unwrap()
    }

    #[test]
    fn asks_again_only_when_approvals_change() {
        let project = tempfile::tempdir().unwrap();
        let asked = Cell::new(0);
        let check = |lock: &Lock, substituters: &BTreeMap<String, String>, answer: bool| {
            approve(project.path(), lock, substituters, || {
                asked.set(asked.get() + 1);
                Ok(answer)
            })
        };
        let none = BTreeMap::new();

        assert!(check(&lock(&[]), &none, false).is_ok());
        assert_eq!(asked.get(), 0);

        assert!(check(&lock(&[("a", "1.0.0")]), &none, false).is_err());
        assert!(check(&lock(&[("a", "1.0.0")]), &none, true).is_ok());
        assert!(check(&lock(&[("a", "1.0.0")]), &none, false).is_ok());
        assert_eq!(asked.get(), 2);

        assert!(check(&lock(&[("a", "1.0.1")]), &none, false).is_err());
        let cache = BTreeMap::from([("https://cache.test".to_string(), "key".to_string())]);
        assert!(check(&lock(&[("a", "1.0.0")]), &cache, false).is_err());
        assert_eq!(asked.get(), 4);
    }
}
