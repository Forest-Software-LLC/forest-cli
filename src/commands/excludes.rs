use std::{collections::HashMap, fs, path::Path};

use anyhow::Result;
use dialoguer::{theme::ColorfulTheme, Input};
use semver::{Version, VersionReq};
use serde_json::Value;

use crate::commands::overrides::{
    confirm, fetch_versions, installed_versions, join_versions, lockfile_package_keys,
    match_override_key, reinstall_or_rollback, remove_map_entry, write_map_entry,
};
use crate::message;
use crate::utils::{normalize_forest_deps, normalize_forest_excludes, resolve_dep_ref, DepRef};

/// What an exclusion range would do to a package's published versions.
struct ExcludeCheck {
    /// Currently published versions the range bans.
    banned: Vec<Version>,
    /// The range has no upper bound, so it also bans every future version.
    open_ended: bool,
}

/// Manage version exclusions: ban a range of a package's versions from ever
/// being installed, recorded under `excludes` in forest.json. Applies
/// uniformly to direct and transitive dependencies — the solver drops the
/// banned versions from the candidate set, so every declared range is still
/// honored (or resolution fails loudly, naming the exclusion). With no
/// package argument, lists the declared exclusions.
pub async fn exclude_command(
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
    let excludes = normalize_forest_excludes(&manifest);

    let Some(reference) = package else {
        if remove || range.is_some() {
            message::fail("Specify which package: forest exclude <scope/name> [--range <range>] [--remove]");
            return Ok(());
        }
        list_excludes(&excludes);
        return Ok(());
    };

    if remove {
        if range.is_some() {
            message::fail("--range and --remove cannot be combined.");
            return Ok(());
        }
        let Some(key) = match_override_key(&excludes, &reference) else {
            message::fail(&format!("No exclusion declared for {}.", reference));
            return Ok(());
        };
        let manifest_before = fs::read_to_string("forest.json")?;
        remove_map_entry(&mut manifest, "excludes", &key)?;
        if reinstall_or_rollback(&manifest, &manifest_before).await? {
            message::success(&format!("Exclusion removed: {}", key));
        }
        return Ok(());
    }

    let platform = manifest
        .get("platform")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing platform in forest.json"))?
        .to_string();

    // Unlike overrides, direct deps are fair game — "never install 1.6.0"
    // is a fact about the package, not about who depends on it. Resolution
    // order: existing exclusion key, declared root key, typed scope/name,
    // unique bare name in the lockfile.
    let roots = normalize_forest_deps(&manifest);
    let full_name = match match_override_key(&excludes, &reference) {
        Some(key) => key,
        None => match resolve_dep_ref(&roots, &reference) {
            DepRef::Match(key) => key,
            DepRef::Ambiguous(candidates) => {
                message::warn(&format!(
                    "\"{}\" matches more than one dependency: {}. Use the full <scope>/<name>.",
                    reference,
                    candidates.join(", ")
                ));
                return Ok(());
            }
            DepRef::NotFound if reference.contains('/') => reference.clone(),
            DepRef::NotFound => {
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
        },
    };

    let (canonical, versions) = match fetch_versions(&full_name, &platform).await? {
        Some(res) => res,
        None => return Ok(()),
    };

    let current_exclude = match_override_key(&excludes, &canonical).map(|k| excludes[&k].clone());

    // Get a valid banned set: --range is single-shot, the prompt re-asks.
    let (new_range, check) = match range {
        Some(r) => match validate_exclude_range(&r, &versions, &canonical) {
            Ok(check) => (r, check),
            Err(reason) => {
                message::fail(&reason);
                return Ok(());
            }
        },
        None => {
            let prompt = match &current_exclude {
                Some(cur) => format!("Versions of {} to exclude (current: {})", canonical, cur),
                None => format!("Versions of {} to exclude (e.g. =1.6.0)", canonical),
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
                match validate_exclude_range(input.trim(), &versions, &canonical) {
                    Ok(check) => break (input.trim().to_string(), check),
                    Err(reason) => message::warn(&reason),
                }
            }
        }
    };

    // Show exactly what the ban covers before committing.
    let banned_list: Vec<String> = check.banned.iter().map(|v| v.to_string()).collect();
    message::info(&format!(
        "Bans {} published version{}: {}.",
        banned_list.len(),
        if banned_list.len() == 1 { "" } else { "s" },
        banned_list.join(", ")
    ));
    if check.open_ended {
        message::warn(
            "This range has no upper bound: it also bans every FUTURE version, including fixes. Consider banning only the known-bad set (e.g. =1.6.0).",
        );
    }
    let before: Vec<Version> = installed_versions(&canonical);
    let hit: Vec<String> = before
        .iter()
        .filter(|v| check.banned.contains(v))
        .map(|v| v.to_string())
        .collect();
    if !hit.is_empty() {
        message::info(&format!(
            "Currently installed {} will be replaced on reinstall.",
            hit.join(", ")
        ));
    } else if !before.is_empty() {
        message::info("No currently installed version is banned; the exclusion is protective.");
    }

    if !yes && !confirm("Accept?") {
        message::info("Exclusion not applied.");
        return Ok(());
    }

    let manifest_before = fs::read_to_string("forest.json")?;
    let slot_key = match_override_key(&excludes, &canonical).unwrap_or_else(|| canonical.clone());
    write_map_entry(&mut manifest, "excludes", slot_key, &new_range)?;

    if !reinstall_or_rollback(&manifest, &manifest_before).await? {
        return Ok(());
    }

    // The honest preview: what the versions actually became.
    let after = installed_versions(&canonical);
    if !before.is_empty() && before != after {
        message::info(&format!(
            "{}: {} -> {}",
            canonical,
            join_versions(&before),
            join_versions(&after)
        ));
    }
    message::success(&format!("Exclusion set: {} -> {}", canonical, new_range));
    Ok(())
}

/// Validate an exclusion range against the published list: it must parse,
/// ban at least one published version (else it's a typo), and must not ban
/// every published version (the package would become uninstallable).
fn validate_exclude_range(
    range: &str,
    versions: &[Version],
    pkg: &str,
) -> std::result::Result<ExcludeCheck, String> {
    let range = range.trim();
    if range.is_empty() {
        return Err("Enter a SemVer range of versions to ban (e.g. =1.6.0).".to_string());
    }
    let Ok(req) = VersionReq::parse(range) else {
        return Err(format!("\"{}\" is not a valid SemVer range.", range));
    };
    let banned: Vec<Version> = versions.iter().filter(|v| req.matches(v)).cloned().collect();
    if banned.is_empty() {
        return Err(format!(
            "{} bans no published versions of {}; check the range (exact versions need =, e.g. =1.6.0).",
            range, pkg
        ));
    }
    if banned.len() == versions.len() {
        return Err(format!(
            "{} bans every published version of {} — the package could never be installed.",
            range, pkg
        ));
    }
    let open_ended = req.matches(&Version::new(u64::MAX >> 1, 0, 0));
    Ok(ExcludeCheck { banned, open_ended })
}

fn list_excludes(excludes: &HashMap<String, String>) {
    if excludes.is_empty() {
        message::info("No exclusions declared. Add one with `forest exclude <scope/name>`.");
        return;
    }
    let mut sorted: Vec<(&String, &String)> = excludes.iter().collect();
    sorted.sort_by_key(|(k, _)| k.to_lowercase());
    for (name, range) in sorted {
        let installed = installed_versions(name);
        let status = if installed.is_empty() {
            "not in tree".to_string()
        } else {
            format!("installed {}", join_versions(&installed))
        };
        message::info(&format!("{} -> bans {} ({})", name, range, status));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(list: &[&str]) -> Vec<Version> {
        list.iter().map(|v| Version::parse(v).unwrap()).collect()
    }

    #[test]
    fn exact_ban_lists_the_banned_version_and_is_bounded() {
        let avail = versions(&["1.5.2", "1.6.0", "1.6.1"]);
        let check = validate_exclude_range("=1.6.0", &avail, "s/p").unwrap();
        assert_eq!(check.banned, versions(&["1.6.0"]));
        assert!(!check.open_ended);
    }

    #[test]
    fn upper_open_range_is_flagged() {
        let avail = versions(&["1.5.2", "1.6.0"]);
        let check = validate_exclude_range(">1.5.2", &avail, "s/p").unwrap();
        assert!(check.open_ended);
    }

    #[test]
    fn banning_nothing_or_everything_is_rejected() {
        let avail = versions(&["1.5.2", "1.6.0"]);
        // "1.6.0" as a bare range means ^1.6.0 in semver — still valid here,
        // but a range matching zero published versions is a typo.
        assert!(validate_exclude_range("=9.9.9", &avail, "s/p").is_err());
        assert!(validate_exclude_range(">=0.0.0", &avail, "s/p").is_err());
        assert!(validate_exclude_range("junk", &avail, "s/p").is_err());
        assert!(validate_exclude_range("", &avail, "s/p").is_err());
    }

    #[test]
    fn bounded_window_ban_is_not_open_ended() {
        let avail = versions(&["1.5.2", "1.6.0", "1.6.5", "1.7.0"]);
        let check = validate_exclude_range(">=1.6.0, <1.7.0", &avail, "s/p").unwrap();
        assert_eq!(check.banned, versions(&["1.6.0", "1.6.5"]));
        assert!(!check.open_ended);
    }
}
