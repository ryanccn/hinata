// SPDX-FileCopyrightText: 2026 Ryan Cao <hello@ryanccn.dev>
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Overrides replace what a dependency asks for before a version is chosen, whoever asks for it.

use std::cell::Cell;
use std::collections::BTreeMap;

use eyre::{Result, WrapErr, eyre};
use node_semver::Range;

use super::{parse_spec, split_version};

pub struct Overrides {
    entries: Vec<Entry>,
}

struct Entry {
    /// The configured key, for reporting.
    key: String,
    name: String,
    /// Only requests whose range overlaps this one are overridden.
    selector: Option<(String, Range)>,
    to: (String, String),
    used: Cell<bool>,
}

impl Overrides {
    /// Keyed by `name` or `name@range`, and valued by a range or an `npm:` alias.
    pub fn new(config: &BTreeMap<String, String>) -> Result<Self> {
        let mut entries = Vec::new();
        for (key, spec) in config {
            let (name, selector) = match split_version(key) {
                Some((name, range)) => {
                    let parsed = Range::parse(range).map_err(|error| {
                        eyre!("the override {key} is not a valid range: {error}")
                    })?;
                    (name, Some((range.to_string(), parsed)))
                }
                None => (key.as_str(), None),
            };
            let to =
                parse_spec(name, spec).wrap_err_with(|| format!("in the override for {key}"))?;
            entries.push(Entry {
                key: key.clone(),
                name: name.to_string(),
                selector,
                to,
                used: Cell::new(false),
            });
        }
        Ok(Overrides { entries })
    }

    /// What replaces a request for `name` at `range`, if an override matches it.
    pub fn applies(&self, name: &str, range: &str) -> Option<(String, String)> {
        let entry = self
            .entries
            .iter()
            .filter(|entry| entry.name == name && entry.selects(range))
            // A selector is more specific than a bare name.
            .max_by_key(|entry| entry.selector.is_some())?;
        entry.used.set(true);
        Some(entry.to.clone())
    }

    pub fn unused(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|entry| !entry.used.get())
            .map(|entry| entry.key.as_str())
            .collect()
    }
}

impl Entry {
    fn selects(&self, range: &str) -> bool {
        let Some((raw, selector)) = &self.selector else {
            return true;
        };
        // Tags and other ranges that do not parse can still be named exactly.
        raw == range || Range::parse(range).is_ok_and(|range| selector.allows_any(&range))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overrides(entries: &[(&str, &str)]) -> Overrides {
        let config = entries
            .iter()
            .map(|(key, spec)| ((*key).to_string(), (*spec).to_string()))
            .collect();
        Overrides::new(&config).unwrap()
    }

    fn applies(overrides: &Overrides, name: &str, range: &str) -> Option<String> {
        overrides
            .applies(name, range)
            .map(|(name, range)| format!("{name}@{range}"))
    }

    #[test]
    fn overrides_every_request_for_a_name() {
        let overrides = overrides(&[("lodash", "^4.17.21")]);

        assert_eq!(
            applies(&overrides, "lodash", "^3.0.0").as_deref(),
            Some("lodash@^4.17.21")
        );
        assert_eq!(
            applies(&overrides, "lodash", "latest").as_deref(),
            Some("lodash@^4.17.21")
        );
        assert_eq!(applies(&overrides, "semver", "^7.0.0"), None);
    }

    #[test]
    fn overrides_only_overlapping_ranges() {
        let overrides = overrides(&[("semver@<7.5.2", "^7.5.2")]);

        assert_eq!(
            applies(&overrides, "semver", "^7.3.0").as_deref(),
            Some("semver@^7.5.2")
        );
        assert_eq!(applies(&overrides, "semver", "^7.6.0"), None);
        assert_eq!(applies(&overrides, "semver", "latest"), None);
    }

    #[test]
    fn prefers_a_selector_over_a_bare_name() {
        let overrides = overrides(&[("semver", "^7.0.0"), ("semver@<6", "^6.3.1")]);

        assert_eq!(
            applies(&overrides, "semver", "^5.7.0").as_deref(),
            Some("semver@^6.3.1")
        );
        assert_eq!(
            applies(&overrides, "semver", "^7.3.0").as_deref(),
            Some("semver@^7.0.0")
        );
    }

    #[test]
    fn overrides_a_name_with_an_alias() {
        let overrides = overrides(&[("lodash", "npm:lodash-es@^4")]);

        assert_eq!(
            applies(&overrides, "lodash", "^4.0.0").as_deref(),
            Some("lodash-es@^4")
        );
    }

    #[test]
    fn reports_overrides_that_never_matched() {
        let overrides = overrides(&[("lodash", "^4.17.21"), ("semver@<6", "^6.3.1")]);
        applies(&overrides, "lodash", "^4.0.0");

        assert_eq!(overrides.unused(), ["semver@<6"]);
    }

    #[test]
    fn rejects_overrides_that_cannot_be_resolved() {
        let config = BTreeMap::from([("lodash".to_string(), "file:../lodash".to_string())]);
        assert!(Overrides::new(&config).is_err());

        let config = BTreeMap::from([("lodash@what".to_string(), "^4".to_string())]);
        assert!(Overrides::new(&config).is_err());
    }
}
