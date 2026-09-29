//! A package reached under an old address. The registry answers with the
//! package's canonical scope/name, which differs from what was asked for
//! when the scope was claimed under a new name or the package was renamed.
//! These rules decide what forest.json and the resolution roots say after.
//!
//! A new scope or new casing only changes the key: the install folder
//! follows the canonical name. A new package NAME keeps the old name as the
//! install alias, so folders and requires written against it keep working.
//! A declared alias is never touched.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{Map, Value};

use crate::lockfile_solver::DepSpec;
use crate::message::{Message, MessageType};
use crate::mounts::Mount;
use crate::utils::digest_package_name;

/// The install name to keep when `typed` reaches `canonical` under a
/// different package name: the typed name, in its typed casing. None when
/// only the scope or casing differs.
pub fn kept_alias(typed: &str, canonical: &str) -> Option<String> {
    let typed_name = digest_package_name(typed).name;
    let canonical_name = digest_package_name(canonical).name;
    (!typed_name.eq_ignore_ascii_case(&canonical_name)).then_some(typed_name)
}

/// The notice for a package reached under a former name.
pub fn renamed_notice(typed: &str, canonical: &str, alias: &str) -> String {
    format!(
        "{} was renamed to {}. It installs as {}, so existing requires keep working.",
        typed, canonical, alias
    )
}

/// Resolution found root keys under an old address, and the lockfile is
/// keyed by the canonical one: rewrite the mount's manifest keys and re-key
/// its roots to match.
pub fn apply_renames(
    mount: &Mount,
    roots: &mut HashMap<String, DepSpec>,
    renames: &HashMap<String, String>,
    msg: &mut Message,
) -> Result<()> {
    let applied = rewrite_manifest_renames(mount, renames)?;
    for a in &applied {
        msg.emit(MessageType::Info, &a.notice);
    }
    let applied_by_key: HashMap<&str, &AppliedRename> =
        applied.iter().map(|a| (a.rename_key.as_str(), a)).collect();

    for (old_key, canonical) in renames {
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
            // Follow the manifest rewrite's decision; for keys the local
            // manifest doesn't hold (UEFN workspace roots) a default-looking
            // alias is treated as defaulted.
            let follows_canonical = applied_by_key
                .get(old_key.as_str())
                .map(|a| a.follows_canonical)
                .unwrap_or_else(|| {
                    spec.alias == digest_package_name(old_key).name
                        && kept_alias(old_key, canonical).is_none()
                });
            if follows_canonical {
                spec.alias = digest_package_name(canonical).name;
            }
            roots.insert(canonical.clone(), spec);
        }
    }
    Ok(())
}

/// One rename actually applied to the local manifest.
pub(crate) struct AppliedRename {
    /// The rename map's key, which is also the key the resolution roots carry.
    pub rename_key: String,
    /// True when the install folder follows the canonical package name: no
    /// explicit alias, and the package name itself didn't change.
    pub follows_canonical: bool,
    pub notice: String,
}

/// Persist renames into a mount's dependencies in the manifest in the
/// current directory (every command chdirs to the manifest dir before
/// resolving). Reads the file fresh so only the dependency entries change.
/// Keys the local manifest doesn't declare are skipped without error; under
/// UEFN the resolution roots span other workspace manifests.
fn rewrite_manifest_renames(mount: &Mount, renames: &HashMap<String, String>) -> Result<Vec<AppliedRename>> {
    let path = "forest.json";
    if !std::path::Path::new(path).exists() {
        return Ok(Vec::new());
    }
    let mut manifest: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let Ok(deps) = crate::mounts::deps_map_mut(&mut manifest, mount) else {
        return Ok(Vec::new());
    };
    let applied = canonicalize_deps(deps, renames);
    if !applied.is_empty() {
        std::fs::write(path, serde_json::to_string_pretty(&manifest)?)?;
    }
    Ok(applied)
}

/// Re-key dependencies whose registry identity is a different address.
/// Pure JSON transform, alias rules per the module doc. Following the
/// canonical casing is safe for claimed scopes: wally-era code requires the
/// wally ALIAS (e.g. `AnimNation`), which is the casing the claimed package
/// carries, never the mirror's lowercase name.
pub(crate) fn canonicalize_deps(
    deps: &mut Map<String, Value>,
    renames: &HashMap<String, String>,
) -> Vec<AppliedRename> {
    let mut applied = Vec::new();

    for (old_key, canonical) in renames {
        // The manifest's own casing of the key wins over the caller's.
        let Some(manifest_key) = deps.keys().find(|k| k.eq_ignore_ascii_case(old_key)).cloned() else {
            continue;
        };
        if deps.keys().any(|k| *k != manifest_key && k.eq_ignore_ascii_case(canonical)) {
            // Both names are declared; the canonical entry already wins at
            // install time; merging two version ranges is the user's call.
            continue;
        }

        let mut value = deps.remove(&manifest_key).expect("key came from deps");
        let has_explicit_alias = value
            .as_object()
            .map_or(false, |o| o.get("alias").map_or(false, Value::is_string));
        let kept = if has_explicit_alias { None } else { kept_alias(&manifest_key, canonical) };

        let notice = match &kept {
            Some(alias) => {
                value = with_alias(value, alias);
                format!("{} forest.json updated.", renamed_notice(&manifest_key, canonical, alias))
            }
            None => format!("{} is now published as {}; forest.json updated.", manifest_key, canonical),
        };

        deps.insert(canonical.clone(), value);
        applied.push(AppliedRename {
            rename_key: old_key.clone(),
            follows_canonical: !has_explicit_alias && kept.is_none(),
            notice,
        });
    }

    applied
}

/// A dependency entry (range string or object) with `alias` set.
fn with_alias(value: Value, alias: &str) -> Value {
    let mut entry = match value {
        Value::Object(map) => map,
        other => {
            let mut map = Map::new();
            map.insert("version".to_string(), other);
            map
        }
    };
    entry.insert("alias".to_string(), Value::String(alias.to_string()));
    Value::Object(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn renames(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    fn canonicalize(manifest: &mut Value, renames: &HashMap<String, String>) -> Vec<AppliedRename> {
        canonicalize_deps(manifest["dependencies"].as_object_mut().unwrap(), renames)
    }

    #[test]
    fn kept_alias_only_when_the_package_name_changes() {
        assert_eq!(
            kept_alias("gamebeast-gg/gamebeast", "gamebeast/RobloxSDK"),
            Some("gamebeast".to_string())
        );
        // Typed casing is what the project's requires use
        assert_eq!(
            kept_alias("gamebeast-gg/Gamebeast", "gamebeast/RobloxSDK"),
            Some("Gamebeast".to_string())
        );
        assert_eq!(kept_alias("michaeldougal/animnation", "chiefwildin/AnimNation"), None);
        assert_eq!(kept_alias("stratiz/datastream", "stratiz/DataStream"), None);
    }

    #[test]
    fn defaulted_alias_follows_the_canonical_name() {
        // Claimed scope: the lowercase mirror name becomes the native
        // casing, which matches the wally ALIAS (`AnimNation`) wally-era
        // code requires. The dep stays a plain string.
        let mut manifest = json!({
            "dependencies": { "michaeldougal/animnation": "^1.11.0" }
        });
        let applied = canonicalize(
            &mut manifest,
            &renames(&[("michaeldougal/animnation", "chiefwildin/AnimNation")]),
        );
        assert_eq!(applied.len(), 1);
        assert!(applied[0].follows_canonical);
        assert_eq!(
            manifest["dependencies"]["chiefwildin/AnimNation"],
            json!("^1.11.0")
        );
        assert!(manifest["dependencies"].get("michaeldougal/animnation").is_none());
    }

    #[test]
    fn renamed_package_keeps_its_old_install_name() {
        let mut manifest = json!({
            "dependencies": { "gamebeast-gg/gamebeast": "^0.10.1" }
        });
        let applied = canonicalize(
            &mut manifest,
            &renames(&[("gamebeast-gg/gamebeast", "gamebeast/RobloxSDK")]),
        );
        assert_eq!(applied.len(), 1);
        assert!(!applied[0].follows_canonical);
        assert!(applied[0].notice.contains("installs as gamebeast"));
        assert_eq!(
            manifest["dependencies"]["gamebeast/RobloxSDK"],
            json!({ "version": "^0.10.1", "alias": "gamebeast" })
        );
    }

    #[test]
    fn renamed_object_entry_gains_the_alias_and_keeps_its_fields() {
        let mut manifest = json!({
            "dependencies": { "gamebeast/gamebeast": { "version": "^0.10.1" } }
        });
        canonicalize(
            &mut manifest,
            &renames(&[("gamebeast/gamebeast", "gamebeast/RobloxSDK")]),
        );
        assert_eq!(
            manifest["dependencies"]["gamebeast/RobloxSDK"],
            json!({ "version": "^0.10.1", "alias": "gamebeast" })
        );
    }

    #[test]
    fn explicit_alias_is_left_untouched() {
        let mut manifest = json!({
            "dependencies": {
                "oldscope/animnation": { "version": "^1.0.0", "alias": "Anim" }
            }
        });
        let applied = canonicalize(
            &mut manifest,
            &renames(&[("oldscope/animnation", "newscope/AnimNation")]),
        );
        assert_eq!(applied.len(), 1);
        assert!(!applied[0].follows_canonical);
        assert_eq!(
            manifest["dependencies"]["newscope/AnimNation"],
            json!({ "version": "^1.0.0", "alias": "Anim" })
        );

        // Also across a package rename
        let mut manifest = json!({
            "dependencies": {
                "gamebeast-gg/gamebeast": { "version": "^0.10.1", "alias": "Gamebeast" }
            }
        });
        canonicalize(
            &mut manifest,
            &renames(&[("gamebeast-gg/gamebeast", "gamebeast/RobloxSDK")]),
        );
        assert_eq!(
            manifest["dependencies"]["gamebeast/RobloxSDK"],
            json!({ "version": "^0.10.1", "alias": "Gamebeast" })
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
        let applied = canonicalize(
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
        let applied = canonicalize(&mut manifest, &renames(&[("x/y", "z/y")]));
        assert!(applied.is_empty());
        assert_eq!(manifest["dependencies"]["a/b"], json!("^1.0.0"));
    }

    #[test]
    fn manifest_key_casing_wins_over_solver_casing() {
        let mut manifest = json!({
            "dependencies": { "MichaelDougal/AnimNation": "^1.11.0" }
        });
        let applied = canonicalize(
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
