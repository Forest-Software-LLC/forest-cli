//! The lockfile format: forest-lock.json's shape, reading it, and the trust
//! check that decides whether a mount's section still satisfies forest.json.
//! No network; resolution and install orchestration live in lockfile_gen.rs.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::lockfile_solver::{DepSpec, LockfileEntry};
use crate::mounts::Mount;
use crate::utils::get_ci;

pub const LOCKFILE: &str = "forest-lock.json";
const FILE_VERSION: u32 = 2;

/// One mount's resolution: the packages it pins plus the overrides and
/// excludes it was solved under, so adding, changing, or removing either
/// invalidates it. Each mount records its own copy so one can re-resolve
/// while the others are carried over untouched.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LockSection {
    /// Absent from disk when empty; pre-override lockfiles parse unchanged.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub overrides: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub excludes: HashMap<String, String>,
    pub packages: HashMap<String, Vec<LockfileEntry>>,
}

impl LockSection {
    /// A root dependency's entry: the one pinned at the tree root
    /// (location "~"). Key lookup is case-insensitive like every other
    /// package-name map.
    pub fn root_entry(&self, name: &str) -> Option<&LockfileEntry> {
        get_ci(&self.packages, name)?.iter().find(|e| e.location == "~")
    }

    /// The version a root dependency is pinned to.
    pub fn pinned_version(&self, name: &str) -> Option<&str> {
        self.root_entry(name).map(|e| e.version.as_str())
    }

    /// Whether this section still satisfies a mount's declared dependencies.
    /// Root deps pin their resolved version at the tree root, so each declared
    /// range is checked against that pin, and a pin whose package is no
    /// longer declared means the dep was removed by hand. Any mismatch (or an
    /// unparseable range/version) sends the mount back through resolution,
    /// which reports invalid ranges properly.
    pub fn satisfies(
        &self,
        roots: &HashMap<String, DepSpec>,
        overrides: &HashMap<String, String>,
        excludes: &HashMap<String, String>,
    ) -> bool {
        // Any drift in the recorded overrides/excludes (added, changed, or
        // removed entry) forces re-resolution. Ranges are compared verbatim;
        // rewriting "^2.0" to "^2.0.0" is a re-solve, which lands on the
        // same versions anyway.
        let maps_match = |locked: &HashMap<String, String>, declared: &HashMap<String, String>| {
            locked.len() == declared.len()
                && declared.iter().all(|(name, range)| {
                    get_ci(locked, name).map_or(false, |l| l.trim() == range.trim())
                })
        };
        if !maps_match(&self.overrides, overrides) || !maps_match(&self.excludes, excludes) {
            return false;
        }

        for (name, spec) in roots {
            let Ok(req) = semver::VersionReq::parse(&spec.version) else {
                return false;
            };
            let Some(root_entry) = self.root_entry(name) else {
                return false;
            };
            let Ok(version) = semver::Version::parse(&root_entry.version) else {
                return false;
            };
            if !req.matches(&version) {
                return false;
            }
        }

        for (name, entries) in &self.packages {
            if entries.iter().any(|e| e.location == "~") && get_ci(roots, name).is_none() {
                return false;
            }
        }

        true
    }
}

/// forest-lock.json. The default mount's section sits at the top level,
/// the only shape older CLIs know, so they keep installing it correctly.
/// Every other mount has its own section under `mounts`, keyed by path.
#[derive(Debug, Serialize, Deserialize)]
pub struct LockFile {
    pub file_version: u32,
    #[serde(flatten)]
    pub default_mount: LockSection,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mounts: BTreeMap<String, LockSection>,
}

/// What forest-lock.json looks like on disk.
pub enum LockState {
    Missing,
    /// Present but an older or unknown format; re-resolved like a missing one.
    Outdated,
    Current(LockFile),
}

impl LockFile {
    pub fn new(default_mount: LockSection, mounts: BTreeMap<String, LockSection>) -> LockFile {
        LockFile { file_version: FILE_VERSION, default_mount, mounts }
    }

    /// Serialize with sorted keys. Serializing the HashMaps directly
    /// streams them in random order; going through serde_json::Value first
    /// sorts every object, so the same resolution writes identical bytes.
    pub fn to_json_pretty(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(&serde_json::to_value(self)?)?)
    }

    pub fn save(&self) -> Result<()> {
        std::fs::write(LOCKFILE, self.to_json_pretty()?).with_context(|| format!("Failed to write {}", LOCKFILE))
    }

    /// Read forest-lock.json from the current directory. Unparseable JSON
    /// (merge conflict markers, say) is an error, never silently replaced.
    pub fn read() -> Result<LockState> {
        if !Path::new(LOCKFILE).exists() {
            return Ok(LockState::Missing);
        }
        let content: Value = serde_json::from_str(&std::fs::read_to_string(LOCKFILE)?)
            .with_context(|| format!("{} is not valid JSON", LOCKFILE))?;
        if content.get("file_version").and_then(Value::as_u64) != Some(FILE_VERSION as u64) {
            return Ok(LockState::Outdated);
        }
        Ok(match serde_json::from_value(content) {
            Ok(lockfile) => LockState::Current(lockfile),
            Err(_) => LockState::Outdated,
        })
    }

    /// The current-format lockfile, or None for any other state.
    pub fn load() -> Option<LockFile> {
        match LockFile::read() {
            Ok(LockState::Current(lockfile)) => Some(lockfile),
            _ => None,
        }
    }

    /// A mount's section. Paths match case-insensitively, like every other
    /// mount comparison.
    pub fn section(&self, mount: &Mount) -> Option<&LockSection> {
        if mount.is_default() {
            return Some(&self.default_mount);
        }
        self.mounts.get(&mount.path).or_else(|| {
            self.mounts
                .iter()
                .find(|(path, _)| path.eq_ignore_ascii_case(&mount.path))
                .map(|(_, section)| section)
        })
    }

    /// Every section, default first.
    pub fn sections(&self) -> impl Iterator<Item = &LockSection> {
        std::iter::once(&self.default_mount).chain(self.mounts.values())
    }

    /// Every version of a package any section holds, sorted and deduped.
    pub fn versions_of(&self, name: &str) -> Vec<semver::Version> {
        let mut versions: Vec<semver::Version> = self
            .sections()
            .filter_map(|s| get_ci(&s.packages, name))
            .flatten()
            .filter_map(|e| semver::Version::parse(&e.version).ok())
            .collect();
        versions.sort();
        versions.dedup();
        versions
    }

    /// Every package key across all sections, deduped case-insensitively.
    pub fn package_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = Vec::new();
        for section in self.sections() {
            for key in section.packages.keys() {
                if !keys.iter().any(|k| k.eq_ignore_ascii_case(key)) {
                    keys.push(key.clone());
                }
            }
        }
        keys.sort();
        keys
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (package, version, location) triples -> a minimal section.
    fn section(entries: &[(&str, &str, &str)]) -> LockSection {
        let mut packages: HashMap<String, Vec<LockfileEntry>> = HashMap::new();
        for (pkg, version, location) in entries {
            packages.entry(pkg.to_string()).or_default().push(LockfileEntry {
                version: version.to_string(),
                integrity: String::new(),
                public: true,
                root: String::new(),
                location: location.to_string(),
                packages_dir: "Packages".to_string(),
                dependencies: HashMap::new(),
            });
        }
        LockSection { overrides: HashMap::new(), excludes: HashMap::new(), packages }
    }

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn roots(pairs: &[(&str, &str)]) -> HashMap<String, DepSpec> {
        pairs.iter()
            .map(|(name, range)| {
                let alias = name.split('/').last().unwrap().to_string();
                (name.to_string(), DepSpec { alias, version: range.to_string() })
            })
            .collect()
    }

    #[test]
    fn satisfied_section_is_trusted() {
        let s = section(&[("a/b", "1.5.2", "~"), ("c/d", "0.3.0", "b")]);
        assert!(s.satisfies(&roots(&[("a/b", "^1.5.0")]), &map(&[]), &map(&[])));
    }

    #[test]
    fn bumped_range_invalidates_the_section() {
        // The reported bug: ^1.5.0 was installed, the manifest now says
        // ^2.0.0, and install kept saying "already up to date".
        let s = section(&[("a/b", "1.5.2", "~")]);
        assert!(!s.satisfies(&roots(&[("a/b", "^2.0.0")]), &map(&[]), &map(&[])));
    }

    #[test]
    fn newly_declared_dep_invalidates_the_section() {
        let s = section(&[("a/b", "1.5.2", "~")]);
        assert!(!s.satisfies(&roots(&[("a/b", "^1.5.0"), ("c/d", "^0.3.0")]), &map(&[]), &map(&[])));
    }

    #[test]
    fn removed_dep_with_lingering_root_pin_invalidates_the_section() {
        let s = section(&[("a/b", "1.5.2", "~"), ("c/d", "0.3.0", "~")]);
        assert!(!s.satisfies(&roots(&[("a/b", "^1.5.0")]), &map(&[]), &map(&[])));
    }

    #[test]
    fn undeclared_transitive_entries_are_fine() {
        // c/d lives inside a/b's subtree, not at the root - it's a/b's
        // dependency, not a removed manifest entry.
        let s = section(&[("a/b", "1.5.2", "~"), ("c/d", "0.3.0", "b")]);
        assert!(s.satisfies(&roots(&[("a/b", "^1.5.0")]), &map(&[]), &map(&[])));
    }

    #[test]
    fn key_casing_differences_still_match() {
        let s = section(&[("Scope/Pkg", "1.5.2", "~")]);
        assert!(s.satisfies(&roots(&[("scope/pkg", "^1.5.0")]), &map(&[]), &map(&[])));
    }

    #[test]
    fn unparseable_range_forces_reresolution() {
        // The solver owns range errors; the check just refuses the fast path.
        let s = section(&[("a/b", "1.5.2", "~")]);
        assert!(!s.satisfies(&roots(&[("a/b", "not-a-range")]), &map(&[]), &map(&[])));
    }

    #[test]
    fn added_override_invalidates_the_section() {
        let s = section(&[("a/b", "1.5.2", "~")]);
        assert!(!s.satisfies(&roots(&[("a/b", "^1.5.0")]), &map(&[("c/d", "^2.0.0")]), &map(&[])));
    }

    #[test]
    fn matching_override_keeps_the_section_trusted() {
        let mut s = section(&[("a/b", "1.5.2", "~")]);
        s.overrides = map(&[("c/d", "^2.0.0")]);
        assert!(s.satisfies(&roots(&[("a/b", "^1.5.0")]), &map(&[("c/d", "^2.0.0")]), &map(&[])));
        // Case-insensitive keys, like every other package-name map.
        assert!(s.satisfies(&roots(&[("a/b", "^1.5.0")]), &map(&[("C/D", "^2.0.0")]), &map(&[])));
    }

    #[test]
    fn changed_or_removed_override_invalidates_the_section() {
        let mut s = section(&[("a/b", "1.5.2", "~")]);
        s.overrides = map(&[("c/d", "^2.0.0")]);
        assert!(!s.satisfies(&roots(&[("a/b", "^1.5.0")]), &map(&[("c/d", "^3.0.0")]), &map(&[])));
        assert!(!s.satisfies(&roots(&[("a/b", "^1.5.0")]), &map(&[]), &map(&[])));
    }

    #[test]
    fn exclude_drift_invalidates_the_section() {
        let mut s = section(&[("a/b", "1.5.2", "~")]);
        s.excludes = map(&[("c/d", "=1.6.0")]);
        // Matching excludes keep the fast path.
        assert!(s.satisfies(&roots(&[("a/b", "^1.5.0")]), &map(&[]), &map(&[("C/D", "=1.6.0")])));
        // Changed or removed excludes re-resolve.
        assert!(!s.satisfies(&roots(&[("a/b", "^1.5.0")]), &map(&[]), &map(&[("c/d", "=1.6.1")])));
        assert!(!s.satisfies(&roots(&[("a/b", "^1.5.0")]), &map(&[]), &map(&[])));
    }

    /// A single-mount lockfile exactly as forest wrote it before mounts
    /// existed. Parsing and re-serializing it must not change a byte, or
    /// every project's lockfile churns on upgrade.
    const PRE_MOUNT_LOCKFILE: &str = r#"{
  "excludes": {
    "evaera/promise": "=4.0.1"
  },
  "file_version": 2,
  "overrides": {
    "sleitnick/signal": "^2.0.0"
  },
  "packages": {
    "evaera/promise": [
      {
        "dependencies": {},
        "integrity": "bb",
        "location": "~/Knit",
        "public": true,
        "root": "lib/init.lua",
        "version": "4.0.0"
      }
    ],
    "sleitnick/knit": [
      {
        "dependencies": {
          "evaera/promise": {
            "alias": "Promise",
            "version": "4.0.0"
          }
        },
        "integrity": "aa",
        "location": "~",
        "packagesDir": "knit_deps",
        "public": true,
        "root": "src/init.luau",
        "version": "1.7.0"
      }
    ]
  }
}"#;

    #[test]
    fn a_single_mount_lockfile_round_trips_byte_identical() {
        let parsed: LockFile = serde_json::from_str(PRE_MOUNT_LOCKFILE).unwrap();
        assert!(parsed.mounts.is_empty());
        assert_eq!(parsed.default_mount.overrides["sleitnick/signal"], "^2.0.0");
        assert_eq!(parsed.to_json_pretty().unwrap(), PRE_MOUNT_LOCKFILE);
    }

    #[test]
    fn mount_sections_round_trip_next_to_the_default_section() {
        let mut mounts = BTreeMap::new();
        mounts.insert("DevPackages".to_string(), section(&[("roblox/testez", "0.4.1", "~")]));
        let lockfile = LockFile::new(section(&[("a/b", "1.0.0", "~")]), mounts);

        let json = lockfile.to_json_pretty().unwrap();
        let value: Value = serde_json::from_str(&json).unwrap();
        assert!(value["packages"]["a/b"].is_array(), "default section stays at the top level");
        assert!(value["mounts"]["DevPackages"]["packages"]["roblox/testez"].is_array());
        assert!(value["mounts"]["DevPackages"].get("overrides").is_none(), "empty maps stay off disk");

        let back: LockFile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.to_json_pretty().unwrap(), json);
    }

    #[test]
    fn versions_and_keys_span_every_section() {
        let mut mounts = BTreeMap::new();
        mounts.insert("DevPackages".to_string(), section(&[("Acme/Pkg", "2.0.0", "~"), ("x/y", "1.0.0", "~")]));
        let lockfile = LockFile::new(section(&[("acme/pkg", "1.0.0", "~")]), mounts);

        let versions: Vec<String> = lockfile.versions_of("acme/pkg").iter().map(|v| v.to_string()).collect();
        assert_eq!(versions, vec!["1.0.0", "2.0.0"]);
        assert_eq!(lockfile.package_keys(), vec!["acme/pkg".to_string(), "x/y".to_string()]);
    }
}
