use std::{fs, path::Path};
use anyhow::Result;
use serde_json::{Value, Map};

use crate::message::{Message, MessageType};
use crate::lockfile_gen::{lockfile_gen};
use crate::utils::{normalize_forest_deps, resolve_dep_ref, DepRef};

/// Remove a dependency from a forest package.
pub async fn remove_command(
    target_package: String,
) -> Result<()> {
    let mut msg = Message::new("Removing...");

    // Same manifest discovery as install: some platforms keep forest.json
    // away from the project root (UEFN: inside Content/).
    if !Path::new("forest.json").exists() {
        if let Some(manifest_dir) = crate::platform::discover_manifest_dir(&std::env::current_dir()?) {
            std::env::set_current_dir(&manifest_dir)?;
            msg.emit(
                MessageType::Info,
                &format!("Using manifest at {}", manifest_dir.join("forest.json").display()),
            );
        }
    }

    // Ensure forest.json exists
    if !Path::new("forest.json").exists() {
        msg.finish(
            MessageType::Info,
            "No forest.json found, nothing to remove.",
        );
        return Ok(());
    }

    // Read and parse forest.json
    let mut info: Value = serde_json::from_str(&fs::read_to_string("forest.json")?)?;
    // Ensure dependencies object exists
    if !info.get("dependencies").map_or(false, |v| v.is_object()) {
        info["dependencies"] = Value::Object(Map::new());
    }

    // The reference may be the full scope/name, the alias, or the bare name.
    let key = match resolve_dep_ref(&normalize_forest_deps(&info), &target_package) {
        DepRef::NotFound => {
            msg.finish(
                MessageType::Info,
                &format!("Package {} is not installed.", target_package),
            );
            return Ok(());
        }
        DepRef::Ambiguous(candidates) => {
            msg.finish(
                MessageType::Warn,
                &format!(
                    "\"{}\" matches more than one installed package: {}. Use the full <scope>/<name>.",
                    target_package,
                    candidates.join(", ")
                ),
            );
            return Ok(());
        }
        DepRef::Match(key) => key,
    };

    let deps = info.get_mut("dependencies").unwrap().as_object_mut().unwrap();
    deps.remove(&key);

    info["dependencies"] = Value::Object(deps.clone());

    fs::write("forest.json", serde_json::to_string_pretty(&info)?)?;

    // Generate and write lockfile using blocking context
    let info_clone = info.clone();
    let lockfile_content = lockfile_gen(&info_clone, &mut msg, false).await?;
    // Convert content to string
    let lockfile_content = serde_json::to_string_pretty(&lockfile_content)?;
    fs::write("forest-lock.json", lockfile_content)?;

    msg.finish(
        MessageType::Success,
        &format!("Package {} removed!", key),
    );

    Ok(())
}
