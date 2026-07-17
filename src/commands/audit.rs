use std::{collections::HashMap, fs, path::Path};
use anyhow::Result;
use colored::Colorize;
use reqwest::Method;
use semver::{Version, VersionReq};
use serde_json::Value;
use urlencoding::encode;

use crate::http::api_request;
use crate::lockfile_gen::lockfile_gen;
use crate::message::{self, Message, MessageType};
use crate::utils::{digest_package_name, get_ci, normalize_forest_deps};

struct AuditRow {
    name: String,
    current: Option<Version>,
    wanted: Option<Version>,
    latest: Version,
}

/// Read the locked version of each root dependency (location "~") from forest-lock.json.
fn locked_versions() -> HashMap<String, Version> {
    let mut locked = HashMap::new();

    let lock: Value = match fs::read_to_string("forest-lock.json")
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
    {
        Some(v) => v,
        None => return locked,
    };

    if let Some(packages) = lock.get("packages").and_then(Value::as_object) {
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

/// Check root dependencies for available updates. With `update`, bump
/// forest.json to the latest versions and reinstall.
pub async fn audit_command(update: bool) -> Result<()> {
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

    let locked = locked_versions();

    let mut rows: Vec<AuditRow> = Vec::new();
    for (name, spec) in &roots {
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

    if outdated.is_empty() {
        message::success("All dependencies are up to date!");
        return Ok(());
    }

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

    if !update {
        message::info("Run `forest audit --update` to update forest.json to the latest versions.");
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
    let lockfile_content = lockfile_gen(&info_clone, &mut msg).await?;
    let lockfile_content = serde_json::to_string_pretty(&lockfile_content)?;
    fs::write("forest-lock.json", lockfile_content)?;

    msg.finish(
        MessageType::Success,
        &format!("Updated {} package(s) to the latest versions!", outdated.len()),
    );

    Ok(())
}
