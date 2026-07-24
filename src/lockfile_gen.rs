//! Shared install orchestration: the lockfile format, dependency
//! resolution entry point, and the services every platform executor uses
//! (CDN base, signed-URL fetch, worker-pool sizing). The actual layout /
//! extraction / bookkeeping work is platform-owned and reached through
//! `Platform::install` (src/platform.rs) — this module contains no
//! platform-specific logic.

use std::collections::HashMap;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use urlencoding::encode;

use reqwest::Method;
use crate::http::packages_api_request;
use crate::platform::Platform;
use crate::utils::{digest_package_name, normalize_forest_deps};
use crate::lockfile_solver::{get_lockfile_packages, DepSpec, LockfileEntry};
use crate::message::{Message, MessageType};


/// The overall lockfile structure.
#[derive(Debug, Serialize, Deserialize)]
pub struct LockFile {
    pub file_version: u32,
    pub packages: HashMap<String, Vec<LockfileEntry>>,
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
    let path = format!(
        "v1/package/{}/{}/{}/{}",
        encode(&name.scope),
        encode(&platform),
        encode(&name.name),
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
/// entire layout/extraction/bookkeeping pipeline.
pub async fn make_directories(lockfile: &LockFile, root_deps: HashMap<String, DepSpec>, platform: &str, force: bool) -> Result<InstallSummary> {
    Platform::parse(platform)?.install(lockfile, root_deps, force).await
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

    msg.update("Resolving dependencies...");
    let (lockfile_packages, license_warnings, root_renames) = get_lockfile_packages(roots.clone(), platform.clone()).await
        .context("Failed to resolve lockfile packages")?;

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
    // One line per package — the full caveats live in `forest audit`.
    for warning in &license_warnings {
        msg.emit(MessageType::Warn, &warning.headline());
    }
    if !license_warnings.is_empty() {
        msg.emit(
            MessageType::Info,
            "Run `forest audit` for license details (automated review, not legal advice).",
        );
    }

    let lockfile : LockFile = LockFile {
        file_version: 2,
        packages: lockfile_packages
    };

    // make_directories draws its own download bars — hide the spinner while
    // they own the terminal, or the two draw systems leave stuck lines.
    msg.pause();
    make_directories(&lockfile, roots, &platform, force).await
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
