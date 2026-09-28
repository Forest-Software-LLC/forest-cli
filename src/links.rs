//! `forest link`: machine-local dependency overrides.
//!
//! Link state lives in gitignored `.forest/links.json` and never touches
//! forest.json or forest-lock.json, so collaborators see a normal install.
//! Resolution runs on the registry graph as if no links existed; the
//! platform executor applies linked slots as an overlay afterwards
//! (Roblox: src/roblox/link_overlay.rs).
//!
//! Core module: storage, policy, and matching stored links against a
//! mount's direct dependencies. Never imports platform code.
//!
//! Links for the default mount live under `links`, the only shape older
//! CLIs know; links for other mounts live under `mounts`, keyed by mount
//! path, where older CLIs never look.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{anyhow, Context, Result};
use serde_json::{Map, Value};

use crate::lockfile_solver::DepSpec;
use crate::mounts::Mount;
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
        if crate::ci::is_ci() {
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

/// One entry as stored on disk: the canonical dependency key it overrides,
/// the target path exactly as the user typed it, and the mount it applies
/// to (None for the default mount).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredLink {
    pub name: String,
    pub path: String,
    pub mount: Option<String>,
}

impl StoredLink {
    pub fn belongs_to(&self, mount: &Mount) -> bool {
        match &self.mount {
            None => mount.is_default(),
            Some(path) => !mount.is_default() && path.eq_ignore_ascii_case(&mount.path),
        }
    }
}

const MOUNT_LINKS: &str = "mounts";

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
    let entries = |links: &Map<String, Value>, mount: Option<&String>| -> Vec<StoredLink> {
        links
            .iter()
            .filter_map(|(name, entry)| {
                let path = entry.get("path").and_then(Value::as_str)?;
                Some(StoredLink { name: name.clone(), path: path.to_string(), mount: mount.cloned() })
            })
            .collect()
    };
    let mut out: Vec<StoredLink> = file
        .get("links")
        .and_then(Value::as_object)
        .map(|links| entries(links, None))
        .unwrap_or_default();
    if let Some(mounts) = file.get(MOUNT_LINKS).and_then(Value::as_object) {
        for (mount, links) in mounts {
            if let Some(links) = links.as_object() {
                out.extend(entries(links, Some(mount)));
            }
        }
    }
    out.sort_by(|a, b| (&a.mount, &a.name).cmp(&(&b.mount, &b.name)));
    out
}

/// The link map a mount's entries live in (None = default), created when
/// absent.
fn links_map_mut<'a>(file: &'a mut Value, mount: Option<&str>) -> &'a mut Map<String, Value> {
    let slot = match mount {
        None => {
            if !file.get("links").map_or(false, Value::is_object) {
                file["links"] = Value::Object(Map::new());
            }
            &mut file["links"]
        }
        Some(mount) => {
            if !file.get(MOUNT_LINKS).map_or(false, Value::is_object) {
                file[MOUNT_LINKS] = Value::Object(Map::new());
            }
            let mounts = file[MOUNT_LINKS].as_object_mut().expect("ensured above");
            let key = mounts
                .keys()
                .find(|k| k.eq_ignore_ascii_case(mount))
                .cloned()
                .unwrap_or_else(|| mount.to_string());
            let slot = mounts.entry(key).or_insert_with(|| Value::Object(Map::new()));
            if !slot.is_object() {
                *slot = Value::Object(Map::new());
            }
            slot
        }
    };
    slot.as_object_mut().expect("ensured above")
}

/// Add or replace the link for `name` in a mount (None = default). A
/// case-insensitive key match replaces in place, keeping the stored casing
/// stable.
pub fn upsert_link(manifest_dir: &Path, name: &str, path: &str, mount: Option<&str>) -> Result<()> {
    let mut file = read_file(manifest_dir).unwrap_or_else(|| Value::Object(Map::new()));
    let links = links_map_mut(&mut file, mount);
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

/// The stored links a reference names: by package (scope/name or bare
/// name, case-insensitive) or by the stored path (verbatim or resolving to
/// the same location).
pub fn matching_links(manifest_dir: &Path, stored: &[StoredLink], reference: &str) -> Vec<StoredLink> {
    let ref_resolved = fs::canonicalize(manifest_dir.join(reference)).ok();
    stored
        .iter()
        .filter(|link| {
            same_package(&link.name, reference)
                || link.name.rsplit('/').next().map_or(false, |n| n.eq_ignore_ascii_case(reference))
                || link.path == reference
                || (ref_resolved.is_some()
                    && fs::canonicalize(manifest_dir.join(&link.path)).ok() == ref_resolved)
        })
        .cloned()
        .collect()
}

/// Remove exactly one stored link.
pub fn remove_stored(manifest_dir: &Path, link: &StoredLink) -> Result<()> {
    let Some(mut file) = read_file(manifest_dir) else {
        return Ok(());
    };
    let links = links_map_mut(&mut file, link.mount.as_deref());
    let key = links.keys().find(|k| same_package(k, &link.name)).cloned();
    if let Some(key) = key {
        links.remove(&key);
    }
    drop_empty_mounts(&mut file);
    write_file(manifest_dir, file)
}

/// Remove every link in every mount. Returns what was removed.
pub fn remove_all(manifest_dir: &Path) -> Result<Vec<StoredLink>> {
    let removed = stored_links_in(manifest_dir);
    let Some(mut file) = read_file(manifest_dir) else {
        return Ok(Vec::new());
    };
    if !removed.is_empty() {
        file["links"] = Value::Object(Map::new());
        if let Some(obj) = file.as_object_mut() {
            obj.remove(MOUNT_LINKS);
        }
        write_file(manifest_dir, file)?;
    }
    Ok(removed)
}

/// Follow a mount rename: its links move with it.
pub fn rename_mount_links(manifest_dir: &Path, old_path: &str, new_path: &str) -> Result<()> {
    let Some(mut file) = read_file(manifest_dir) else {
        return Ok(());
    };
    let Some(mounts) = file.get_mut(MOUNT_LINKS).and_then(Value::as_object_mut) else {
        return Ok(());
    };
    let Some(key) = mounts.keys().find(|k| k.eq_ignore_ascii_case(old_path)).cloned() else {
        return Ok(());
    };
    let entry = mounts.remove(&key).expect("key came from the map");
    mounts.insert(new_path.to_string(), entry);
    write_file(manifest_dir, file)
}

/// Forget a removed mount's links. Returns what was dropped.
pub fn drop_mount_links(manifest_dir: &Path, mount_path: &str) -> Result<Vec<StoredLink>> {
    let dropped: Vec<StoredLink> = stored_links_in(manifest_dir)
        .into_iter()
        .filter(|l| l.mount.as_deref().map_or(false, |m| m.eq_ignore_ascii_case(mount_path)))
        .collect();
    if dropped.is_empty() {
        return Ok(dropped);
    }
    let Some(mut file) = read_file(manifest_dir) else {
        return Ok(Vec::new());
    };
    if let Some(mounts) = file.get_mut(MOUNT_LINKS).and_then(Value::as_object_mut) {
        mounts.retain(|k, _| !k.eq_ignore_ascii_case(mount_path));
    }
    drop_empty_mounts(&mut file);
    write_file(manifest_dir, file)?;
    Ok(dropped)
}

/// Keep the file tidy: no empty per-mount maps, no empty `mounts` field.
fn drop_empty_mounts(file: &mut Value) {
    let Some(mounts) = file.get_mut(MOUNT_LINKS).and_then(Value::as_object_mut) else {
        return;
    };
    mounts.retain(|_, links| links.as_object().map_or(false, |l| !l.is_empty()));
    if mounts.is_empty() {
        if let Some(obj) = file.as_object_mut() {
            obj.remove(MOUNT_LINKS);
        }
    }
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

/// Match a mount's stored links against its direct dependencies and check
/// each target is still readable. Links that don't match a dependency or
/// whose target is gone become warnings, never errors; install must keep
/// working with the registry graph.
pub fn resolve_active(root_deps: &HashMap<String, DepSpec>, mount: &Mount) -> LinkResolution {
    let stored: Vec<StoredLink> = stored_links().into_iter().filter(|l| l.belongs_to(mount)).collect();
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
            let (place, flag) = match &link.mount {
                None => (String::new(), String::new()),
                Some(path) => (format!(" of mount {}", path), format!(" --mount {}", path)),
            };
            res.warnings.push(format!(
                "Link for {} no longer matches a dependency{} in forest.json; run `forest unlink {}{}` to clean it up.",
                link.name, place, link.name, flag
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

        upsert_link(&dir, "acme/promise", "../promise", None).unwrap();

        let file: Value = serde_json::from_str(&fs::read_to_string(dir.join(LINKS_FILE)).unwrap()).unwrap();
        assert_eq!(file["futureField"]["keep"], Value::Bool(true), "unknown top-level fields survive");
        assert_eq!(file["links"]["acme/knit"]["extra"], Value::from(7), "unknown entry fields survive");
        assert_eq!(file["links"]["acme/knit"]["path"], Value::from("../knit"));
        assert_eq!(file["links"]["acme/promise"]["path"], Value::from("../promise"));
        assert_eq!(file["_comment"], Value::from(LINKS_COMMENT), "comment is normalized on write");

        // Case-insensitive upsert replaces in place, keeping the stored casing.
        upsert_link(&dir, "ACME/Knit", "../knit2", None).unwrap();
        let links = stored_links_in(&dir);
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].name, "acme/knit");
        assert_eq!(links[0].path, "../knit2");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn matching_finds_links_by_name_bare_name_and_path() {
        let dir = fixture("remove");
        upsert_link(&dir, "acme/knit", "../knit", None).unwrap();
        upsert_link(&dir, "acme/promise", "../promise", None).unwrap();
        let stored = stored_links_in(&dir);
        let names = |refr: &str| -> Vec<String> {
            matching_links(&dir, &stored, refr).into_iter().map(|l| l.name).collect()
        };

        assert_eq!(names("ACME/KNIT"), vec!["acme/knit"]);
        assert_eq!(names("promise"), vec!["acme/promise"]);
        assert_eq!(names("../knit"), vec!["acme/knit"], "verbatim path matches");
        assert!(names("acme/gone").is_empty());

        remove_stored(&dir, &matching_links(&dir, &stored, "knit")[0]).unwrap();
        assert_eq!(stored_links_in(&dir).len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mount_links_live_apart_from_default_links() {
        let dir = fixture("mount-links");
        upsert_link(&dir, "acme/signal", "../signal", None).unwrap();
        upsert_link(&dir, "acme/signal", "../signal-dev", Some("DevPackages")).unwrap();

        let file: Value = serde_json::from_str(&fs::read_to_string(dir.join(LINKS_FILE)).unwrap()).unwrap();
        assert_eq!(file["links"]["acme/signal"]["path"], "../signal", "older CLIs keep seeing the default link");
        assert_eq!(file["mounts"]["DevPackages"]["acme/signal"]["path"], "../signal-dev");

        let stored = stored_links_in(&dir);
        assert_eq!(stored.len(), 2);
        let both = matching_links(&dir, &stored, "signal");
        assert_eq!(both.len(), 2, "the same package linked in two mounts");

        rename_mount_links(&dir, "devpackages", "tools/Dev").unwrap();
        let moved = stored_links_in(&dir);
        assert!(moved.iter().any(|l| l.mount.as_deref() == Some("tools/Dev")));

        let dropped = drop_mount_links(&dir, "tools/Dev").unwrap();
        assert_eq!(dropped.len(), 1);
        let file: Value = serde_json::from_str(&fs::read_to_string(dir.join(LINKS_FILE)).unwrap()).unwrap();
        assert!(file.get("mounts").is_none(), "an emptied mounts map is dropped");
        assert_eq!(stored_links_in(&dir).len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_all_clears_and_reports() {
        let dir = fixture("remove-all");
        assert!(remove_all(&dir).unwrap().is_empty(), "no file is a clean no-op");
        upsert_link(&dir, "a/x", "../x", None).unwrap();
        upsert_link(&dir, "b/y", "../y", Some("DevPackages")).unwrap();
        let removed: Vec<String> = remove_all(&dir).unwrap().into_iter().map(|l| l.name).collect();
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
