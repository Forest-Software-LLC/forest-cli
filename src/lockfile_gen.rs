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
    let (lockfile_packages, license_warnings) = get_lockfile_packages(roots.clone(), platform.clone()).await
        .context("Failed to resolve lockfile packages")?;

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
