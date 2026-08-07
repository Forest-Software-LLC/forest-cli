use std::{collections::HashMap, fs, path::Path};

use anyhow::Result;
use dialoguer::{theme::ColorfulTheme, Confirm, Input};
use reqwest::Method;
use semver::{Version, VersionReq};
use serde_json::{Map, Value};
use urlencoding::encode;

use crate::http::api_request;
use crate::lockfile_gen::lockfile_gen;
use crate::message::{self, Message, MessageType};
use crate::utils::{
    digest_package_name, normalize_forest_deps, normalize_forest_excludes,
    normalize_forest_overrides, resolve_dep_ref, DepRef,
};

/// Manage dependency overrides: force every transitive occurrence of a
/// package onto one semver range, recorded under `overrides` in forest.json.
/// With no package argument, lists the declared overrides. Direct
/// dependencies are rejected — their range already lives in `dependencies`.
/// For banning specific bad versions without forcing a range, see
/// `forest exclude`.
pub async fn override_command(
    package: Option<String>,
    range: Option<String>,
    yes: bool,
    remove: bool,
) -> Result<()> {
    // Same manifest discovery as install/tree (UEFN keeps forest.json in
    // Content/).
    if !Path::new("forest.json").exists() {
        if let Some(manifest_dir) = crate::platform::discover_manifest_dir(&std::env::current_dir()?) {
            std::env::set_current_dir(&manifest_dir)?;
        }
    }
    if !Path::new("forest.json").exists() {
        message::fail("No forest.json found. Run `forest init` to create a new package.");
        return Ok(());
    }

    let mut manifest: Value = serde_json::from_str(&fs::read_to_string("forest.json")?)?;
    let overrides = normalize_forest_overrides(&manifest);

    let Some(reference) = package else {
        if remove || range.is_some() {
            message::fail("Specify which package: forest override <scope/name> [--range <range>] [--remove]");
            return Ok(());
        }
        list_overrides(&overrides);
        return Ok(());
    };

    if remove {
        if range.is_some() {
            message::fail("--range and --remove cannot be combined.");
            return Ok(());
        }
        return remove_override(&mut manifest, &overrides, &reference).await;
    }

    let platform = manifest
        .get("platform")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing platform in forest.json"))?
        .to_string();
    let roots = normalize_forest_deps(&manifest);

    // Direct dependencies are not overridden — the manifest range IS the
    // user's own constraint. But a reference that names an existing override
    // is an edit of that override, even if a root dep shares the bare name.
    let existing_key = match_override_key(&overrides, &reference);
    if existing_key.is_none() {
        match resolve_dep_ref(&roots, &reference) {
            DepRef::Match(key) => {
                message::fail(&format!(
                    "{} is a direct dependency; change its range with `forest install {} -v <version>` or edit forest.json. To ban specific versions of it instead, use `forest exclude {}`.",
                    key, key, key
                ));
                return Ok(());
            }
            DepRef::Ambiguous(_) => {} // Fall through: full scope/name is required below anyway.
            DepRef::NotFound => {}
        }
    }

    // Resolve the reference to a full scope/name: an existing override key,
    // the typed scope/name, or a unique bare-name match in the lockfile.
    let full_name = match &existing_key {
        Some(key) => key.clone(),
        None if reference.contains('/') => reference.clone(),
        None => {
            let lock_keys = lockfile_package_keys();
            let name = reference.as_str();
            let candidates: Vec<&String> = lock_keys
                .iter()
                .filter(|k| k.rsplit('/').next().map_or(false, |n| n.eq_ignore_ascii_case(name)))
                .collect();
            match candidates.as_slice() {
                [key] => (*key).clone(),
                [] => {
                    message::fail(&format!(
                        "\"{}\" is not in the installed tree. Use the full <scope>/<name>.",
                        reference
                    ));
                    return Ok(());
                }
                many => {
                    let mut keys: Vec<&str> = many.iter().map(|k| k.as_str()).collect();
                    keys.sort();
                    message::warn(&format!(
                        "\"{}\" matches more than one installed package: {}. Use the full <scope>/<name>.",
                        reference,
                        keys.join(", ")
                    ));
                    return Ok(());
                }
            }
        }
    };

    // Fetch the version list once; the wizard loop validates against it
    // locally. The response also carries the canonical stored casing.
    let (canonical, versions) = match fetch_versions(&full_name, &platform).await? {
        Some(res) => res,
        None => return Ok(()),
    };

    // Validate and preview against the pool the solver can actually pick
    // from: versions banned by a declared exclusion are dropped up front,
    // so the wizard never promises a version resolution would refuse.
    let excludes = normalize_forest_excludes(&manifest);
    let (versions, banned) = drop_excluded_versions(versions, &excludes, &canonical);
    if let Some((exclude_range, removed)) = banned {
        if versions.is_empty() {
            message::fail(&format!(
                "Every published version of {} is banned by the excludes entry \"{}\"; remove or narrow it with `forest exclude {}` first.",
                canonical, exclude_range, canonical
            ));
            return Ok(());
        }
        message::info(&format!(
            "Excludes entry \"{}\" bans {} published version{} of {}; the override cannot pick them.",
            exclude_range,
            removed,
            if removed == 1 { "" } else { "s" },
            canonical
        ));
    }

    let current_override = match_override_key(&overrides, &canonical).map(|k| overrides[&k].clone());
    let current_versions = installed_versions(&canonical);
    if current_versions.is_empty() {
        message::info(&format!(
            "{} is not in the current dependency tree; the override will apply when it appears.",
            canonical
        ));
    }

    // Get a satisfiable range: --range is single-shot (scripts must fail
    // loudly), the prompt re-asks until the range matches a version.
    let new_range = match range {
        Some(r) => match validate_range(&r, &versions, &canonical) {
            Ok(_) => r,
            Err(reason) => {
                message::fail(&reason);
                return Ok(());
            }
        },
        None => {
            // The lockfile can hold several versions of one package at once
            // (split buckets) — show them all, not one end of the list.
            let current_note = current_override.clone().or_else(|| {
                (!current_versions.is_empty())
                    .then(|| format!("installed {}", join_versions(&current_versions)))
            });
            let prompt = match current_note {
                Some(note) => format!("New SemVer range for {} (current: {})", canonical, note),
                None => format!("New SemVer range for {}", canonical),
            };
            loop {
                let input: String = match Input::with_theme(&ColorfulTheme::default())
                    .with_prompt(&prompt)
                    .interact_text()
                {
                    Ok(v) => v,
                    Err(_) => {
                        message::fail("Interactive prompt unavailable; pass the range with --range.");
                        return Ok(());
                    }
                };
                match validate_range(input.trim(), &versions, &canonical) {
                    Ok(_) => break input.trim().to_string(),
                    Err(reason) => message::warn(&reason),
                }
            }
        }
    };

    let resolves_to = validate_range(&new_range, &versions, &canonical)
        .expect("range was validated above");

    let change = if current_versions.iter().any(|v| *v != resolves_to) {
        format!(
            "Override version will be v{} (currently {}).",
            resolves_to,
            join_versions(&current_versions)
        )
    } else {
        format!("Override version will be v{}.", resolves_to)
    };
    message::info(&change);

    if !yes && !confirm("Accept?") {
        message::info("Override not applied.");
        return Ok(());
    }

    // Update the manifest, reusing an existing key's casing when the same
    // package is already overridden.
    let manifest_before = fs::read_to_string("forest.json")?;
    let slot_key = match_override_key(&overrides, &canonical).unwrap_or_else(|| canonical.clone());
    write_map_entry(&mut manifest, "overrides", slot_key, &new_range)?;

    if reinstall_or_rollback(&manifest, &manifest_before).await? {
        message::success(&format!("Override set: {} -> {}", canonical, new_range));
    }
    Ok(())
}

/// Fetch a package's published version list (sorted ascending) plus its
/// canonical scope/name casing. `Ok(None)` = failure already reported.
pub(crate) async fn fetch_versions(full_name: &str, platform: &str) -> Result<Option<(String, Vec<Version>)>> {
    let msg = Message::new(&format!("Fetching versions for {}...", full_name));
    let pkg = digest_package_name(full_name);
    let endpoint = format!(
        "v1/package/{}/{}/{}",
        encode(&pkg.scope),
        encode(platform),
        encode(&pkg.name)
    );
    let (data, status) = match api_request(&endpoint, Method::GET, None, None).await {
        Ok(res) => res,
        Err(e) => {
            msg.finish(MessageType::Fail, &format!("Failed to fetch package info for {}: {}", full_name, e));
            return Ok(None);
        }
    };
    if !status.is_success() {
        msg.finish(
            MessageType::Fail,
            &format!("Failed to fetch package info for {}: HTTP {}", full_name, status),
        );
        return Ok(None);
    }
    msg.destroy();

    let canonical = match (
        data.get("scope").and_then(Value::as_str),
        data.get("name").and_then(Value::as_str),
    ) {
        (Some(scope), Some(name)) => format!("{}/{}", scope, name),
        _ => full_name.to_string(),
    };

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
        message::fail(&format!("No published versions found for {}.", canonical));
        return Ok(None);
    }
    Ok(Some((canonical, versions)))
}

pub(crate) fn confirm(prompt: &str) -> bool {
    Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .default(true)
        .interact()
        .unwrap_or(false)
}

/// Insert `key -> range` into the named manifest map field, creating it if
/// needed, and persist forest.json.
pub(crate) fn write_map_entry(manifest: &mut Value, field: &str, key: String, range: &str) -> Result<()> {
    if !manifest.get(field).map_or(false, Value::is_object) {
        manifest[field] = Value::Object(Map::new());
    }
    manifest[field]
        .as_object_mut()
        .expect("field object ensured above")
        .insert(key, Value::String(range.to_string()));
    fs::write("forest.json", serde_json::to_string_pretty(&manifest)?)?;
    Ok(())
}

/// Remove `key` from the named manifest map field (dropping the field when
/// it empties) and persist forest.json.
pub(crate) fn remove_map_entry(manifest: &mut Value, field: &str, key: &str) -> Result<()> {
    let map = manifest
        .get_mut(field)
        .and_then(Value::as_object_mut)
        .expect("key came from the manifest");
    map.remove(key);
    if map.is_empty() {
        manifest.as_object_mut().unwrap().remove(field);
    }
    fs::write("forest.json", serde_json::to_string_pretty(manifest)?)?;
    Ok(())
}

/// `Ok(best matching version)` or the user-facing rejection message —
/// shared by the prompt loop and the --range path.
fn validate_range(range: &str, versions: &[Version], pkg: &str) -> std::result::Result<Version, String> {
    let range = range.trim();
    if range.is_empty() {
        return Err("Enter a SemVer range (e.g. ^2.0.0).".to_string());
    }
    let Ok(req) = VersionReq::parse(range) else {
        return Err(format!("\"{}\" is not a valid SemVer range.", range));
    };
    match versions.iter().filter(|v| req.matches(v)).max() {
        Some(best) => Ok(best.clone()),
        None => Err(format!(
            "{} satisfies no published versions of {} and cannot be installed.",
            range, pkg
        )),
    }
}

/// Remove versions banned by a declared exclusion for `canonical`, so the
/// override wizard only offers what resolution could actually install.
/// Returns the remaining pool plus the applied exclude range and how many
/// versions it removed. An unparseable exclude range filters nothing — the
/// solver rejects it with a proper error on reinstall.
fn drop_excluded_versions(
    versions: Vec<Version>,
    excludes: &HashMap<String, String>,
    canonical: &str,
) -> (Vec<Version>, Option<(String, usize)>) {
    let Some(key) = match_override_key(excludes, canonical) else {
        return (versions, None);
    };
    let range = excludes[&key].clone();
    let Ok(req) = VersionReq::parse(&range) else {
        return (versions, None);
    };
    let before = versions.len();
    let kept: Vec<Version> = versions.into_iter().filter(|v| !req.matches(v)).collect();
    let removed = before - kept.len();
    if removed == 0 {
        return (kept, None);
    }
    (kept, Some((range, removed)))
}

pub(crate) fn join_versions(list: &[Version]) -> String {
    list.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ")
}

fn list_overrides(overrides: &HashMap<String, String>) {
    if overrides.is_empty() {
        message::info("No overrides declared. Add one with `forest override <scope/name>`.");
        return;
    }
    let mut sorted: Vec<(&String, &String)> = overrides.iter().collect();
    sorted.sort_by_key(|(k, _)| k.to_lowercase());
    for (name, range) in sorted {
        let installed = installed_versions(name);
        let status = if installed.is_empty() {
            "not in tree".to_string()
        } else {
            format!("installed {}", join_versions(&installed))
        };
        message::info(&format!("{} -> {} ({})", name, range, status));
    }
}

async fn remove_override(
    manifest: &mut Value,
    overrides: &HashMap<String, String>,
    reference: &str,
) -> Result<()> {
    let Some(key) = match_override_key(overrides, reference) else {
        message::fail(&format!("No override declared for {}.", reference));
        return Ok(());
    };
    let manifest_before = fs::read_to_string("forest.json")?;
    remove_map_entry(manifest, "overrides", &key)?;
    if reinstall_or_rollback(manifest, &manifest_before).await? {
        message::success(&format!("Override removed: {}", key));
    }
    Ok(())
}

/// Match a reference (scope/name or bare name, case-insensitive) against
/// declared override keys.
pub(crate) fn match_override_key(overrides: &HashMap<String, String>, reference: &str) -> Option<String> {
    if reference.contains('/') {
        return overrides
            .keys()
            .find(|k| k.eq_ignore_ascii_case(reference))
            .cloned();
    }
    let matches: Vec<&String> = overrides
        .keys()
        .filter(|k| k.rsplit('/').next().map_or(false, |n| n.eq_ignore_ascii_case(reference)))
        .collect();
    match matches.as_slice() {
        [key] => Some((*key).clone()),
        _ => None,
    }
}

/// Every version of the package the lockfile currently holds, sorted.
pub(crate) fn installed_versions(name: &str) -> Vec<Version> {
    let Ok(raw) = fs::read_to_string("forest-lock.json") else {
        return Vec::new();
    };
    let Ok(lock) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let mut versions: Vec<Version> = lock
        .get("packages")
        .and_then(Value::as_object)
        .and_then(|pkgs| {
            pkgs.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, entries)| entries)
        })
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| e.get("version").and_then(Value::as_str))
                .filter_map(|v| Version::parse(v).ok())
                .collect()
        })
        .unwrap_or_default();
    versions.sort();
    versions.dedup();
    versions
}

pub(crate) fn lockfile_package_keys() -> Vec<String> {
    let Ok(raw) = fs::read_to_string("forest-lock.json") else {
        return Vec::new();
    };
    let Ok(lock) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    lock.get("packages")
        .and_then(Value::as_object)
        .map(|pkgs| pkgs.keys().cloned().collect())
        .unwrap_or_default()
}

/// Re-resolve and reinstall under the updated manifest, like `audit --update`.
/// On failure the pre-change manifest is restored, so a constraint that
/// can't resolve (or a network hiccup) never leaves forest.json poisoned;
/// the caller should stop after a `false` return.
pub(crate) async fn reinstall_or_rollback(manifest: &Value, manifest_before: &str) -> Result<bool> {
    let mut msg = Message::new("Updating packages...");
    match lockfile_gen(manifest, &mut msg, false).await {
        Ok(lockfile) => {
            fs::write("forest-lock.json", serde_json::to_string_pretty(&lockfile)?)?;
            msg.destroy();
            Ok(true)
        }
        Err(e) => {
            msg.destroy();
            fs::write("forest.json", manifest_before)?;
            message::fail(&format!("Change rolled back, resolution failed: {:#}", e));
            Ok(false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(list: &[&str]) -> Vec<Version> {
        list.iter().map(|v| Version::parse(v).unwrap()).collect()
    }

    fn overrides(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn valid_range_resolves_to_the_highest_match() {
        let avail = versions(&["1.0.0", "2.0.5", "2.1.2"]);
        assert_eq!(
            validate_range("^2.0.0", &avail, "scope/pkg").unwrap(),
            Version::parse("2.1.2").unwrap()
        );
    }

    #[test]
    fn unsatisfiable_range_reports_the_package() {
        let avail = versions(&["1.0.0"]);
        assert_eq!(
            validate_range("^3.0.0", &avail, "scope/pkg").unwrap_err(),
            "^3.0.0 satisfies no published versions of scope/pkg and cannot be installed."
        );
    }

    #[test]
    fn malformed_and_empty_ranges_are_rejected() {
        let avail = versions(&["1.0.0"]);
        assert!(validate_range("not-a-range", &avail, "scope/pkg").is_err());
        assert!(validate_range("  ", &avail, "scope/pkg").is_err());
    }

    #[test]
    fn override_keys_match_by_full_name_or_unique_bare_name() {
        let o = overrides(&[("Scope/Pkg", "^2.0.0")]);
        assert_eq!(match_override_key(&o, "scope/pkg"), Some("Scope/Pkg".to_string()));
        assert_eq!(match_override_key(&o, "pkg"), Some("Scope/Pkg".to_string()));
        assert_eq!(match_override_key(&o, "other"), None);
    }

    #[test]
    fn ambiguous_bare_name_matches_nothing() {
        let o = overrides(&[("a/pkg", "^1.0.0"), ("b/pkg", "^2.0.0")]);
        assert_eq!(match_override_key(&o, "pkg"), None);
        assert_eq!(match_override_key(&o, "a/pkg"), Some("a/pkg".to_string()));
    }

    #[test]
    fn excluded_versions_are_dropped_from_the_override_pool() {
        let avail = versions(&["1.5.2", "1.6.0", "2.0.0"]);
        let ex = overrides(&[("Scope/Pkg", "=1.6.0")]);
        let (kept, banned) = drop_excluded_versions(avail, &ex, "scope/pkg");
        assert_eq!(kept, versions(&["1.5.2", "2.0.0"]));
        assert_eq!(banned, Some(("=1.6.0".to_string(), 1)));
    }

    #[test]
    fn exclusion_banning_the_whole_pool_leaves_it_empty() {
        let avail = versions(&["1.6.0", "1.6.1"]);
        let ex = overrides(&[("s/p", "^1.6.0")]);
        let (kept, banned) = drop_excluded_versions(avail, &ex, "s/p");
        assert!(kept.is_empty());
        assert_eq!(banned, Some(("^1.6.0".to_string(), 2)));
    }

    #[test]
    fn irrelevant_or_unparseable_exclusions_filter_nothing() {
        let avail = versions(&["1.5.2"]);
        // No exclusion for this package at all.
        assert_eq!(
            drop_excluded_versions(avail.clone(), &overrides(&[]), "s/p"),
            (avail.clone(), None)
        );
        // Declared but banning no published version.
        let miss = overrides(&[("s/p", "=9.9.9")]);
        assert_eq!(
            drop_excluded_versions(avail.clone(), &miss, "s/p"),
            (avail.clone(), None)
        );
        // Unparseable range: the solver reports it, the wizard stays out.
        let bad = overrides(&[("s/p", "junk")]);
        assert_eq!(drop_excluded_versions(avail.clone(), &bad, "s/p"), (avail, None));
    }
}
