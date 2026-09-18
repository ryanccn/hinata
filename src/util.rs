// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr, eyre};
use log::debug;
use owo_colors::colors::Blue;
use sha2::{Digest, Sha256};

use crate::logging::LogDisplay as _;

/// GC roots are kept outside projects, which the Nix daemon may not be permitted to read.
pub fn cache_dir() -> Result<PathBuf> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| eyre!("neither XDG_CACHE_HOME nor HOME is set"))?;
    Ok(cache.join("hinata"))
}

pub fn projects_dir() -> Result<PathBuf> {
    Ok(cache_dir()?.join("projects"))
}

pub fn metadata_dir() -> Result<PathBuf> {
    Ok(cache_dir()?.join("metadata"))
}

pub fn project_dir(root: &Path) -> Result<PathBuf> {
    Ok(projects_dir()?.join(hex(Sha256::digest(root.as_os_str().as_bytes()))))
}

pub fn hex(bytes: impl IntoIterator<Item = u8>) -> String {
    let mut hex = String::new();
    for byte in bytes {
        write!(hex, "{byte:02x}").expect("writing to a string succeeds");
    }
    hex
}

/// Readers never see a partially written file, even when installs run concurrently.
pub fn write_atomically(path: &Path, contents: &[u8]) -> Result<()> {
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
pub fn prune_projects(projects: &Path) -> usize {
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
    use super::*;

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
