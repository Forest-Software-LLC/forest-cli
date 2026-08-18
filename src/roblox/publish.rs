//! Roblox publish preflight: entry-point (root) resolution and naming
//! rules. Reached only through the `Platform` seam.

use anyhow::Result;
use dialoguer::{theme::ColorfulTheme, Input};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

use crate::message::warn;
use crate::platform::Preflight;

/// Resolve the package's `root` (its init file): honor an explicit
/// forest.json value, auto-detect at the first or second directory level,
/// or prompt. Always writes `forest_json["root"]`; never aborts.
pub fn publish_preflight(cwd: &Path, forest_json: &mut Value) -> Result<Preflight> {
    // Validate `packagesDir` first: it ships as registry metadata and
    // becomes a folder name in every consumer's tree.
    if let Some(value) = forest_json.get("packagesDir") {
        let Some(dir) = value.as_str() else {
            anyhow::bail!("Invalid packagesDir in forest.json: must be a string.");
        };
        if let Err(reason) = crate::roblox::validate_packages_dir(dir) {
            anyhow::bail!("Invalid packagesDir in forest.json: {}", reason);
        }
    }

    // Roblox uses `.luau`, but `.lua` is still valid, so accept either.
    const INIT_FILES: [&str; 2] = ["init.luau", "init.lua"];

    let mut init_lua_path = match forest_json["root"].as_str() {
        Some(root) => cwd.join(root),
        None => cwd.join(INIT_FILES[0]),
    };
    if !init_lua_path.exists() {
        let mut found: Option<PathBuf> = None;

        // Top level first.
        for candidate in INIT_FILES {
            let top = cwd.join(candidate);
            if top.exists() {
                found = Some(top);
                break;
            }
        }

        // Then one directory deep.
        if found.is_none() {
            'search: for entry in fs::read_dir(cwd)? {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    for candidate in INIT_FILES {
                        let nested_init = path.join(candidate);
                        if nested_init.exists() {
                            found = Some(nested_init);
                            break 'search;
                        }
                    }
                }
            }
        }

        if let Some(p) = found {
            init_lua_path = p;
        }
    }

    if !init_lua_path.exists() {
        warn("Failed to resolve root for init.luau/init.lua");
        let cwd_owned = cwd.to_path_buf();
        let target_root: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Root file (init.luau or init.lua) not found. Please provide the relative path to your root file. (e.g. src/init.luau)")
            .validate_with(move |input: &String| {
                if input.is_empty() {
                    Err(anyhow::anyhow!("Path cannot be empty"))
                } else if fs::metadata(cwd_owned.join(input)).is_ok() {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("File does not exist at the provided path"))
                }
            })
            .interact_text()?;

        // Forward slashes always: a backslash root published from Windows
        // never prefix-matches on extraction elsewhere (see extract.rs).
        forest_json["root"] = Value::String(target_root.replace('\\', "/"));
    } else {
        forest_json["root"] = Value::String(
            init_lua_path.strip_prefix(cwd).unwrap().to_string_lossy().replace('\\', "/"),
        );
    }

    Ok(Preflight::Continue)
}

/// Ignore patterns forced onto the publish matcher: when the manifest
/// declares dependencies, the hoisted install mount and the lockfile are
/// install artifacts, not package content; consumers regenerate both from
/// the manifest, and packing them would ship every resolved dependency
/// inside the tarball. The mount lives wherever the manifest's `root` puts
/// it (e.g. `src/Packages/`), so the pattern is derived, not hardcoded.
pub fn publish_ignores(forest_json: &Value) -> Vec<String> {
    let has_deps = forest_json
        .get("dependencies")
        .and_then(Value::as_object)
        .map_or(false, |deps| !deps.is_empty());
    if !has_deps {
        return Vec::new();
    }
    vec![
        format!("/{}/", crate::roblox::packages_base(forest_json)),
        "/forest-lock.json".to_string(),
    ]
}

/// Roblox package-name rule: letter start, then letters/digits/`_`/`-`.
pub fn validate_package_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let starts_with_letter = chars.next().map_or(false, |c| c.is_ascii_alphabetic());
    if !starts_with_letter {
        return Err("Invalid package name. Names must start with a letter.".to_string());
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err("Invalid package name. Only letters, numbers, underscores, and hyphens are allowed.".to_string());
    }
    Ok(())
}

/// PascalCase a package name into a valid Luau identifier (hyphen segments
/// joined and capitalized: "nav-mesh" -> "NavMesh"). Shared by the hyphen
/// advisory and init's starter-module scaffold.
pub(crate) fn pascal_ident(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut c = part.chars();
            match c.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// Hyphenated names can't be dot-indexed in Luau, so discourage without
/// rejecting.
pub fn name_advisory(name: &str) -> Option<String> {
    if !name.contains('-') {
        return None;
    }
    Some(format!(
        "warning: hyphenated package names can't be dot-indexed in Luau requires; consider PascalCase (e.g. \"{}\")",
        pascal_ident(name)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_rule_matches_the_historic_behavior() {
        assert!(validate_package_name("DataStream").is_ok());
        assert!(validate_package_name("nav-mesh").is_ok(), "hyphens are legal on Roblox");
        assert!(validate_package_name("x_1").is_ok());
        assert!(validate_package_name("1thing").is_err());
        assert!(validate_package_name("_lead").is_err(), "must start with a letter");
        assert!(validate_package_name("a.b").is_err());
    }

    #[test]
    fn publish_ignores_pack_artifacts_only_when_deps_declared() {
        let with_deps = serde_json::json!({ "dependencies": { "roads": "^1.0.0" } });
        assert_eq!(
            publish_ignores(&with_deps),
            vec!["/Packages/".to_string(), "/forest-lock.json".to_string()]
        );

        let empty_deps = serde_json::json!({ "dependencies": {} });
        assert!(publish_ignores(&empty_deps).is_empty());
        assert!(publish_ignores(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn publish_ignores_follow_the_root_mount() {
        // A nested root moves the mount inside the root dir; the exclusion
        // must move with it or the tarball ships every installed dependency.
        let nested = serde_json::json!({
            "dependencies": { "roads": "^1.0.0" },
            "root": "src/init.luau"
        });
        assert_eq!(
            publish_ignores(&nested),
            vec!["/src/Packages/".to_string(), "/forest-lock.json".to_string()]
        );

        let top_level = serde_json::json!({
            "dependencies": { "roads": "^1.0.0" },
            "root": "init.luau"
        });
        assert_eq!(
            publish_ignores(&top_level),
            vec!["/Packages/".to_string(), "/forest-lock.json".to_string()]
        );
    }

    #[test]
    fn publish_ignores_follow_a_renamed_container() {
        // A renamed container is still the install mount: it must be
        // force-excluded (via packages_base) or the tarball ships every
        // resolved dependency inside the package.
        let renamed = serde_json::json!({
            "dependencies": { "roads": "^1.0.0" },
            "packagesDir": "roblox_packages"
        });
        assert_eq!(
            publish_ignores(&renamed),
            vec!["/roblox_packages/".to_string(), "/forest-lock.json".to_string()]
        );

        let nested = serde_json::json!({
            "dependencies": { "roads": "^1.0.0" },
            "root": "src/init.luau",
            "packagesDir": "roblox_packages"
        });
        assert_eq!(
            publish_ignores(&nested),
            vec!["/src/roblox_packages/".to_string(), "/forest-lock.json".to_string()]
        );
    }

    #[test]
    fn preflight_rejects_a_bad_packages_dir_early() {
        let dir = std::env::temp_dir().join(format!("forest-preflight-pd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("init.luau"), "return {}").unwrap();

        for bad in [serde_json::json!(".."), serde_json::json!("CON"), serde_json::json!(7)] {
            let mut manifest = serde_json::json!({ "root": "init.luau", "packagesDir": bad });
            // Preflight doesn't derive Debug, so match instead of unwrap_err.
            let err = match publish_preflight(&dir, &mut manifest) {
                Err(e) => e.to_string(),
                Ok(_) => panic!("{:?} must be rejected", bad),
            };
            assert!(err.contains("packagesDir"), "{:?}: {}", bad, err);
        }

        // A valid rename and the absent default both pass.
        let mut ok = serde_json::json!({ "root": "init.luau", "packagesDir": "roblox_packages" });
        assert!(publish_preflight(&dir, &mut ok).is_ok());
        let mut absent = serde_json::json!({ "root": "init.luau" });
        assert!(publish_preflight(&dir, &mut absent).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hyphen_advisory_suggests_pascal_case() {
        let note = name_advisory("nav-mesh-query").unwrap();
        assert!(note.contains("NavMeshQuery"), "{}", note);
        assert!(name_advisory("NavMesh").is_none());
    }
}
