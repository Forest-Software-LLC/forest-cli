use std::{fs, path::Path};
use anyhow::Result;
use serde_json::{Value, Map};

use crate::message::{Message, MessageType};
use crate::lockfile_gen::{lockfile_gen};

/// Install dependencies for a forest package.
pub async fn remove_command(
    target_package: String,
) -> Result<()> {
    let mut msg = Message::new("Removing...");

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

    let deps = info.get_mut("dependencies").unwrap().as_object_mut().unwrap();
    if deps.contains_key(&target_package) == false {
        msg.finish(
            MessageType::Info,
            &format!("Package {} is not installed.", target_package),
        );
        return Ok(());
    } else {
        deps.remove(&target_package);
    }

    info["dependencies"] = Value::Object(deps.clone());

    fs::write("forest.json", serde_json::to_string_pretty(&info)?)?;

    // Generate and write lockfile using blocking context
    let info_clone = info.clone();
    let lockfile_content = lockfile_gen(&info_clone, &mut msg).await?;
    // Convert content to string
    let lockfile_content = serde_json::to_string_pretty(&lockfile_content)?;
    fs::write("forest-lock.json", lockfile_content)?;

    msg.finish(
        MessageType::Success,
        &format!("Package {} removed!", target_package),
    );

    Ok(())
}
