use std::{collections::HashMap, fs};
use anyhow::Result;
use colored::Colorize;
use reqwest::Method;
use semver::{Version, VersionReq};
use serde_json::Value;
use urlencoding::encode;

use crate::http::api_request;
use crate::license_helper::{extract_license_info, LicenseInfo};
use crate::lockfile::{LockFile, LockSection};
use crate::lockfile_gen::{sync_or_restore, Refresh, SyncOptions};
use crate::lockfile_solver::DepSpec;
use crate::message::{self, Message, MessageType};
use crate::mounts::{DepLocation, Mount};
use crate::utils::digest_package_name;

struct AuditRow {
    /// Index into the project's mounts.
    mount: usize,
    name: String,
    current: Option<Version>,
    wanted: Option<Version>,
    latest: Version,
}

/// How the optional package argument resolved against the project.
enum AuditTarget {
    /// No package argument: audit everything.
    All,
    /// A direct dependency: (mount index, manifest key) for each mount
    /// declaring it.
    Roots(Vec<(usize, String)>),
    /// Installed, but only as a transitive dependency (canonical lockfile key).
    Transitive(String),
}

/// The locked version of each root dependency (location "~") in a section.
fn locked_versions(section: Option<&LockSection>) -> HashMap<String, Version> {
    let mut locked = HashMap::new();
    let Some(section) = section else {
        return locked;
    };
    for name in section.packages.keys() {
        if let Some(version) = section.pinned_version(name).and_then(|v| Version::parse(v).ok()) {
            locked.insert(name.clone(), version);
        }
    }
    locked
}

/// Print one table of outdated rows (pad before coloring; ANSI codes break
/// width padding).
fn print_table(rows: &[&AuditRow]) {
    let fmt_opt = |v: &Option<Version>| v.as_ref().map_or("-".to_string(), |v| v.to_string());
    let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(0).max("Package".len());
    let cur_w = rows.iter().map(|r| fmt_opt(&r.current).len()).max().unwrap_or(0).max("Current".len());
    let want_w = rows.iter().map(|r| fmt_opt(&r.wanted).len()).max().unwrap_or(0).max("Wanted".len());
    let lat_w = rows.iter().map(|r| r.latest.to_string().len()).max().unwrap_or(0).max("Latest".len());

    println!(
        "  {}",
        format!(
            "{:<name_w$}  {:>cur_w$}  {:>want_w$}  {:>lat_w$}",
            "Package", "Current", "Wanted", "Latest"
        )
        .bold()
    );
    for row in rows {
        println!(
            "  {}  {:>cur_w$}  {}  {}",
            format!("{:<name_w$}", row.name).cyan(),
            fmt_opt(&row.current),
            format!("{:>want_w$}", fmt_opt(&row.wanted)).yellow(),
            format!("{:>lat_w$}", row.latest).green(),
        );
    }
}

/// Render one flagged package. Color only the accents; caveat text stays in
/// the terminal's default color so long lists remain readable.
fn render_license_block(info: &LicenseInfo) -> String {
    let severity = match info.rating.as_str() {
        "unsafe" => "legal risk for closed-source games".red().bold(),
        _ => "usable with conditions".yellow(),
    };
    let mut out = format!(
        "  {} {} {} {} {}",
        info.label.cyan(),
        "·".dimmed(),
        info.license.bold(),
        "·".dimmed(),
        severity
    );
    for caveat in &info.caveats {
        out.push_str(&format!("\n      {} {}", "•".dimmed(), caveat));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn license_block_puts_caveats_on_plain_indented_lines() {
        // Deterministic output regardless of the test terminal
        colored::control::set_override(false);
        let block = render_license_block(&LicenseInfo {
            label: "scope/pkg@1.2.3".to_string(),
            license: "GPL-3.0".to_string(),
            rating: "unsafe".to_string(),
            caveats: vec!["First caveat.".to_string(), "Second caveat.".to_string()],
        });
        colored::control::unset_override();

        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(lines[0], "  scope/pkg@1.2.3 · GPL-3.0 · legal risk for closed-source games");
        assert_eq!(lines[1], "      • First caveat.");
        assert_eq!(lines[2], "      • Second caveat.");
    }

    #[test]
    fn caution_block_uses_softer_severity_text() {
        colored::control::set_override(false);
        let block = render_license_block(&LicenseInfo {
            label: "scope/pkg@2.0.0".to_string(),
            license: "Apache-2.0".to_string(),
            rating: "caution".to_string(),
            caveats: vec![],
        });
        colored::control::unset_override();

        assert_eq!(block, "  scope/pkg@2.0.0 · Apache-2.0 · usable with conditions");
    }
}

/// Check dependencies for available updates and license considerations. With
/// `target_package`, limit the audit to that dependency; with `mount`, to
/// one mount. With `update`, bump forest.json to the latest versions and
/// reinstall.
pub async fn audit_command(target_package: Option<String>, update: bool, mount: Option<String>) -> Result<()> {
    // Discovery included: audit historically read forest.json from the cwd
    // only, so it failed where install worked (UEFN keeps the manifest in
    // Content/).
    let Some(project) = super::context::load_project()? else {
        crate::message::fail("No forest.json found. Run `forest init` to create a new package.");
        return Ok(());
    };
    let mut info = project.manifest;
    let mounts = crate::mounts::project_mounts(&info, project.platform)?;
    let scope = mount.as_deref().map(|r| crate::mounts::find_mount(&mounts, r)).transpose()?;
    let audited: Vec<usize> = (0..mounts.len())
        .filter(|&i| scope.map_or(true, |s| s.path == mounts[i].path))
        .collect();
    let mut msg = Message::new("Checking for updates...");

    let platform = info
        .get("platform")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    if audited.iter().all(|&i| mounts[i].deps.is_empty()) {
        msg.finish(MessageType::Info, "No dependencies to audit.");
        return Ok(());
    }

    // Declared excludes filter every candidate list below, so the report
    // never shows a version the solver would refuse and -u never bumps to
    // one. A bad exclude range only degrades the report; install fails
    // loudly on it.
    let mut excludes: HashMap<String, VersionReq> = HashMap::new();
    for (pkg, range) in crate::utils::normalize_forest_excludes(&info) {
        match VersionReq::parse(&range) {
            Ok(req) => {
                excludes.insert(pkg.to_lowercase(), req);
            }
            Err(_) => msg.emit(
                MessageType::Warn,
                &format!("Invalid exclude range {} for {} in forest.json; ignoring it for this report.", range, pkg),
            ),
        }
    }

    // Read the lockfile once: root pins feed the updates table, and the full
    // resolved trees (direct + transitive) feed the license report.
    let lockfile = LockFile::load();
    let section_of = |i: usize| lockfile.as_ref().and_then(|lf| lf.section(&mounts[i]));
    let index_of = |m: &Mount| mounts.iter().position(|x| x.path == m.path).expect("mount came from the list");

    // The reference may be the full scope/name, the alias, or the bare name.
    let target = match &target_package {
        None => AuditTarget::All,
        Some(raw) => match crate::mounts::locate_dep(&mounts, scope, raw) {
            DepLocation::Found(m, key) => AuditTarget::Roots(vec![(index_of(m), key)]),
            DepLocation::InSeveral(found) => {
                AuditTarget::Roots(found.into_iter().map(|(m, key)| (index_of(m), key)).collect())
            }
            DepLocation::Ambiguous(candidates) => {
                msg.finish(
                    MessageType::Warn,
                    &format!(
                        "\"{}\" matches more than one dependency: {}. Use the full <scope>/<name>.",
                        raw,
                        candidates.join(", ")
                    ),
                );
                return Ok(());
            }
            DepLocation::NotFound => {
                // Not a declared dep - maybe transitive, so try the
                // lockfile's resolved trees (full key or bare name; lockfile
                // entries carry no aliases).
                let name = raw.as_str();
                let keys = lockfile.as_ref().map(LockFile::package_keys).unwrap_or_default();
                let candidates: Vec<&String> = keys
                    .iter()
                    .filter(|k| {
                        k.eq_ignore_ascii_case(name)
                            || (!name.contains('/')
                                && k.rsplit('/').next().map_or(false, |n| n.eq_ignore_ascii_case(name)))
                    })
                    .collect();
                match candidates.as_slice() {
                    [key] => AuditTarget::Transitive((*key).clone()),
                    [] => {
                        msg.finish(
                            MessageType::Fail,
                            &format!("Package {} is not a dependency of this project.", name),
                        );
                        return Ok(());
                    }
                    many => {
                        let keys: Vec<&str> = many.iter().map(|k| k.as_str()).collect();
                        msg.finish(
                            MessageType::Warn,
                            &format!(
                                "\"{}\" matches more than one dependency: {}. Use the full <scope>/<name>.",
                                raw,
                                keys.join(", ")
                            ),
                        );
                        return Ok(());
                    }
                }
            }
        },
    };

    let check_roots: Vec<(usize, &String, &DepSpec)> = match &target {
        AuditTarget::All => audited
            .iter()
            .flat_map(|&i| mounts[i].deps.iter().map(move |(k, spec)| (i, k, spec)))
            .collect(),
        AuditTarget::Roots(roots) => roots
            .iter()
            .filter_map(|(i, key)| mounts[*i].deps.get_key_value(key).map(|(k, spec)| (*i, k, spec)))
            .collect(),
        AuditTarget::Transitive(_) => Vec::new(),
    };

    // ---- Update check ----
    // One version-list fetch per package, however many mounts declare it.
    let mut published: HashMap<String, Option<Vec<Version>>> = HashMap::new();
    let mut rows: Vec<AuditRow> = Vec::new();
    for &(i, name, spec) in &check_roots {
        let lc = name.to_lowercase();
        if !published.contains_key(&lc) {
            let versions = fetch_published(name, &platform, &excludes, &mut msg).await;
            published.insert(lc.clone(), versions);
        }
        let Some(versions) = published[&lc].as_ref() else {
            continue;
        };

        // Latest stable release; fall back to the newest prerelease when the
        // package has no stable versions yet.
        let latest = versions
            .iter()
            .rev()
            .find(|v| v.pre.is_empty())
            .unwrap_or_else(|| versions.last().unwrap())
            .clone();

        // Newest version that still satisfies the declared range.
        let wanted = VersionReq::parse(&spec.version)
            .ok()
            .and_then(|req| versions.iter().rev().find(|v| req.matches(v)).cloned());

        rows.push(AuditRow {
            mount: i,
            name: name.clone(),
            // Lockfile keys are canonical; the manifest key may differ by case
            current: crate::utils::get_ci(&locked_versions(section_of(i)), name).cloned(),
            wanted,
            latest,
        });
    }

    let mut outdated: Vec<&AuditRow> = rows
        .iter()
        .filter(|row| match (&row.current, &row.wanted) {
            (Some(current), _) => row.latest > *current,
            (None, Some(wanted)) => row.latest > *wanted,
            (None, None) => true,
        })
        .collect();
    outdated.sort_by(|a, b| (a.mount, &a.name).cmp(&(b.mount, &b.name)));

    msg.destroy();

    if let AuditTarget::Transitive(key) = &target {
        message::info(&format!(
            "{} is not a direct dependency; checking its installed license only.",
            key
        ));
    } else if outdated.is_empty() {
        // Skip the all-clear when every fetch failed; the warnings above tell
        // the real story.
        if !rows.is_empty() {
            match &target {
                AuditTarget::Roots(roots) => message::success(&format!("{} is up to date.", roots[0].1)),
                _ => message::success("All dependencies are up to date!"),
            }
        }
    } else {
        message::warn(&format!("{} package(s) have updates available:", outdated.len()));
        println!();
        if mounts.len() == 1 {
            print_table(&outdated);
            println!();
        } else {
            for &i in &audited {
                let group: Vec<&AuditRow> = outdated.iter().copied().filter(|r| r.mount == i).collect();
                if group.is_empty() {
                    continue;
                }
                println!("  {}", mounts[i].label().bold());
                print_table(&group);
                println!();
            }
        }
    }

    // ---- License check ----
    // Prefer the lockfile's resolved trees so transitive dependencies are
    // covered (matching what install warns about); fall back to the direct
    // dependencies' resolved-range versions when no lockfile exists.
    let mut pairs: Vec<(String, String)> = Vec::new();
    if let Some(lf) = &lockfile {
        let sections: Vec<&LockSection> = match &target {
            AuditTarget::All => audited.iter().filter_map(|&i| lf.section(&mounts[i])).collect(),
            AuditTarget::Roots(roots) => roots.iter().filter_map(|(i, _)| lf.section(&mounts[*i])).collect(),
            AuditTarget::Transitive(_) => lf.sections().collect(),
        };
        for section in sections {
            for (name, entries) in &section.packages {
                let in_scope = match &target {
                    AuditTarget::All => true,
                    AuditTarget::Roots(roots) => roots.iter().any(|(_, key)| name.eq_ignore_ascii_case(key)),
                    AuditTarget::Transitive(key) => name == key,
                };
                if in_scope {
                    pairs.extend(entries.iter().map(|e| (name.clone(), e.version.clone())));
                }
            }
        }
    } else {
        if matches!(target, AuditTarget::All) {
            message::info("No lockfile found; the license check covers direct dependencies only.");
        }
        for row in &rows {
            let version = row
                .current
                .clone()
                .or_else(|| row.wanted.clone())
                .unwrap_or_else(|| row.latest.clone());
            pairs.push((row.name.clone(), version.to_string()));
        }
    }
    pairs.sort();
    pairs.dedup();

    let mut license_infos: Vec<LicenseInfo> = Vec::new();
    if !pairs.is_empty() {
        // Metadata only, so this goes to the main API rather than the package
        // gateway; no download URLs are needed for a license check.
        let mut msg = Message::new(&format!("Checking licenses for {} package(s)...", pairs.len()));
        for (name, version) in &pairs {
            let pkg = digest_package_name(name);
            let endpoint = format!(
                "v1/package/{}/{}/{}/{}",
                encode(&pkg.scope),
                encode(&platform),
                encode(&pkg.name),
                encode(version)
            );
            let (data, status) = match api_request(&endpoint, Method::GET, None, None).await {
                Ok(res) => res,
                Err(e) => {
                    msg.emit(
                        MessageType::Warn,
                        &format!("Failed to fetch license info for {}@{}: {}", name, version, e),
                    );
                    continue;
                }
            };
            if !status.is_success() {
                msg.emit(
                    MessageType::Warn,
                    &format!("Failed to fetch license info for {}@{}: HTTP {}", name, version, status),
                );
                continue;
            }
            license_infos.push(extract_license_info(&data, &format!("{}@{}", name, version)));
        }
        msg.destroy();
    }

    let flagged: Vec<&LicenseInfo> = license_infos.iter().filter(|i| i.is_flagged()).collect();

    if matches!(target, AuditTarget::All) {
        // Stay quiet when every license fetch failed; the warnings above
        // already explain the gap.
        if flagged.is_empty() && !license_infos.is_empty() {
            message::success("No license considerations found in the dependency tree.");
        }
    } else {
        // Named package: also report clean/pending/unknown states explicitly.
        for info in license_infos.iter().filter(|i| !i.is_flagged()) {
            match info.rating.as_str() {
                "safe" => message::success(&format!(
                    "{}: license '{}' has no known considerations.",
                    info.label, info.license
                )),
                "pending" => message::info(&format!(
                    "{}: license review is still pending; check back shortly.",
                    info.label
                )),
                _ => message::info(&format!(
                    "{}: license '{}' has no safety rating.",
                    info.label, info.license
                )),
            }
        }
    }

    if !flagged.is_empty() {
        message::warn(&format!("{} package(s) have license considerations:", flagged.len()));
        println!();
        for info in &flagged {
            println!("{}", render_license_block(info));
            println!();
        }
        println!("  {}", "Automated license review, not legal advice.".dimmed());
        println!();
    }

    // ---- Overrides & excludes ----
    // Presence + resolved versions come straight from the lockfile; the
    // deeper "no longer needed"/"inert" analysis runs at resolve time
    // (install).
    if matches!(target, AuditTarget::All) {
        let installed_of = |name: &str| -> Vec<String> {
            lockfile
                .as_ref()
                .map(|lf| lf.versions_of(name).iter().map(|v| v.to_string()).collect())
                .unwrap_or_default()
        };
        let report_map = |label: &str, command: &str, entries: &HashMap<String, String>| {
            if entries.is_empty() {
                return;
            }
            message::info(&format!("{} {}(s) declared in forest.json:", entries.len(), label));
            let mut sorted: Vec<(&String, &String)> = entries.iter().collect();
            sorted.sort_by_key(|(k, _)| k.to_lowercase());
            for (name, range) in sorted {
                let installed = installed_of(name);
                if installed.is_empty() {
                    message::warn(&format!(
                        "  {} -> {} (matched nothing; remove with `forest {} {} --remove`)",
                        name, range, command, name
                    ));
                } else {
                    message::info(&format!("  {} -> {} (installed {})", name, range, installed.join(", ")));
                }
            }
        };
        report_map("override", "override", &crate::utils::normalize_forest_overrides(&info));
        report_map("exclusion", "exclude", &crate::utils::normalize_forest_excludes(&info));
    }

    if outdated.is_empty() {
        return Ok(());
    }

    if !update {
        let target_arg = match (&target, &target_package) {
            (AuditTarget::Roots(_), Some(raw)) => format!("{} ", raw),
            _ => String::new(),
        };
        let mount_arg = mount.as_deref().map(|m| format!("--mount {} ", m)).unwrap_or_default();
        message::info(&format!(
            "Run `forest audit {}{}--update` to bump forest.json to the latest versions, or `forest update` to stay within your declared ranges.",
            target_arg, mount_arg
        ));
        return Ok(());
    }

    // Bump each outdated dependency's declared range to the latest version,
    // preserving any alias (object form), in the mount that declares it.
    let mut changed: Vec<String> = Vec::new();
    for row in &outdated {
        let owner = &mounts[row.mount];
        let deps = crate::mounts::deps_map_mut(&mut info, owner)?;
        let new_range = Value::String(format!("^{}", row.latest));
        match deps.get_mut(&row.name) {
            Some(Value::Object(obj)) => {
                obj.insert("version".to_string(), new_range);
            }
            Some(slot) => {
                *slot = new_range;
            }
            // Rows come from the manifest's own keys, so a miss means
            // the file changed under us.
            None => {
                message::warn(&format!("{} not found in forest.json; skipped.", row.name));
                continue;
            }
        }
        if !changed.contains(&owner.path) {
            changed.push(owner.path.clone());
        }
    }
    let manifest_before = fs::read_to_string("forest.json")?;
    fs::write("forest.json", serde_json::to_string_pretty(&info)?)?;

    // Re-resolve and reinstall with the new ranges
    let mut msg = Message::new("Updating packages...");
    let opts = SyncOptions::new(scope.map(|m| m.path.clone()), Refresh::Mounts(changed));
    sync_or_restore(&info, &manifest_before, &mut msg, &opts).await?;

    msg.finish(
        MessageType::Success,
        &format!("Updated {} package(s) to the latest versions!", outdated.len()),
    );

    Ok(())
}

/// A package's published versions minus the excluded ones, sorted. None
/// (after a warning) when the list can't be fetched or nothing is left.
async fn fetch_published(
    name: &str,
    platform: &str,
    excludes: &HashMap<String, VersionReq>,
    msg: &mut Message,
) -> Option<Vec<Version>> {
    let pkg = digest_package_name(name);
    let endpoint = format!(
        "v1/package/{}/{}/{}",
        encode(&pkg.scope),
        encode(platform),
        encode(&pkg.name)
    );

    let (data, status) = match api_request(&endpoint, Method::GET, None, None).await {
        Ok(res) => res,
        Err(e) => {
            msg.emit(
                MessageType::Warn,
                &format!("Failed to fetch package info for {}: {}", name, e),
            );
            return None;
        }
    };
    if !status.is_success() {
        msg.emit(
            MessageType::Warn,
            &format!("Failed to fetch package info for {}: HTTP {}{}", name, status, crate::lockfile_solver::not_found_hint(status)),
        );
        return None;
    }

    let exclude_req = excludes.get(&name.to_lowercase());
    let mut versions: Vec<Version> = data
        .get("versions")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|v| v.get("version").and_then(Value::as_str))
                .filter_map(|v| Version::parse(v).ok())
                .collect()
        })
        .unwrap_or_default();
    let published = versions.len();
    versions.retain(|v| exclude_req.map_or(true, |req| !req.matches(v)));
    versions.sort();

    if versions.is_empty() {
        if published > 0 {
            msg.emit(
                MessageType::Warn,
                &format!("Every published version of {} is excluded by forest.json; remove or narrow the exclusion with `forest exclude {} --remove`.", name, name),
            );
        } else {
            msg.emit(MessageType::Warn, &format!("No versions found for {}", name));
        }
        return None;
    }
    Some(versions)
}

