//! `forest link`: machine-local dependency overrides.
//!
//! Link state lives in gitignored `.forest/links.json` and never touches
//! forest.json or forest-lock.json, so collaborators see a normal install.
//! Resolution runs on the registry graph as if no links existed; the
//! platform executor applies linked slots as an overlay afterwards
//! (Roblox: src/roblox/link_overlay.rs).
//!
//! Core module: storage, policy, and matching stored links against the
//! manifest's direct dependencies. Never imports platform code.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, Context, Result};
use serde_json::{Map, Value};

use crate::lockfile_solver::DepSpec;
use crate::utils::same_package;

pub const LINKS_DIR: &str = ".forest";
pub const LINKS_FILE: &str = ".forest/links.json";
const LINKS_COMMENT: &str =
    "Machine-local forest link overrides. Do NOT commit this file.";
const LINKS_VERSION: u64 = 1;

// Policy: whether this process applies links at install time. CI ignores
// links by default so a leaked links file can't change what CI builds.

/// The `--links` install flag. `forbid` is enforced by the install command
/// before any work runs; `apply`/`ignore` feed the process-wide policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LinksMode {
    /// Apply local links (the default outside CI)
    Apply,
    /// Install exactly what the lockfile says, ignoring links (the default under CI)
    Ignore,
    /// Fail the install if any local links are configured
    Forbid,
}

pub enum LinkPolicy {
    Apply,
    /// Ignore all links, with a user-facing reason.
    Ignore(String),
}

static POLICY: OnceLock<LinkPolicy> = OnceLock::new();

/// Set the process-wide link policy (first call wins; commands set it from
/// their flags before any install work runs).
pub fn set_policy(policy: LinkPolicy) {
    let _ = POLICY.set(policy);
}

fn policy() -> &'static LinkPolicy {
    POLICY.get_or_init(|| {
        if std::env::var("CI").is_ok() {
            LinkPolicy::Ignore(
                "CI environment detected; run `forest install --links apply` to apply them".to_string(),
            )
        } else {
            LinkPolicy::Apply
        }
    })
}

// Storage. Writes round-trip through serde_json::Value so unknown fields
// survive; the schema is versioned.

/// One entry as stored on disk: the canonical dependency key it overrides
/// and the target path exactly as the user typed it.
#[derive(Debug, Clone)]
pub struct StoredLink {
    pub name: String,
    pub path: String,
}

fn read_file(manifest_dir: &Path) -> Option<Value> {
    let text = fs::read_to_string(manifest_dir.join(LINKS_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_file(manifest_dir: &Path, mut file: Value) -> Result<()> {
    let obj = file
        .as_object_mut()
        .ok_or_else(|| anyhow!("links file root must be an object"))?;
    obj.insert("_comment".to_string(), Value::String(LINKS_COMMENT.to_string()));
    obj.insert("version".to_string(), Value::from(LINKS_VERSION));
    if !obj.get("links").map_or(false, Value::is_object) {
        obj.insert("links".to_string(), Value::Object(Map::new()));
    }
    let dir = manifest_dir.join(LINKS_DIR);
    fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    fs::write(
        manifest_dir.join(LINKS_FILE),
        serde_json::to_string_pretty(&file)?,
    )
    .with_context(|| format!("Failed to write {}", LINKS_FILE))
}

/// Every link stored in the current directory's links file. Malformed
/// entries are skipped, a missing or unparseable file reads as empty.
pub fn stored_links() -> Vec<StoredLink> {
    stored_links_in(Path::new("."))
}

pub fn stored_links_in(manifest_dir: &Path) -> Vec<StoredLink> {
    let Some(file) = read_file(manifest_dir) else {
        return Vec::new();
    };
    let Some(links) = file.get("links").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut out: Vec<StoredLink> = links
        .iter()
        .filter_map(|(name, entry)| {
            let path = entry.get("path").and_then(Value::as_str)?;
            Some(StoredLink { name: name.clone(), path: path.to_string() })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Add or replace the link for `name` (case-insensitive key match replaces
/// in place, keeping the stored casing stable).
pub fn upsert_link(manifest_dir: &Path, name: &str, path: &str) -> Result<()> {
    let mut file = read_file(manifest_dir).unwrap_or_else(|| Value::Object(Map::new()));
    if !file.get("links").map_or(false, Value::is_object) {
        file["links"] = Value::Object(Map::new());
    }
    let links = file["links"].as_object_mut().expect("links is an object");
    let key = links
        .keys()
        .find(|k| same_package(k, name))
        .cloned()
        .unwrap_or_else(|| name.to_string());
    // Preserve unknown fields of an existing entry; only path/createdAt move.
    let mut entry = links
        .get(&key)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    entry.insert("path".to_string(), Value::String(path.to_string()));
    entry.insert(
        "createdAt".to_string(),
        Value::String(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
    );
    links.insert(key, Value::Object(entry));
    write_file(manifest_dir, file)
}

/// Remove one link, matched by package name (case-insensitive) or by the
/// stored path (verbatim or resolving to the same location). Returns the
/// removed key, or None when nothing matched.
pub fn remove_link(manifest_dir: &Path, reference: &str) -> Result<Option<String>> {
    let Some(mut file) = read_file(manifest_dir) else {
        return Ok(None);
    };
    let Some(links) = file.get_mut("links").and_then(Value::as_object_mut) else {
        return Ok(None);
    };
    let ref_resolved = fs::canonicalize(manifest_dir.join(reference)).ok();
    let key = links
        .iter()
        .find(|(name, entry)| {
            if same_package(name, reference)
                || name.rsplit('/').next().map_or(false, |n| n.eq_ignore_ascii_case(reference))
            {
                return true;
            }
            let Some(stored_path) = entry.get("path").and_then(Value::as_str) else {
                return false;
            };
            stored_path == reference
                || (ref_resolved.is_some()
                    && fs::canonicalize(manifest_dir.join(stored_path)).ok() == ref_resolved)
        })
        .map(|(name, _)| name.clone());
    if let Some(key) = &key {
        links.remove(key);
        write_file(manifest_dir, file)?;
    }
    Ok(key)
}

/// Remove every link. Returns the removed keys.
pub fn remove_all(manifest_dir: &Path) -> Result<Vec<String>> {
    let Some(mut file) = read_file(manifest_dir) else {
        return Ok(Vec::new());
    };
    let Some(links) = file.get_mut("links").and_then(Value::as_object_mut) else {
        return Ok(Vec::new());
    };
    let keys: Vec<String> = links.keys().cloned().collect();
    if !keys.is_empty() {
        links.clear();
        write_file(manifest_dir, file)?;
    }
    Ok(keys)
}

// Resolution against the manifest's direct dependencies.

/// A stored link that matched a direct dependency and whose target is
/// readable right now; everything the overlay needs.
#[derive(Debug, Clone)]
pub struct ActiveLink {
    /// The manifest's dependency key, in the manifest's casing.
    pub name: String,
    /// Install-folder name for the dep (explicit alias or the name part).
    pub alias: String,
    /// Target path as the user typed it (for display).
    pub path_display: String,
    /// Directory the slot mounts: the parent of the linked manifest's root
    /// module, or the target itself for top-level roots.
    pub source_dir: PathBuf,
    /// The linked manifest's `root` ("" when absent).
    pub root: String,
    /// The linked manifest's version ("" when absent).
    pub version: String,
    /// The linked manifest's declared dependencies: name -> range.
    pub dependencies: HashMap<String, String>,
}

#[derive(Debug, Default)]
pub struct LinkResolution {
    pub active: Vec<ActiveLink>,
    pub warnings: Vec<String>,
    /// Set when the policy suppressed links: (how many, why).
    pub ignored: Option<(usize, String)>,
}

/// Read the linked manifest and build an ActiveLink. The Err string is a
/// user-facing warning.
fn resolve_one(link: &StoredLink, alias: &str, dep_key: &str) -> std::result::Result<ActiveLink, String> {
    let target = Path::new(&link.path);
    let target = if target.is_absolute() {
        target.to_path_buf()
    } else {
        Path::new(".").join(target)
    };
    let manifest_path = target.join("forest.json");
    let text = fs::read_to_string(&manifest_path).map_err(|_| {
        format!(
            "Link for {} is broken: {} not found. The registry version stays installed; run `forest unlink {}` or fix the path.",
            dep_key,
            manifest_path.display(),
            dep_key
        )
    })?;
    let manifest: Value = serde_json::from_str(&text).map_err(|e| {
        format!("Link for {} is broken: {} is not valid JSON ({}). The registry version stays installed.", dep_key, manifest_path.display(), e)
    })?;
    let target = fs::canonicalize(&target).map_err(|_| {
        format!("Link for {} is broken: could not resolve {}.", dep_key, target.display())
    })?;

    let root = manifest
        .get("root")
        .and_then(Value::as_str)
        .unwrap_or("")
        .replace('\\', "/");
    let root = root.strip_prefix("./").unwrap_or(&root).to_string();
    let source_dir = match crate::utils::manifest_root_parent(&root) {
        Some(parent) => target.join(parent),
        None => target.clone(),
    };
    if !source_dir.is_dir() {
        return Err(format!(
            "Link for {} is broken: root directory {} does not exist. The registry version stays installed.",
            dep_key,
            source_dir.display()
        ));
    }

    Ok(ActiveLink {
        name: dep_key.to_string(),
        alias: alias.to_string(),
        path_display: link.path.clone(),
        source_dir,
        root,
        version: manifest
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        dependencies: crate::utils::manifest_dep_ranges(&manifest),
    })
}

/// Match the stored links against the manifest's direct dependencies and
/// check each target is still readable. Links that don't match a dependency
/// or whose target is gone become warnings, never errors; install must keep
/// working with the registry graph.
pub fn resolve_active(root_deps: &HashMap<String, DepSpec>) -> LinkResolution {
    let stored = stored_links();
    if stored.is_empty() {
        return LinkResolution::default();
    }
    if let LinkPolicy::Ignore(reason) = policy() {
        return LinkResolution {
            active: Vec::new(),
            warnings: Vec::new(),
            ignored: Some((stored.len(), reason.clone())),
        };
    }

    let mut res = LinkResolution::default();
    for link in stored {
        let Some((dep_key, spec)) = root_deps
            .iter()
            .find(|(k, _)| same_package(k, &link.name))
        else {
            res.warnings.push(format!(
                "Link for {} no longer matches a dependency in forest.json; run `forest unlink {}` to clean it up.",
                link.name, link.name
            ));
            continue;
        };
        match resolve_one(&link, &spec.alias, dep_key) {
            Ok(active) => res.active.push(active),
            Err(warning) => res.warnings.push(warning),
        }
    }
    res.active.sort_by(|a, b| a.name.cmp(&b.name));
    res
}

// Reporting helpers.

/// The banner every install/resolve-adjacent command prints while links are
/// active. One line per link, on stderr. `pinned` maps a dependency key to
/// the lockfile-pinned version.
pub fn print_banner(active: &[ActiveLink], pinned: impl Fn(&str) -> Option<String>) {
    if active.is_empty() {
        return;
    }
    use colored::Colorize;
    eprintln!(
        "{}",
        format!(
            "⚠  {} package{} linked locally:",
            active.len(),
            if active.len() == 1 { "" } else { "s" }
        )
        .yellow()
        .bold()
    );
    for link in active {
        let pin = pinned(&link.name).unwrap_or_else(|| "not installed".to_string());
        let linked = if link.version.is_empty() { "unversioned".to_string() } else { link.version.clone() };
        eprintln!(
            "{}",
            format!(
                "   {} → {} (registry pin: {}, linked: {})",
                link.name, link.path_display, pin, linked
            )
            .yellow()
        );
    }
}

/// Differences between the linked manifest's declared deps and the registry
/// version's pinned dependency set. Informational: a linked package's deps
/// come from its own working tree, not this project's graph.
pub fn dep_divergences(
    link: &ActiveLink,
    pinned_deps: &HashMap<String, DepSpec>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (name, range) in &link.dependencies {
        match pinned_deps.iter().find(|(k, _)| same_package(k, name)) {
            None => out.push(format!("adds {} ({})", name, range)),
            Some((_, spec)) => {
                let matches = semver::VersionReq::parse(range)
                    .ok()
                    .zip(semver::Version::parse(&spec.version).ok())
                    .map(|(req, ver)| req.matches(&ver));
                if matches == Some(false) {
                    out.push(format!(
                        "wants {} {} (registry version pinned {})",
                        name, range, spec.version
                    ));
                }
            }
        }
    }
    for name in pinned_deps.keys() {
        if !link.dependencies.keys().any(|k| same_package(k, name)) {
            out.push(format!("drops {}", name));
        }
    }
    out.sort();
    out
}

/// Make sure `.forest/` is gitignored in `dir`, creating .gitignore when
/// missing. Returns true when an entry was added.
pub fn ensure_gitignored(dir: &Path) -> Result<bool> {
    let path = dir.join(".gitignore");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let covered = existing.lines().map(str::trim).any(|line| {
        matches!(line, ".forest" | ".forest/" | "/.forest" | "/.forest/" | ".forest/links.json")
    });
    if covered {
        return Ok(false);
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(".forest/\n");
    fs::write(&path, updated).with_context(|| format!("Failed to update {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("forest-links-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn links_file_round_trips_and_preserves_unknown_fields() {
        let dir = fixture("roundtrip");
        fs::create_dir_all(dir.join(LINKS_DIR)).unwrap();
        fs::write(
            dir.join(LINKS_FILE),
            r#"{
              "_comment": "old comment",
              "version": 1,
              "futureField": {"keep": true},
              "links": {
                "acme/knit": {"path": "../knit", "createdAt": "2026-01-01T00:00:00Z", "extra": 7}
              }
            }"#,
        )
        .unwrap();

        upsert_link(&dir, "acme/promise", "../promise").unwrap();

        let file: Value = serde_json::from_str(&fs::read_to_string(dir.join(LINKS_FILE)).unwrap()).unwrap();
        assert_eq!(file["futureField"]["keep"], Value::Bool(true), "unknown top-level fields survive");
        assert_eq!(file["links"]["acme/knit"]["extra"], Value::from(7), "unknown entry fields survive");
        assert_eq!(file["links"]["acme/knit"]["path"], Value::from("../knit"));
        assert_eq!(file["links"]["acme/promise"]["path"], Value::from("../promise"));
        assert_eq!(file["_comment"], Value::from(LINKS_COMMENT), "comment is normalized on write");

        // Case-insensitive upsert replaces in place, keeping the stored casing.
        upsert_link(&dir, "ACME/Knit", "../knit2").unwrap();
        let links = stored_links_in(&dir);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].name, "acme/knit");
        assert_eq!(links[0].path, "../knit2");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_matches_name_bare_name_and_path() {
        let dir = fixture("remove");
        upsert_link(&dir, "acme/knit", "../knit").unwrap();
        upsert_link(&dir, "acme/promise", "../promise").unwrap();

        assert_eq!(remove_link(&dir, "ACME/KNIT").unwrap(), Some("acme/knit".to_string()));
        assert_eq!(remove_link(&dir, "promise").unwrap(), Some("acme/promise".to_string()));
        assert_eq!(remove_link(&dir, "acme/gone").unwrap(), None, "unknown reference is a no-op");

        upsert_link(&dir, "acme/knit", "../knit").unwrap();
        assert_eq!(remove_link(&dir, "../knit").unwrap(), Some("acme/knit".to_string()), "verbatim path matches");
        assert!(stored_links_in(&dir).is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_all_clears_and_reports() {
        let dir = fixture("remove-all");
        assert!(remove_all(&dir).unwrap().is_empty(), "no file is a clean no-op");
        upsert_link(&dir, "a/x", "../x").unwrap();
        upsert_link(&dir, "b/y", "../y").unwrap();
        let mut removed = remove_all(&dir).unwrap();
        removed.sort();
        assert_eq!(removed, vec!["a/x".to_string(), "b/y".to_string()]);
        assert!(stored_links_in(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_links_files_read_as_empty() {
        let dir = fixture("malformed");
        fs::create_dir_all(dir.join(LINKS_DIR)).unwrap();
        fs::write(dir.join(LINKS_FILE), "{not json").unwrap();
        assert!(stored_links_in(&dir).is_empty());
        fs::write(dir.join(LINKS_FILE), r#"{"links": 42}"#).unwrap();
        assert!(stored_links_in(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gitignore_entry_is_added_once() {
        let dir = fixture("gitignore");
        assert!(ensure_gitignored(&dir).unwrap(), "created with the entry");
        assert!(!ensure_gitignored(&dir).unwrap(), "second call is a no-op");
        let text = fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert_eq!(text.matches(".forest").count(), 1);

        // Existing file without trailing newline gets a clean append.
        fs::write(dir.join(".gitignore"), "target").unwrap();
        assert!(ensure_gitignored(&dir).unwrap());
        assert_eq!(fs::read_to_string(dir.join(".gitignore")).unwrap(), "target\n.forest/\n");

        // Already covered by a variant spelling.
        fs::write(dir.join(".gitignore"), "/.forest/\n").unwrap();
        assert!(!ensure_gitignored(&dir).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dep_divergences_report_adds_drops_and_range_conflicts() {
        let link = ActiveLink {
            name: "acme/knit".into(),
            alias: "Knit".into(),
            path_display: "../knit".into(),
            source_dir: PathBuf::new(),
            root: String::new(),
            version: "1.1.0-dev".into(),
            dependencies: [
                ("acme/comm".to_string(), "^1.0.0".to_string()),
                ("acme/new".to_string(), "^0.1.0".to_string()),
                ("acme/promise".to_string(), "^3.0.0".to_string()),
            ]
            .into(),
        };
        let pinned: HashMap<String, DepSpec> = [
            ("acme/comm".to_string(), DepSpec { alias: "Comm".into(), version: "1.2.0".into() }),
            ("acme/promise".to_string(), DepSpec { alias: "Promise".into(), version: "2.0.0".into() }),
            ("acme/old".to_string(), DepSpec { alias: "Old".into(), version: "0.9.0".into() }),
        ]
        .into();

        let diffs = dep_divergences(&link, &pinned);
        assert_eq!(diffs, vec![
            "adds acme/new (^0.1.0)".to_string(),
            "drops acme/old".to_string(),
            "wants acme/promise ^3.0.0 (registry version pinned 2.0.0)".to_string(),
        ]);
    }
}
