use std::{collections::HashMap, fs, path::Path};
use anyhow::Result;
use colored::Colorize;
use reqwest::Method;
use semver::{Version, VersionReq};
use serde_json::{Map, Value};
use urlencoding::encode;

use crate::http::api_request;
use crate::licensce_helper::{extract_license_info, LicenseInfo};
use crate::lockfile_gen::lockfile_gen;
use crate::lockfile_solver::DepSpec;
use crate::message::{self, Message, MessageType};
use crate::utils::{digest_package_name, get_ci, normalize_forest_deps};

struct AuditRow {
    name: String,
    current: Option<Version>,
    wanted: Option<Version>,
    latest: Version,
}

/// How the optional package argument resolved against the project.
enum AuditTarget {
    /// No package argument — audit everything.
    All,
    /// A direct dependency (manifest key).
    Root(String),
    /// Installed, but only as a transitive dependency (canonical lockfile key).
    Transitive(String),
}

/// Read the locked version of each root dependency (location "~") from the
/// lockfile's packages map.
fn locked_versions(packages: Option<&Map<String, Value>>) -> HashMap<String, Version> {
    let mut locked = HashMap::new();

    if let Some(packages) = packages {
        for (name, entries) in packages {
            let root_version = entries
                .as_array()
                .and_then(|list| {
                    list.iter().find(|e| {
                        e.get("location").and_then(Value::as_str) == Some("~")
                    })
                })
                .and_then(|e| e.get("version").and_then(Value::as_str))
                .and_then(|v| Version::parse(v).ok());

            if let Some(version) = root_version {
                locked.insert(name.clone(), version);
            }
        }
    }

    locked
}

/// Render one flagged package. Color only the accents — caveat text stays in
/// the terminal's default color so long lists remain readable.
fn render_license_block(info: &LicenseInfo) -> String {
    let severity = match info.rating.as_str() {
        "unsafe" => "legal risk for closed-source games".red().bold(),
        _ => "usable with conditions".yellow(),
    };
    let mut out = format!(
        "  {} {} {} {} {}",
        info.label.cyan(),
        "—".dimmed(),
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
        assert_eq!(lines[0], "  scope/pkg@1.2.3 — GPL-3.0 · legal risk for closed-source games");
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

        assert_eq!(block, "  scope/pkg@2.0.0 — Apache-2.0 · usable with conditions");
    }
}

/// Check dependencies for available updates and license considerations. With
/// `target_package`, limit the audit to that dependency. With `update`, bump
/// forest.json to the latest versions and reinstall.
pub async fn audit_command(target_package: Option<String>, update: bool) -> Result<()> {
    let mut msg = Message::new("Checking for updates...");

    // Ensure forest.json exists
    if !Path::new("forest.json").exists() {
        msg.finish(
            MessageType::Fail,
            "No forest.json found. Run `forest init` to create a new package.",
        );
        return Ok(());
    }

    // Read and parse forest.json
    let mut info: Value = serde_json::from_str(&fs::read_to_string("forest.json")?)?;

    let platform = info
        .get("platform")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing platform in forest.json"))?
        .to_string();

    let roots = normalize_forest_deps(&info);
    if roots.is_empty() {
        msg.finish(MessageType::Info, "No dependencies to audit.");
        return Ok(());
    }

    // Parse the lockfile once: root versions feed the updates table, and the
    // full resolved tree (direct + transitive) feeds the license report.
    let lock: Option<Value> = fs::read_to_string("forest-lock.json")
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok());
    let lock_packages = lock
        .as_ref()
        .and_then(|l| l.get("packages"))
        .and_then(Value::as_object);

    let target = match &target_package {
        None => AuditTarget::All,
        Some(raw) => {
            let name = raw.trim_start_matches('@');
            if !name.contains('/') {
                msg.finish(
                    MessageType::Fail,
                    &format!("Invalid package name '{}'. Use the full name: <scope>/<name>.", raw),
                );
                return Ok(());
            }
            if let Some(key) = roots.keys().find(|k| k.eq_ignore_ascii_case(name)) {
                AuditTarget::Root(key.clone())
            } else if let Some(key) =
                lock_packages.and_then(|pkgs| pkgs.keys().find(|k| k.eq_ignore_ascii_case(name)))
            {
                AuditTarget::Transitive(key.clone())
            } else {
                msg.finish(
                    MessageType::Fail,
                    &format!("Package {} is not a dependency of this project.", name),
                );
                return Ok(());
            }
        }
    };

    let check_roots: Vec<(&String, &DepSpec)> = match &target {
        AuditTarget::All => roots.iter().collect(),
        AuditTarget::Root(key) => roots.iter().filter(|(k, _)| *k == key).collect(),
        AuditTarget::Transitive(_) => Vec::new(),
    };

    let locked = locked_versions(lock_packages);

    // ---- Update check ----
    let mut rows: Vec<AuditRow> = Vec::new();
    for &(name, spec) in &check_roots {
        let pkg = digest_package_name(name);
        let endpoint = format!(
            "v1/package/{}/{}/{}",
            encode(&pkg.scope),
            encode(&platform),
            encode(&pkg.name)
        );

        let (data, status) = match api_request(&endpoint, Method::GET, None, None).await {
            Ok(res) => res,
            Err(e) => {
                msg.emit(
                    MessageType::Warn,
                    &format!("Failed to fetch package info for {}: {}", name, e),
                );
                continue;
            }
        };
        if !status.is_success() {
            msg.emit(
                MessageType::Warn,
                &format!("Failed to fetch package info for {}: HTTP {}", name, status),
            );
            continue;
        }

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
        versions.sort();

        if versions.is_empty() {
            msg.emit(MessageType::Warn, &format!("No versions found for {}", name));
            continue;
        }

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
            name: name.clone(),
            // Lockfile keys are canonical; the manifest key may differ by case
            current: get_ci(&locked, name).cloned(),
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
    outdated.sort_by(|a, b| a.name.cmp(&b.name));

    msg.destroy();

    if let AuditTarget::Transitive(key) = &target {
        message::info(&format!(
            "{} is not a direct dependency — checking its installed license only.",
            key
        ));
    } else if outdated.is_empty() {
        // Skip the all-clear when every fetch failed — the warnings above tell
        // the real story.
        if !rows.is_empty() {
            match &target {
                AuditTarget::Root(key) => message::success(&format!("{} is up to date.", key)),
                _ => message::success("All dependencies are up to date!"),
            }
        }
    } else {
        // Render the table (pad before coloring — ANSI codes break width padding)
        let fmt_opt = |v: &Option<Version>| v.as_ref().map_or("-".to_string(), |v| v.to_string());
        let name_w = outdated.iter().map(|r| r.name.len()).max().unwrap().max("Package".len());
        let cur_w = outdated.iter().map(|r| fmt_opt(&r.current).len()).max().unwrap().max("Current".len());
        let want_w = outdated.iter().map(|r| fmt_opt(&r.wanted).len()).max().unwrap().max("Wanted".len());
        let lat_w = outdated.iter().map(|r| r.latest.to_string().len()).max().unwrap().max("Latest".len());

        message::warn(&format!("{} package(s) have updates available:", outdated.len()));
        println!();
        println!(
            "  {}",
            format!(
                "{:<name_w$}  {:>cur_w$}  {:>want_w$}  {:>lat_w$}",
                "Package", "Current", "Wanted", "Latest"
            )
            .bold()
        );
        for row in &outdated {
            println!(
                "  {}  {:>cur_w$}  {}  {}",
                format!("{:<name_w$}", row.name).cyan(),
                fmt_opt(&row.current),
                format!("{:>want_w$}", fmt_opt(&row.wanted)).yellow(),
                format!("{:>lat_w$}", row.latest).green(),
            );
        }
        println!();
    }

    // ---- License check ----
    // Prefer the lockfile's resolved tree so transitive dependencies are
    // covered (matching what install warns about); fall back to the direct
    // dependencies' resolved-range versions when no lockfile exists.
    let mut pairs: Vec<(String, String)> = Vec::new();
    if let Some(packages) = lock_packages {
        for (name, entries) in packages {
            let in_scope = match &target {
                AuditTarget::All => true,
                AuditTarget::Root(key) => name.eq_ignore_ascii_case(key),
                AuditTarget::Transitive(key) => name == key,
            };
            if !in_scope {
                continue;
            }
            if let Some(list) = entries.as_array() {
                for entry in list {
                    if let Some(version) = entry.get("version").and_then(Value::as_str) {
                        pairs.push((name.clone(), version.to_string()));
                    }
                }
            }
        }
    } else {
        if matches!(target, AuditTarget::All) {
            message::info("No lockfile found — the license check covers direct dependencies only.");
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
        // gateway — no download URLs are needed for a license check.
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
        // Stay quiet when every license fetch failed — the warnings above
        // already explain the gap.
        if flagged.is_empty() && !license_infos.is_empty() {
            message::success("No license considerations found in the dependency tree.");
        }
    } else {
        // Named package: also report clean/pending/unknown states explicitly.
        for info in license_infos.iter().filter(|i| !i.is_flagged()) {
            match info.rating.as_str() {
                "safe" => message::success(&format!(
                    "{} — license '{}' has no known considerations.",
                    info.label, info.license
                )),
                "pending" => message::info(&format!(
                    "{} — license review is still pending; check back shortly.",
                    info.label
                )),
                _ => message::info(&format!(
                    "{} — license '{}' has no safety rating.",
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
        println!("  {}", "Automated license review — not legal advice.".dimmed());
        println!();
    }

    if outdated.is_empty() {
        return Ok(());
    }

    if !update {
        let target_arg = match &target {
            AuditTarget::Root(key) => format!("{} ", key),
            _ => String::new(),
        };
        message::info(&format!(
            "Run `forest audit {}--update` to update forest.json to the latest versions.",
            target_arg
        ));
        return Ok(());
    }

    // Bump each outdated dependency's declared range to the latest version,
    // preserving any alias (object form).
    {
        let deps = info
            .get_mut("dependencies")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| anyhow::anyhow!("Missing dependencies in forest.json"))?;

        for row in &outdated {
            let new_range = Value::String(format!("^{}", row.latest));
            match deps.get_mut(&row.name) {
                Some(Value::Object(obj)) => {
                    obj.insert("version".to_string(), new_range);
                }
                Some(slot) => {
                    *slot = new_range;
                }
                None => {}
            }
        }
    }
    fs::write("forest.json", serde_json::to_string_pretty(&info)?)?;

    // Re-resolve and reinstall with the new ranges
    let mut msg = Message::new("Updating packages...");
    let info_clone = info.clone();
    let lockfile_content = lockfile_gen(&info_clone, &mut msg, false).await?;
    let lockfile_content = serde_json::to_string_pretty(&lockfile_content)?;
    fs::write("forest-lock.json", lockfile_content)?;

    msg.finish(
        MessageType::Success,
        &format!("Updated {} package(s) to the latest versions!", outdated.len()),
    );

    Ok(())
}
