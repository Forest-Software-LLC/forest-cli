//! Install orchestration. A run decides per mount whether its section of
//! forest-lock.json can be installed as is or must be resolved again,
//! resolves, reports what the solver saw, installs every mount in the run
//! through the platform, and writes forest-lock.json (its only writer).
//! Also the download services every platform executor shares (CDN base,
//! signed-URL fetch). Layout, extraction, and bookkeeping are
//! platform-owned (`Platform::install`); nothing here is platform-specific.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use urlencoding::encode;

use reqwest::Method;
use crate::http::packages_api_request;
use crate::license_helper::LicenseInfo;
use crate::lockfile::{LockFile, LockSection, LockState};
use crate::install_report::DenyReason;
use crate::lockfile_solver::{get_lockfile_packages, locked_private, DepSpec, SolveReport};
use crate::message::{Message, MessageType};
use crate::mounts::Mount;
use crate::platform::Platform;
use crate::utils::{digest_package_name, normalize_forest_excludes, normalize_forest_overrides};

/// Tarballs are content-addressed on the CDN (`/{public|private}/{sha256}.tgz`),
/// so public download URLs are derived from the lockfile's integrity hash rather
/// than stored in the lockfile. Overridable for local stacks, following
/// update.rs's FOREST_INSTALL_BASE convention.
const DEFAULT_CDN_BASE: &str = "https://registry.forest.dev";

pub(crate) fn cdn_base() -> String {
    std::env::var("FOREST_CDN_BASE").unwrap_or_else(|_| DEFAULT_CDN_BASE.to_string())
}

/// Fetch the short-lived signed download URL for one private package version,
/// cross-checking the registry's integrity hash against the lockfile's before
/// anything is downloaded. A refusal comes back as an
/// `install_report::DeniedPackage` error so the pool can skip it.
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
        if let Some(denied) = crate::install_report::DeniedPackage::from_status(&pkg_name, &version, status) {
            return Err(denied.into());
        }
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

/// What an install run actually did, which lets callers print "up to date"
/// instead of implying work happened.
pub struct InstallSummary {
    pub installed: usize,
    #[allow(dead_code)]
    pub kept: usize,
}

/// Which mounts a run resolves from the registry even though their section
/// of forest-lock.json still satisfies forest.json. Stale sections always
/// resolve.
#[derive(Debug, Clone)]
pub enum Refresh {
    /// Nothing beyond the stale sections (install).
    Stale,
    /// Every mount in the run (update).
    All,
    /// These mounts, by path (a command just edited their dependencies).
    Mounts(Vec<String>),
}

impl Refresh {
    fn covers(&self, mount: &Mount) -> bool {
        match self {
            Refresh::Stale => false,
            Refresh::All => true,
            Refresh::Mounts(paths) => paths.iter().any(|p| p.eq_ignore_ascii_case(&mount.path)),
        }
    }
}

pub struct SyncOptions {
    /// Limit the run to one mount, by path. None runs every mount.
    pub only: Option<String>,
    pub refresh: Refresh,
    /// Reinstall everything, trusting no receipts and no metadata cache.
    pub force: bool,
    /// Fail instead of resolving anything or rewriting forest-lock.json.
    pub frozen: bool,
    /// Say why forest-lock.json is being regenerated (bulk install).
    pub announce: bool,
}

impl SyncOptions {
    pub fn new(only: Option<String>, refresh: Refresh) -> SyncOptions {
        SyncOptions { only, refresh, force: false, frozen: false, announce: false }
    }
}

pub struct SyncOutcome {
    pub lockfile: LockFile,
    /// Some mount was resolved rather than installed straight from
    /// forest-lock.json.
    pub resolved: bool,
    pub installed: usize,
}

/// One mount's part in a run.
struct Target<'a> {
    mount: &'a Mount,
    /// What the mount resolves against: its dependencies, re-keyed after
    /// renames (claimed scopes, renamed packages).
    roots: HashMap<String, DepSpec>,
    section: Option<LockSection>,
    resolved: bool,
}

/// Bring forest-lock.json and the installed mounts in line with forest.json.
pub async fn sync(manifest: &Value, msg: &mut Message, opts: &SyncOptions) -> Result<SyncOutcome> {
    let result = sync_mounts(manifest, msg, opts).await;
    if result.is_err() {
        // Clear the spinner so the error prints on a clean line.
        msg.pause();
    }
    result
}

/// `sync` after a command wrote its manifest edit. On failure forest.json
/// goes back to `manifest_before`. The edit has to be on disk first because
/// renames rewrite the file.
pub async fn sync_or_restore(
    manifest: &Value,
    manifest_before: &str,
    msg: &mut Message,
    opts: &SyncOptions,
) -> Result<SyncOutcome> {
    // Absolute: a UEFN install moves the working directory to Content/.
    let manifest_path: PathBuf = std::env::current_dir()?.join("forest.json");
    match sync(manifest, msg, opts).await {
        Ok(outcome) => Ok(outcome),
        Err(e) => {
            std::fs::write(&manifest_path, manifest_before).context("Failed to restore forest.json")?;
            Err(e.context("Couldn't apply the change, so forest.json was left as it was"))
        }
    }
}

async fn sync_mounts(manifest: &Value, msg: &mut Message, opts: &SyncOptions) -> Result<SyncOutcome> {
    let platform = Platform::from_manifest(manifest)?;
    let mounts = crate::mounts::project_mounts(manifest, platform)?;
    let overrides = normalize_forest_overrides(manifest);
    let excludes = normalize_forest_excludes(manifest);
    let in_run = |mount: &Mount| opts.only.as_ref().map_or(true, |p| p.eq_ignore_ascii_case(&mount.path));

    let existing = match LockFile::read()? {
        LockState::Current(lockfile) => Some(lockfile),
        LockState::Missing => {
            if opts.announce {
                msg.emit(MessageType::Warn, "No lockfile found. Commit forest-lock.json to avoid inconsistencies.");
            }
            None
        }
        LockState::Outdated => {
            if opts.announce {
                msg.emit(MessageType::Warn, "Lockfile format is out of date; regenerating forest-lock.json.");
            }
            None
        }
    };

    // A section is only trusted while it still satisfies the mount's
    // declared ranges: a hand-edited range (^1.5.0 -> ^2.0.0) or a removed
    // dependency resolves again instead of silently keeping the old pin.
    let mut targets: Vec<Target> = Vec::new();
    let mut stale: Vec<&str> = Vec::new();
    for mount in mounts.iter().filter(|m| in_run(m)) {
        let roots = platform.resolution_roots(mount.deps.clone())?;
        let section = existing.as_ref().and_then(|lf| lf.section(mount));
        let satisfied = section.map_or(false, |s| s.satisfies(&roots, &overrides, &excludes));
        if existing.is_some() && !satisfied {
            stale.push(&mount.path);
        }
        let section = section.filter(|_| satisfied && !opts.refresh.covers(mount)).cloned();
        targets.push(Target { mount, roots, section, resolved: false });
    }

    // Sections for mounts forest.json no longer declares. A full run drops
    // them (the lockfile is the only record of where the folder was, so say
    // so); a run limited to one mount leaves them alone.
    let orphaned: Vec<String> = existing
        .as_ref()
        .map(|lf| {
            lf.mounts
                .keys()
                .filter(|key| !mounts.iter().any(|m| !m.is_default() && m.path.eq_ignore_ascii_case(key)))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let drop_orphans = opts.only.is_none() && !orphaned.is_empty();

    if opts.frozen && (targets.iter().any(|t| t.section.is_none()) || drop_orphans) {
        bail!("--frozen: forest-lock.json is missing or out of date with forest.json. Run `forest install` locally and commit the lockfile.");
    }
    // Local links into a folder that is no longer a mount can't apply, and
    // their junctions would sit in a folder the user may delete with a tool
    // that deletes through them. A full run clears both.
    if opts.only.is_none() {
        let mut stale_links: Vec<String> = crate::links::stored_links()
            .into_iter()
            .filter_map(|l| l.mount)
            .filter(|path| !mounts.iter().any(|m| !m.is_default() && m.path.eq_ignore_ascii_case(path)))
            .collect();
        stale_links.sort_by_key(|p| p.to_ascii_lowercase());
        stale_links.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        for path in &stale_links {
            platform.remove_link_slots(path)?;
            let dropped = crate::links::drop_mount_links(Path::new("."), path)?;
            msg.emit(
                MessageType::Info,
                &format!(
                    "Dropped {} local link{} into {}/, which is no longer a mount.",
                    dropped.len(),
                    if dropped.len() == 1 { "" } else { "s" },
                    path
                ),
            );
        }
    }
    if opts.announce && !stale.is_empty() {
        let note = if mounts.len() == 1 {
            "forest.json dependencies changed; updating forest-lock.json.".to_string()
        } else {
            format!("forest.json dependencies changed ({}); updating forest-lock.json.", stale.join(", "))
        };
        msg.emit(MessageType::Info, &note);
    }
    if drop_orphans {
        for path in &orphaned {
            msg.emit(
                MessageType::Warn,
                &format!(
                    "Mount {} is no longer declared in forest.json, so forest stopped managing {}/. Delete the folder if you don't need it.",
                    path, path
                ),
            );
        }
    }

    // Resolve every target without a trusted section.
    let multi = mounts.len() > 1;
    let mut reports: Vec<SolveReport> = Vec::new();
    let mut license_warnings: Vec<LicenseInfo> = Vec::new();
    for target in targets.iter_mut().filter(|t| t.section.is_none()) {
        // Fallback pins for private packages the registry refuses.
        let locked = existing
            .as_ref()
            .and_then(|lf| lf.section(target.mount))
            .map(|s| locked_private(&s.packages))
            .unwrap_or_default();
        msg.update(&if multi {
            format!("Resolving {}...", target.mount.path)
        } else {
            "Resolving dependencies...".to_string()
        });
        // --force also bypasses the metadata disk cache, like receipts at install.
        let (packages, warnings, renames, report) = get_lockfile_packages(
            target.roots.clone(),
            &overrides,
            &excludes,
            &locked,
            platform.as_str().to_string(),
            msg,
            !opts.force,
        )
        .await
        .with_context(|| {
            if multi {
                format!("Failed to resolve mount {}", target.mount.path)
            } else {
                "Failed to resolve lockfile packages".to_string()
            }
        })?;
        if !renames.is_empty() {
            crate::renames::apply_renames(target.mount, &mut target.roots, &renames, msg)?;
        }
        license_warnings.extend(warnings);
        reports.push(report);
        target.section = Some(LockSection { overrides: overrides.clone(), excludes: excludes.clone(), packages });
        target.resolved = true;
    }
    if !reports.is_empty() {
        let every_mount = reports.len() == mounts.len();
        let mut report = merge_reports(reports);
        // Calling an override or exclusion unused or unneeded takes every
        // mount's graph. A mount kept from the lockfile or left out of the
        // run wasn't consulted and may be the one relying on it.
        if !every_mount {
            report.override_unused.clear();
            report.override_unnecessary.clear();
            report.exclude_unused.clear();
            report.exclude_inert.clear();
        }
        report_solve(&report, msg);
    }

    // Surface registry license-safety ratings for anything caution/unsafe in
    // the resolved trees (direct and transitive) before files land on disk.
    // One consolidated line; per-package details live in `forest audit`.
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

    // The new lockfile: this run's sections, plus every other mount's
    // carried over untouched.
    let mut default_section: Option<LockSection> = None;
    let mut mount_sections: BTreeMap<String, LockSection> = BTreeMap::new();
    let mut place = |mount: &Mount, section: LockSection| {
        if mount.is_default() {
            default_section = Some(section);
        } else {
            mount_sections.insert(mount.path.clone(), section);
        }
    };
    for target in &targets {
        place(target.mount, target.section.clone().expect("every target was kept or resolved"));
    }
    if let Some(lf) = &existing {
        for mount in mounts.iter().filter(|m| !in_run(m)) {
            if let Some(section) = lf.section(mount) {
                place(mount, section.clone());
            }
        }
        if !drop_orphans {
            for path in &orphaned {
                mount_sections.insert(path.clone(), lf.mounts[path].clone());
            }
        }
    }
    let lockfile = LockFile::new(default_section.unwrap_or_default(), mount_sections);

    // Platform installs draw their own download bars; hide the spinner
    // while they own the terminal, or the two draw systems leave stuck
    // lines.
    msg.pause();
    platform.check_mounts(&mounts, &orphaned);
    let mut installed = 0;
    for target in &targets {
        let section = target.section.as_ref().expect("every target was kept or resolved");
        let summary = platform
            .install(section, target.roots.clone(), target.mount, opts.force)
            .await
            .with_context(|| {
                if multi {
                    format!("Failed to install mount {}", target.mount.path)
                } else {
                    "Failed to create directories for lockfile packages".to_string()
                }
            })?;
        installed += summary.installed;
    }
    msg.resume();

    let resolved = targets.iter().any(|t| t.resolved);
    if !opts.frozen && (resolved || drop_orphans) {
        lockfile.save()?;
    }
    Ok(SyncOutcome { lockfile, resolved, installed })
}

/// Follow `forest mount rename`/`remove` in forest-lock.json without
/// resolving anything: an extra mount's section moves to `new_path`, or
/// goes with the mount.
pub fn move_mount_section(old_path: &str, new_path: Option<&str>) -> Result<()> {
    let Some(mut lockfile) = LockFile::load() else {
        return Ok(());
    };
    let Some(key) = lockfile.mounts.keys().find(|k| k.eq_ignore_ascii_case(old_path)).cloned() else {
        return Ok(());
    };
    let section = lockfile.mounts.remove(&key).expect("key came from the map");
    if let Some(new_path) = new_path {
        lockfile.mounts.insert(new_path.to_string(), section);
    }
    lockfile.save()
}

/// The solver's notes on overrides, excludes, and private pins, once per
/// run however many mounts resolved.
fn report_solve(report: &SolveReport, msg: &mut Message) {
    for (pinned, reason) in &report.locked_private {
        let text = match reason {
            DenyReason::NotLoggedIn => format!(
                "Not logged in, so private package {} kept the version pinned in forest-lock.json. Run `forest login` to update it.",
                pinned
            ),
            DenyReason::SessionRejected => format!(
                "Your login was rejected, so private package {} kept the version pinned in forest-lock.json. Run `forest login` to update it.",
                pinned
            ),
            DenyReason::NoAccess => format!(
                "No access to private package {} (your account can't read it, or it no longer exists); kept the version pinned in forest-lock.json. Ask its maintainer for access to update it.",
                pinned
            ),
        };
        msg.emit(MessageType::Warn, &text);
    }
    if report.override_edges > 0 {
        msg.emit(
            MessageType::Info,
            &format!(
                "Declared overrides modified {} edge{}. Run `forest tree` to view.",
                report.override_edges,
                if report.override_edges == 1 { "" } else { "s" }
            ),
        );
    }
    for key in &report.override_unused {
        msg.emit(
            MessageType::Warn,
            &format!("Override for {} matched no dependency in the tree; remove it with `forest override {} --remove`.", key, key),
        );
    }
    for key in &report.override_unnecessary {
        msg.emit(
            MessageType::Info,
            &format!("Override for {} is no longer needed; dependencies already resolve inside it. Remove it with `forest override {} --remove`.", key, key),
        );
    }
    for key in &report.exclude_unused {
        msg.emit(
            MessageType::Warn,
            &format!("Exclusion for {} matched no dependency in the tree; remove it with `forest exclude {} --remove`.", key, key),
        );
    }
    for key in &report.exclude_inert {
        msg.emit(
            MessageType::Info,
            &format!("Exclusion for {} no longer affects resolution; every range now picks an allowed version. Safe to remove with `forest exclude {} --remove`.", key, key),
        );
    }
}

/// One report for a run that resolved several mounts with the same
/// overrides and excludes. One is unused only when no mount's graph reached
/// it, and unnecessary (or inert) only when every mount that reached it
/// agrees.
fn merge_reports(reports: Vec<SolveReport>) -> SolveReport {
    let in_every = |pick: fn(&SolveReport) -> &Vec<String>| -> Vec<String> {
        let mut keys: Vec<String> = reports
            .first()
            .map(|first| pick(first).iter().filter(|k| reports.iter().all(|r| pick(r).contains(k))).cloned().collect())
            .unwrap_or_default();
        keys.sort();
        keys
    };
    let settled = |pick: fn(&SolveReport) -> &Vec<String>, unused: fn(&SolveReport) -> &Vec<String>| -> Vec<String> {
        let mut keys: Vec<String> = reports
            .iter()
            .flat_map(|r| pick(r).iter())
            .filter(|k| reports.iter().all(|r| pick(r).contains(k) || unused(r).contains(k)))
            .cloned()
            .collect();
        keys.sort();
        keys.dedup();
        keys
    };
    let mut locked_private: Vec<(String, DenyReason)> = reports.iter().flat_map(|r| r.locked_private.iter().cloned()).collect();
    locked_private.sort();
    locked_private.dedup();
    SolveReport {
        override_edges: reports.iter().map(|r| r.override_edges).sum(),
        override_unused: in_every(|r| &r.override_unused),
        override_unnecessary: settled(|r| &r.override_unnecessary, |r| &r.override_unused),
        exclude_unused: in_every(|r| &r.exclude_unused),
        exclude_inert: settled(|r| &r.exclude_inert, |r| &r.exclude_unused),
        locked_private,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn keys(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_single_report_merges_to_itself() {
        let report = SolveReport {
            override_edges: 3,
            override_unused: keys(&["a/unused"]),
            override_unnecessary: keys(&["a/loose"]),
            exclude_unused: keys(&["b/unused"]),
            exclude_inert: keys(&["b/inert"]),
            locked_private: vec![("p/q@1.0.0".to_string(), DenyReason::NoAccess)],
        };
        let merged = merge_reports(vec![report]);
        assert_eq!(merged.override_edges, 3);
        assert_eq!(merged.override_unused, keys(&["a/unused"]));
        assert_eq!(merged.override_unnecessary, keys(&["a/loose"]));
        assert_eq!(merged.exclude_unused, keys(&["b/unused"]));
        assert_eq!(merged.exclude_inert, keys(&["b/inert"]));
        assert_eq!(merged.locked_private, vec![("p/q@1.0.0".to_string(), DenyReason::NoAccess)]);
    }

    #[test]
    fn a_constraint_one_mount_uses_is_not_reported_unused() {
        // The server mount reaches both packages; the dev mount reaches
        // neither. Neither constraint is unused project-wide.
        let server = SolveReport {
            override_edges: 2,
            override_unused: vec![],
            exclude_unused: vec![],
            ..SolveReport::default()
        };
        let dev = SolveReport {
            override_unused: keys(&["a/sig"]),
            exclude_unused: keys(&["b/pro"]),
            ..SolveReport::default()
        };
        let merged = merge_reports(vec![server, dev]);
        assert_eq!(merged.override_edges, 2);
        assert!(merged.override_unused.is_empty());
        assert!(merged.exclude_unused.is_empty());

        // Unused everywhere stays unused.
        let a = SolveReport { override_unused: keys(&["a/sig"]), ..SolveReport::default() };
        let b = SolveReport { override_unused: keys(&["a/sig"]), ..SolveReport::default() };
        assert_eq!(merge_reports(vec![a, b]).override_unused, keys(&["a/sig"]));
    }

    #[test]
    fn unnecessary_and_inert_need_every_mount_that_reached_them_to_agree() {
        // Unnecessary in one mount, unused in the other: still unnecessary.
        let a = SolveReport { override_unnecessary: keys(&["a/sig"]), exclude_inert: keys(&["b/pro"]), ..SolveReport::default() };
        let b = SolveReport { override_unused: keys(&["a/sig"]), exclude_unused: keys(&["b/pro"]), ..SolveReport::default() };
        let merged = merge_reports(vec![a, b]);
        assert_eq!(merged.override_unnecessary, keys(&["a/sig"]));
        assert_eq!(merged.exclude_inert, keys(&["b/pro"]));

        // Load-bearing in one mount: needed.
        let a = SolveReport { override_unnecessary: keys(&["a/sig"]), exclude_inert: keys(&["b/pro"]), ..SolveReport::default() };
        let b = SolveReport::default();
        let merged = merge_reports(vec![a, b]);
        assert!(merged.override_unnecessary.is_empty());
        assert!(merged.exclude_inert.is_empty());
    }
}
