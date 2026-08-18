//! Shared install orchestration: the lockfile format, dependency
//! resolution entry point, and the services every platform executor uses
//! (CDN base, signed-URL fetch, worker-pool sizing). The actual layout /
//! extraction / bookkeeping work is platform-owned and reached through
//! `Platform::install` (src/platform.rs) — this module contains no
//! platform-specific logic.

use std::collections::{HashMap, HashSet};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use urlencoding::encode;

use reqwest::Method;
use crate::http::packages_api_request;
use crate::platform::Platform;
use crate::utils::{digest_package_name, get_ci, normalize_forest_deps, normalize_forest_excludes, normalize_forest_overrides};
use crate::lockfile_solver::{get_lockfile_packages, DepSpec, LockfileEntry};
use crate::message::{Message, MessageType};


/// The overall lockfile structure.
#[derive(Debug, Serialize, Deserialize)]
pub struct LockFile {
    pub file_version: u32,
    /// The manifest overrides this resolution was solved under, so adding,
    /// changing, or removing an override invalidates the lockfile. Absent
    /// from disk when empty — pre-override lockfiles parse unchanged.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub overrides: HashMap<String, String>,
    /// Same recording for the manifest's excludes (banned version ranges).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub excludes: HashMap<String, String>,
    pub packages: HashMap<String, Vec<LockfileEntry>>,
}

/// Whether the lockfile still satisfies the manifest's declared dependencies.
/// Root deps pin their resolved version at the tree root (`location == "~"`),
/// so each declared range is checked against that pin, and a pin whose package
/// is no longer declared means the dep was removed by hand. Any mismatch (or
/// an unparseable range/version) sends install back through resolution, which
/// reports invalid ranges properly.
pub fn lockfile_satisfies_manifest(
    lockfile: &LockFile,
    roots: &HashMap<String, DepSpec>,
    overrides: &HashMap<String, String>,
    excludes: &HashMap<String, String>,
) -> bool {
    // The lockfile records the overrides/excludes it was solved under; any
    // drift (added, changed, or removed entry) forces re-resolution. Ranges
    // are compared verbatim — rewriting "^2.0" to "^2.0.0" is a re-solve,
    // which lands on the same versions anyway.
    let maps_match = |locked: &HashMap<String, String>, declared: &HashMap<String, String>| {
        locked.len() == declared.len()
            && declared.iter().all(|(name, range)| {
                get_ci(locked, name).map_or(false, |l| l.trim() == range.trim())
            })
    };
    if !maps_match(&lockfile.overrides, overrides) || !maps_match(&lockfile.excludes, excludes) {
        return false;
    }

    for (name, spec) in roots {
        let Ok(req) = semver::VersionReq::parse(&spec.version) else {
            return false;
        };
        let Some(root_entry) = get_ci(&lockfile.packages, name)
            .and_then(|entries| entries.iter().find(|e| e.location == "~"))
        else {
            return false;
        };
        let Ok(version) = semver::Version::parse(&root_entry.version) else {
            return false;
        };
        if !req.matches(&version) {
            return false;
        }
    }

    for (name, entries) in &lockfile.packages {
        if entries.iter().any(|e| e.location == "~") && get_ci(roots, name).is_none() {
            return false;
        }
    }

    true
}

/// Tarballs are content-addressed on the CDN (`/{public|private}/{sha256}.tgz`),
/// so public download URLs are derived from the lockfile's integrity hash rather
/// than stored in the lockfile. Overridable for local stacks, following
/// update.rs's FOREST_INSTALL_BASE convention.
const DEFAULT_CDN_BASE: &str = "https://registry.forest.dev";

pub(crate) fn cdn_base() -> String {
    std::env::var("FOREST_CDN_BASE").unwrap_or_else(|_| DEFAULT_CDN_BASE.to_string())
}

/// How many tarballs download (and signed URLs prefetch) at once. Bounded so
/// a large tree doesn't spawn hundreds of OS threads and TLS connections.
/// Used by both platform executors' worker pools.
pub(crate) const DOWNLOAD_WORKERS: usize = 8;

/// Fetch the short-lived signed download URL for one private package version,
/// cross-checking the registry's integrity hash against the lockfile's before
/// anything is downloaded.
pub(crate) async fn fetch_signed_url(
    pkg_name: String,
    version: String,
    lockfile_integrity: String,
    platform: String,
) -> Result<((String, String), String)> {
    let name = digest_package_name(&pkg_name);
    // Lowercased like every package URL. Only the legacy-public fallback
    // benefits at the edge cache (private responses are never cached), but
    // one URL convention keeps the key space enumerable for purging.
    let scope_lc = name.scope.to_lowercase();
    let name_lc = name.name.to_lowercase();
    let path = format!(
        "v1/package/{}/{}/{}/{}",
        encode(&scope_lc),
        encode(&platform),
        encode(&name_lc),
        encode(&version)
    );
    let (info, status) = packages_api_request(&path, Method::GET, None, None).await
        .with_context(|| format!("Failed to fetch access URL for {}@{}", pkg_name, version))?;
    if !status.is_success() {
        return Err(anyhow!(
            "Failed to fetch access URL for {}@{}: HTTP {}",
            pkg_name, version, status
        ));
    }
    let registry_integrity = info.get("integrity")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if !registry_integrity.eq_ignore_ascii_case(lockfile_integrity.trim()) {
        return Err(anyhow!(
            "Integrity mismatch for {}@{}: lockfile has {} but the registry reports {}. \
             Refusing to install. If this version was republished, delete forest-lock.json and re-run `forest install`.",
            pkg_name, version, lockfile_integrity, registry_integrity
        ));
    }
    let url = info.get("accessUrl")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Registry returned no access URL for {}@{}", pkg_name, version))?;
    Ok(((pkg_name, version), url.to_string()))
}

/// What an install run actually did — lets callers print "up to date"
/// instead of implying work happened.
pub struct InstallSummary {
    pub installed: usize,
    #[allow(dead_code)]
    pub kept: usize,
}

/// Materialize a lockfile on disk. Thin dispatcher: each platform owns its
/// entire layout/extraction/bookkeeping pipeline. Takes the whole manifest
/// (not just the platform string) because layout can depend on other
/// manifest fields — Roblox mounts Packages/ inside the `root` dir.
pub async fn make_directories(lockfile: &LockFile, root_deps: HashMap<String, DepSpec>, manifest: &Value, force: bool) -> Result<InstallSummary> {
    Platform::from_manifest(manifest)?.install(lockfile, root_deps, manifest, force).await
}

/// Generate a lockfile JSON string given the forest manifest & message spinner.
pub async fn lockfile_gen(forest_json: &Value, msg: &mut Message, force: bool) -> Result<LockFile> {
    let roots = normalize_forest_deps(forest_json);
    let platform: String = forest_json
        .get("platform")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing platform in forest.json"))?
        .to_string(); // clone the value so we don't hold a borrow

    // Platforms may widen the roots beyond the invoking manifest (UEFN
    // resolves the whole workspace: project manifest + authored packages).
    let roots = Platform::parse(&platform)?.resolution_roots(roots)?;
    let overrides = normalize_forest_overrides(forest_json);
    let excludes = normalize_forest_excludes(forest_json);

    msg.update("Resolving dependencies...");
    // --force also bypasses the metadata disk cache, like receipts at install.
    let (lockfile_packages, license_warnings, root_renames, solve_report) = get_lockfile_packages(roots.clone(), &overrides, &excludes, platform.clone(), msg, !force).await
        .context("Failed to resolve lockfile packages")?;

    if solve_report.override_edges > 0 {
        msg.emit(
            MessageType::Info,
            &format!(
                "Declared overrides modified {} edge{}. Run `forest tree` to view.",
                solve_report.override_edges,
                if solve_report.override_edges == 1 { "" } else { "s" }
            ),
        );
    }
    for key in &solve_report.override_unused {
        msg.emit(
            MessageType::Warn,
            &format!("Override for {} matched no dependency in the tree; remove it with `forest override {} --remove`.", key, key),
        );
    }
    for key in &solve_report.override_unnecessary {
        msg.emit(
            MessageType::Info,
            &format!("Override for {} is no longer needed — dependencies already resolve inside it. Remove it with `forest override {} --remove`.", key, key),
        );
    }
    for key in &solve_report.exclude_unused {
        msg.emit(
            MessageType::Warn,
            &format!("Exclusion for {} matched no dependency in the tree; remove it with `forest exclude {} --remove`.", key, key),
        );
    }
    for key in &solve_report.exclude_inert {
        msg.emit(
            MessageType::Info,
            &format!("Exclusion for {} no longer affects resolution — every range now picks an allowed version. Safe to remove with `forest exclude {} --remove`.", key, key),
        );
    }

    // A claimed/renamed scope resolves under its old name but the lockfile is keyed by the canonical one. re-key the roots to match
    let mut roots = roots;
    if !root_renames.is_empty() {
        let applied = rewrite_manifest_renames(&root_renames)?;
        for a in &applied {
            msg.emit(MessageType::Info, &a.notice);
        }
        let applied_by_key: HashMap<&str, &AppliedRename> =
            applied.iter().map(|a| (a.rename_key.as_str(), a)).collect();

        for (old_key, canonical) in &root_renames {
            if roots.keys().any(|k| k != old_key && k.eq_ignore_ascii_case(canonical)) {
                msg.emit(
                    MessageType::Warn,
                    &format!(
                        "{} and {} are the same package; remove {} from forest.json.",
                        old_key, canonical, old_key
                    ),
                );
                continue;
            }
            if let Some(mut spec) = roots.remove(old_key) {
                // Follow the manifest rewrite's explicit-alias decision; for
                // keys the local manifest doesn't hold (UEFN workspace roots)
                // a default-looking alias is treated as defaulted.
                let defaulted = applied_by_key
                    .get(old_key.as_str())
                    .map(|a| a.defaulted)
                    .unwrap_or_else(|| spec.alias == digest_package_name(old_key).name);
                if defaulted {
                    spec.alias = digest_package_name(canonical).name;
                }
                roots.insert(canonical.clone(), spec);
            }
        }
    }

    // Surface registry license-safety ratings for anything caution/unsafe in
    // the resolved tree (direct and transitive) before files land on disk.
    // One consolidated line — per-package details live in `forest audit`.
    if !license_warnings.is_empty() {
        let flagged: HashSet<&str> = license_warnings
            .iter()
            .map(|w| w.label.split('@').next().unwrap_or(&w.label))
            .collect();
        let count = flagged.len();
        msg.emit(
            MessageType::Warn,
            &format!(
                "{} package{} license considerations, please run `forest audit` to view.",
                count,
                if count == 1 { " has" } else { "s have" }
            ),
        );
    }

    let lockfile : LockFile = LockFile {
        file_version: 2,
        overrides,
        excludes,
        packages: lockfile_packages
    };

    // make_directories draws its own download bars — hide the spinner while
    // they own the terminal, or the two draw systems leave stuck lines.
    msg.pause();
    make_directories(&lockfile, roots, forest_json, force).await
        .context("Failed to create directories for lockfile packages")?;
    msg.resume();

    Ok(lockfile)
}

/// One claimed-scope rename actually applied to the local manifest.
pub(crate) struct AppliedRename {
    /// The rename map's key — also the key the resolution roots carry.
    pub rename_key: String,
    /// True when the dependency declared no explicit alias, so its install
    /// folder follows the (now canonical) package name.
    pub defaulted: bool,
    pub notice: String,
}

/// Persist claimed-scope renames into the manifest in the current directory
/// (every command chdirs to the manifest dir before resolving). Reads the
/// file fresh so only the dependency keys change. Keys the local manifest
/// doesn't declare are skipped without error — under UEFN the resolution
/// roots span other workspace manifests.
fn rewrite_manifest_renames(renames: &HashMap<String, String>) -> Result<Vec<AppliedRename>> {
    let path = "forest.json";
    if !std::path::Path::new(path).exists() {
        return Ok(Vec::new());
    }
    let mut manifest: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let applied = canonicalize_manifest_deps(&mut manifest, renames);
    if !applied.is_empty() {
        std::fs::write(path, serde_json::to_string_pretty(&manifest)?)?;
    }
    Ok(applied)
}

/// Re-key manifest dependencies whose registry identity is a different
/// package name (claimed/renamed scope). Pure JSON transform.
///
/// A dependency without an explicit alias deliberately follows the canonical
/// name after the rename: wally-era code requires the wally ALIAS (the
/// wally.toml key, e.g. `AnimNation`), which is the casing the claimed
/// native package carries — the old mirrored key's lowercase name was never
/// what that code referenced. An explicitly declared alias always survives
/// untouched.
pub(crate) fn canonicalize_manifest_deps(
    manifest: &mut Value,
    renames: &HashMap<String, String>,
) -> Vec<AppliedRename> {
    let mut applied = Vec::new();
    let Some(deps) = manifest.get_mut("dependencies").and_then(Value::as_object_mut) else {
        return applied;
    };

    for (old_key, canonical) in renames {
        // The manifest's own casing of the key wins over the caller's.
        let Some(manifest_key) = deps.keys().find(|k| k.eq_ignore_ascii_case(old_key)).cloned() else {
            continue;
        };
        if deps.keys().any(|k| *k != manifest_key && k.eq_ignore_ascii_case(canonical)) {
            // Both names are declared — the canonical entry already wins at
            // install time; merging two version ranges is the user's call.
            continue;
        }

        let value = deps.remove(&manifest_key).expect("key came from deps");
        let has_explicit_alias = value
            .as_object()
            .map_or(false, |o| o.get("alias").map_or(false, Value::is_string));

        deps.insert(canonical.clone(), value);
        applied.push(AppliedRename {
            rename_key: old_key.clone(),
            defaulted: !has_explicit_alias,
            notice: format!(
                "{} is now published as {}; forest.json updated.",
                manifest_key, canonical
            ),
        });
    }

    applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn renames(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    /// (package, version, location) triples -> a minimal lockfile.
    fn lockfile(entries: &[(&str, &str, &str)]) -> LockFile {
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
        LockFile { file_version: 2, overrides: HashMap::new(), excludes: HashMap::new(), packages }
    }

    fn overrides(pairs: &[(&str, &str)]) -> HashMap<String, String> {
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
    fn satisfied_lockfile_is_trusted() {
        let lf = lockfile(&[("a/b", "1.5.2", "~"), ("c/d", "0.3.0", "b")]);
        assert!(lockfile_satisfies_manifest(&lf, &roots(&[("a/b", "^1.5.0")]), &overrides(&[]), &overrides(&[])));
    }

    #[test]
    fn bumped_range_invalidates_the_lockfile() {
        // The reported bug: ^1.5.0 was installed, the manifest now says
        // ^2.0.0, and install kept saying "already up to date".
        let lf = lockfile(&[("a/b", "1.5.2", "~")]);
        assert!(!lockfile_satisfies_manifest(&lf, &roots(&[("a/b", "^2.0.0")]), &overrides(&[]), &overrides(&[])));
    }

    #[test]
    fn newly_declared_dep_invalidates_the_lockfile() {
        let lf = lockfile(&[("a/b", "1.5.2", "~")]);
        assert!(!lockfile_satisfies_manifest(
            &lf,
            &roots(&[("a/b", "^1.5.0"), ("c/d", "^0.3.0")]),
            &overrides(&[]),
            &overrides(&[]),
        ));
    }

    #[test]
    fn removed_dep_with_lingering_root_pin_invalidates_the_lockfile() {
        let lf = lockfile(&[("a/b", "1.5.2", "~"), ("c/d", "0.3.0", "~")]);
        assert!(!lockfile_satisfies_manifest(&lf, &roots(&[("a/b", "^1.5.0")]), &overrides(&[]), &overrides(&[])));
    }

    #[test]
    fn undeclared_transitive_entries_are_fine() {
        // c/d lives inside a/b's subtree, not at the root - it's a/b's
        // dependency, not a removed manifest entry.
        let lf = lockfile(&[("a/b", "1.5.2", "~"), ("c/d", "0.3.0", "b")]);
        assert!(lockfile_satisfies_manifest(&lf, &roots(&[("a/b", "^1.5.0")]), &overrides(&[]), &overrides(&[])));
    }

    #[test]
    fn key_casing_differences_still_match() {
        let lf = lockfile(&[("Scope/Pkg", "1.5.2", "~")]);
        assert!(lockfile_satisfies_manifest(&lf, &roots(&[("scope/pkg", "^1.5.0")]), &overrides(&[]), &overrides(&[])));
    }

    #[test]
    fn unparseable_range_forces_reresolution() {
        // The solver owns range errors; the check just refuses the fast path.
        let lf = lockfile(&[("a/b", "1.5.2", "~")]);
        assert!(!lockfile_satisfies_manifest(&lf, &roots(&[("a/b", "not-a-range")]), &overrides(&[]), &overrides(&[])));
    }

    #[test]
    fn added_override_invalidates_the_lockfile() {
        let lf = lockfile(&[("a/b", "1.5.2", "~")]);
        assert!(!lockfile_satisfies_manifest(
            &lf,
            &roots(&[("a/b", "^1.5.0")]),
            &overrides(&[("c/d", "^2.0.0")]),
            &overrides(&[]),
        ));
    }

    #[test]
    fn matching_override_keeps_the_lockfile_trusted() {
        let mut lf = lockfile(&[("a/b", "1.5.2", "~")]);
        lf.overrides = overrides(&[("c/d", "^2.0.0")]);
        assert!(lockfile_satisfies_manifest(
            &lf,
            &roots(&[("a/b", "^1.5.0")]),
            &overrides(&[("c/d", "^2.0.0")]),
            &overrides(&[]),
        ));
        // Case-insensitive keys, like every other package-name map.
        assert!(lockfile_satisfies_manifest(
            &lf,
            &roots(&[("a/b", "^1.5.0")]),
            &overrides(&[("C/D", "^2.0.0")]),
            &overrides(&[]),
        ));
    }

    #[test]
    fn changed_or_removed_override_invalidates_the_lockfile() {
        let mut lf = lockfile(&[("a/b", "1.5.2", "~")]);
        lf.overrides = overrides(&[("c/d", "^2.0.0")]);
        assert!(!lockfile_satisfies_manifest(
            &lf,
            &roots(&[("a/b", "^1.5.0")]),
            &overrides(&[("c/d", "^3.0.0")]),
            &overrides(&[]),
        ));
        assert!(!lockfile_satisfies_manifest(
            &lf,
            &roots(&[("a/b", "^1.5.0")]),
            &overrides(&[]),
            &overrides(&[]),
        ));
    }

    #[test]
    fn exclude_drift_invalidates_the_lockfile() {
        let mut lf = lockfile(&[("a/b", "1.5.2", "~")]);
        lf.excludes = overrides(&[("c/d", "=1.6.0")]);
        // Matching excludes keep the fast path.
        assert!(lockfile_satisfies_manifest(
            &lf,
            &roots(&[("a/b", "^1.5.0")]),
            &overrides(&[]),
            &overrides(&[("C/D", "=1.6.0")]),
        ));
        // Changed or removed excludes re-resolve.
        assert!(!lockfile_satisfies_manifest(
            &lf,
            &roots(&[("a/b", "^1.5.0")]),
            &overrides(&[]),
            &overrides(&[("c/d", "=1.6.1")]),
        ));
        assert!(!lockfile_satisfies_manifest(
            &lf,
            &roots(&[("a/b", "^1.5.0")]),
            &overrides(&[]),
            &overrides(&[]),
        ));
    }

    #[test]
    fn defaulted_alias_follows_the_canonical_name() {
        // The claimed-scope shape that surfaced this: wally's lowercase
        // mirror name becomes the natively-cased name on claim. Wally-era
        // code requires the wally ALIAS (`AnimNation`), which the canonical
        // name matches — the dep stays a plain string and the install
        // folder follows the new key's default.
        let mut manifest = json!({
            "dependencies": { "michaeldougal/animnation": "^1.11.0" }
        });
        let applied = canonicalize_manifest_deps(
            &mut manifest,
            &renames(&[("michaeldougal/animnation", "chiefwildin/AnimNation")]),
        );
        assert_eq!(applied.len(), 1);
        assert!(applied[0].defaulted);
        assert_eq!(
            manifest["dependencies"]["chiefwildin/AnimNation"],
            json!("^1.11.0")
        );
        assert!(manifest["dependencies"].get("michaeldougal/animnation").is_none());
    }

    #[test]
    fn explicit_alias_is_left_untouched() {
        let mut manifest = json!({
            "dependencies": {
                "oldscope/animnation": { "version": "^1.0.0", "alias": "Anim" }
            }
        });
        let applied = canonicalize_manifest_deps(
            &mut manifest,
            &renames(&[("oldscope/animnation", "newscope/AnimNation")]),
        );
        assert_eq!(applied.len(), 1);
        assert!(!applied[0].defaulted);
        assert_eq!(
            manifest["dependencies"]["newscope/AnimNation"],
            json!({ "version": "^1.0.0", "alias": "Anim" })
        );
    }

    #[test]
    fn skips_when_canonical_already_declared() {
        let mut manifest = json!({
            "dependencies": {
                "michaeldougal/animnation": "^1.11.0",
                "chiefwildin/AnimNation": "^1.14.0"
            }
        });
        let applied = canonicalize_manifest_deps(
            &mut manifest,
            &renames(&[("michaeldougal/animnation", "chiefwildin/AnimNation")]),
        );
        assert!(applied.is_empty());
        assert_eq!(manifest["dependencies"]["michaeldougal/animnation"], json!("^1.11.0"));
        assert_eq!(manifest["dependencies"]["chiefwildin/AnimNation"], json!("^1.14.0"));
    }

    #[test]
    fn skips_keys_the_local_manifest_does_not_declare() {
        // UEFN widens resolution roots with other workspace manifests' deps;
        // those renames must not error or touch this file.
        let mut manifest = json!({ "dependencies": { "a/b": "^1.0.0" } });
        let applied =
            canonicalize_manifest_deps(&mut manifest, &renames(&[("x/y", "z/y")]));
        assert!(applied.is_empty());
        assert_eq!(manifest["dependencies"]["a/b"], json!("^1.0.0"));
    }

    #[test]
    fn manifest_key_casing_wins_over_solver_casing() {
        let mut manifest = json!({
            "dependencies": { "MichaelDougal/AnimNation": "^1.11.0" }
        });
        let applied = canonicalize_manifest_deps(
            &mut manifest,
            &renames(&[("michaeldougal/animnation", "chiefwildin/AnimNation")]),
        );
        assert_eq!(applied.len(), 1);
        assert_eq!(
            manifest["dependencies"]["chiefwildin/AnimNation"],
            json!("^1.11.0")
        );
    }
}
