use std::{fs, path::Path};
use anyhow::Result;
use serde_json::{Value, Map};
use urlencoding::encode;
use reqwest::Method;

use std::collections::{HashMap};
use crate::http::api_request;
use crate::lockfile_solver::DepSpec;
use crate::message::{Message, MessageType};
use crate::lockfile_gen::{lockfile_gen, make_directories};
use crate::utils::normalize_forest_deps;

/// Install dependencies for a forest package.
pub async fn install_command(
    target_package: Option<String>,
    version: Option<String>,
    alias: Option<String>,
) -> Result<()> {
    let mut msg = Message::new("Installing...");

    // Ensure forest.json exists
    if !Path::new("forest.json").exists() {
        msg.emit(
            MessageType::Fail,
            "No forest.json found. Run `forest init` to create a new package.",
        );
        return Ok(());
    }

    // Read and parse forest.json
    let mut info: Value = serde_json::from_str(&fs::read_to_string("forest.json")?)?;
    // Ensure dependencies object exists
    if !info.get("dependencies").map_or(false, |v| v.is_object()) {
        info["dependencies"] = Value::Object(Map::new());
    }

    let platform = info
        .get("platform")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing platform in forest.json"))?
        .to_string(); // clone the value so we don't hold a borrow

    let deps = info.get_mut("dependencies").unwrap().as_object_mut().unwrap();

    if let Some(pkg) = target_package {
        

        let mut package_identifiers : Vec<&str> = pkg.split("/").collect();


        if package_identifiers.len() != 2 {
            msg.finish(
                MessageType::Fail,
                "Invalid package identifier. Use format: <scope>/<name> -v [version]",
            );
            return Ok(());
        }        

        if package_identifiers[0].starts_with('@') {
            package_identifiers[0] = &package_identifiers[0][1..];
        }

        let resolved_alias = alias.clone().unwrap_or_else(|| package_identifiers[1].to_string());

        // `_`/`.`-prefixed folders in packages/ are exempt from install cleanup
        // (e.g. Wally's `_Index`), so aliases must not claim those names.
        if resolved_alias.starts_with('_') || resolved_alias.starts_with('.') {
            msg.finish(
                MessageType::Fail,
                &format!("Alias {} cannot start with '_' or '.'", resolved_alias),
            );
            return Ok(());
        }

        // Fetch package info
        let ver: String = version.unwrap_or_else(|| "latest".to_string());
        let endpoint = format!(
            "v1/package/{}/{}/{}/{}",
            encode(package_identifiers[0]), // scope
            encode(&platform), // platform
            encode(package_identifiers[1]), // name 
            encode(&ver) // version
        );
        let (package_info, status_code) = match api_request(&endpoint, Method::GET, None, None).await {
            Ok(data) => data,
            Err(e) => {
                msg.emit(
                    MessageType::Fail,
                    &format!("Failed to fetch package information: {}", e),
                );
                return Ok(());
            }
        };

        if !status_code.is_success() {
            msg.emit(
                MessageType::Fail,
                &format!(
                    "Failed to fetch package information for {}: HTTP {}",
                    pkg, status_code
                ),
            );
            return Ok(());
        }

        // Check existing install
        if deps.contains_key(&pkg) {
            msg.emit(
                MessageType::Info,
                &format!("Package {} is already installed.", pkg),
            );
            return Ok(());
        }

        // Check for alias conflicts
        let normalized_root_deps: HashMap<String, DepSpec> = deps.iter()
            .map(|(name, val)| {
                let default_alias = name.split('/').last().unwrap_or(name).to_string();
                let spec = if let Some(vstr) = val.as_str() {
                    DepSpec { alias: default_alias.clone(), version: vstr.to_string() }
                } else if let Some(obj) = val.as_object() {
                    let version = obj.get("version").and_then(Value::as_str).unwrap_or("").to_string();
                    let alias = obj.get("alias").and_then(Value::as_str).map(|s| s.to_string()).unwrap_or(default_alias.clone());
                    DepSpec { alias, version }
                } else {
                    DepSpec { alias: default_alias.clone(), version: String::new() }
                };
                (name.clone(), spec)
            })
            .collect();


        if normalized_root_deps.values().any(|spec| &spec.alias == &resolved_alias) {
            //TODO: Prompt for a new alias.
            msg.emit(
                MessageType::Fail,
                &format!("Alias {} is already in use by another package.", resolved_alias),
            );
            return Ok(());
        }

        // Add dependency
        let pkg_version = package_info
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();


        if alias.is_some() {
            deps.insert(pkg.clone(), Value::Object({
                let mut map = Map::new();
                map.insert("version".to_string(), Value::String(format!("^{}", pkg_version)));
                map.insert("alias".to_string(), Value::String(resolved_alias));
                map
            }));
        } else {
            deps.insert(pkg.clone(), Value::String(format!("^{}", pkg_version)));
        }
        fs::write("forest.json", serde_json::to_string_pretty(&info)?)?;

        // Generate and write lockfile using blocking context
        let info_clone = info.clone();
        let lockfile_content = lockfile_gen(&info_clone, &mut msg).await?;
        // Convert content to string
        let lockfile_content = serde_json::to_string_pretty(&lockfile_content)?;
        fs::write("forest-lock.json", lockfile_content)?;

        msg.finish(
            MessageType::Success,
            &format!("Package {} added!", pkg),
        );
    } else {
        // No specific package: install all via lockfile
        let lock_content: Option<Value> = if Path::new("forest-lock.json").exists() {
            Some(serde_json::from_str(&fs::read_to_string("forest-lock.json")?)?)
        } else {
            msg.emit(
                MessageType::Warn,
                "No lockfile found. Commit forest-lock.json to avoid inconsistencies.",
            );
            None
        };

        // Only the current format installs straight from the lockfile; anything
        // else (older format, unknown version) is re-resolved like a missing one.
        let usable = lock_content.as_ref()
            .and_then(|c| c.get("file_version"))
            .and_then(Value::as_u64)
            == Some(2);

        if usable {
            msg.destroy();
            make_directories(&serde_json::from_value(lock_content.unwrap()).unwrap(), normalize_forest_deps(&info.clone()), &platform).await?;

            msg = Message::new("");
        } else {
            if lock_content.is_some() {
                msg.emit(
                    MessageType::Warn,
                    "Lockfile format is out of date — regenerating forest-lock.json.",
                );
            }
            let info_clone = info.clone();
            let lockfile_content = lockfile_gen(&info_clone, &mut msg).await?;
            // Convert content to string
            let lockfile_content = serde_json::to_string_pretty(&lockfile_content)?;

            fs::write("forest-lock.json", lockfile_content)?;
        }

        msg.finish(MessageType::Success, "Installed all dependencies!");
    }

    Ok(())
}
