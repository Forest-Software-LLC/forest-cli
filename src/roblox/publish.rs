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

        forest_json["root"] = Value::String(target_root);
    } else {
        forest_json["root"] = Value::String(init_lua_path.strip_prefix(cwd).unwrap().to_string_lossy().to_string());
    }

    Ok(Preflight::Continue)
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

/// Hyphenated names can't be dot-indexed in Luau, so discourage without
/// rejecting.
pub fn name_advisory(name: &str) -> Option<String> {
    if !name.contains('-') {
        return None;
    }
    let pascal: String = name
        .split('-')
        .map(|part| {
            let mut c = part.chars();
            match c.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + c.as_str(),
                None => String::new(),
            }
        })
        .collect();
    Some(format!(
        "warning: hyphenated package names can't be dot-indexed in Luau requires; consider PascalCase (e.g. \"{}\")",
        pascal
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
    fn hyphen_advisory_suggests_pascal_case() {
        let note = name_advisory("nav-mesh-query").unwrap();
        assert!(note.contains("NavMeshQuery"), "{}", note);
        assert!(name_advisory("NavMesh").is_none());
    }
}
