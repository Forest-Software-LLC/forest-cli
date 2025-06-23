use std::{fs, path::Path};
use anyhow::Result;
use serde_json::{Value, Map};
use urlencoding::encode;
use reqwest::Method;

use crate::http::api_request;
use crate::message::{Message, MessageType};
use crate::lockfile_gen::{lockfile_gen, make_directories};

/// Install dependencies for a forest package.
pub async fn install_command(
    target_package: Option<String>,
    version: Option<String>,
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
    let deps = info.get_mut("dependencies").unwrap().as_object_mut().unwrap();

    if let Some(pkg) = target_package {
        // Fetch package info
        let ver: String = version.unwrap_or_else(|| "latest".to_string());
        let endpoint = format!(
            "v1/package/get?packageId={}&version={}",
            encode(&pkg),
            encode(&ver)
        );
        let package_info = match api_request(&endpoint, Method::GET, None).await {
            Ok(data) => data,
            Err(e) => {
                msg.emit(
                    MessageType::Fail,
                    &format!("Failed to fetch package information: {}", e),
                );
                return Ok(());
            }
        };

        // Check existing install
        if deps.contains_key(&pkg) {
            msg.emit(
                MessageType::Info,
                &format!("Package {} is already installed.", pkg),
            );
            return Ok(());
        }

        // Add dependency
        let pkg_version = package_info
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        deps.insert(pkg.clone(), Value::String(format!("^{}", pkg_version)));
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
        if !Path::new("forest-lock.json").exists() {
            msg.emit(
                MessageType::Warn,
                "No lockfile found. Commit forest-lock.json to avoid inconsistencies.",
            );
            let info_clone = info.clone();
            let lockfile_content = lockfile_gen(&info_clone, &mut msg).await?;
            // Convert content to string
            let lockfile_content = serde_json::to_string_pretty(&lockfile_content)?;

            fs::write("forest-lock.json", lockfile_content)?;
        } else {
            let lock_content: Value = serde_json::from_str(
                &fs::read_to_string("forest-lock.json")?
            )?;
            let file_version = lock_content
                .get("fileVersion")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if file_version != 1 {
                msg.emit(
                    MessageType::Fail,
                    "Unsupported lockfile version. Delete forest-lock.json and run `forest i` again.",
                );
                return Ok(());
            }
            msg.destroy();
            make_directories(&serde_json::from_value(lock_content.clone()).unwrap()).await?;

            msg = Message::new("");

        }

        msg.emit(MessageType::Success, "Installed all dependencies!");
    }

    Ok(())
}
