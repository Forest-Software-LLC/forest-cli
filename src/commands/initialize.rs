use anyhow::Result;
use dialoguer::{theme::ColorfulTheme, Select};
use serde_json::json;
use std::{env, fs, path::PathBuf};

use crate::message::{success, info, warn};

/// Initialize a new Forest package in a subdirectory.
///
/// `platform` lets callers skip the interactive picker (e.g. `forest init
/// --platform roblox`). When `None`, we prompt as before. This keeps `init`
/// scriptable/agent-friendly without an interactive terminal.
pub async fn init_command(platform: Option<String>) -> Result<()> {
    // Prompt for project name with validation
    /*let name: String = Input::new()
        .with_prompt("Project name")
        .validate_with(|input: &String| {
            if input.is_empty() {
                Err("Package name cannot be empty")
            } else if input.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
                Ok(())
            } else {
                Err("Invalid package name. Only lowercase letters, numbers, and hyphens are allowed.".into())
            }
        })
        .interact_text()?;
    */

    // Prompt for description with default
    /*let description: String = Input::new()
        .with_prompt("Project description")
        .default("A new Forest package".into())
        .interact_text()?;
    */

    if PathBuf::from("./forest.json").exists() {
        warn("forest.json already exists in the current directory. Please remove it before initializing a new project.");
        return Ok(());
    }

    // Resolve the platform: use the flag if given (validated), otherwise prompt.
    let platforms = &["Roblox", "UEFN"];
    let platform = match platform {
        Some(p) => {
            let normalized = p.trim().to_lowercase();
            if !platforms.iter().any(|valid| valid.to_lowercase() == normalized) {
                warn(&format!(
                    "Invalid platform '{}'. Supported platforms: roblox, uefn.",
                    p
                ));
                return Ok(());
            }
            normalized
        }
        None => {
            let selection = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("Platform (Use arrow keys to navigate)")
                .items(platforms)
                .default(0)
                .interact()?;
            platforms[selection].to_lowercase()
        }
    };

    // Build the manifest object
    let manifest = json!({
        //"name": name,
        //"description": description,
        //"version": "0.1.0",
        "dependencies": serde_json::Map::<String, serde_json::Value>::new(),
        "platform": platform,
        //"main": "init.lua",
    });
    let content = serde_json::to_string_pretty(&manifest)?;

    // Determine target directory
    let dir = env::current_dir()?;
    let packages_dir = dir.clone().join("packages");

    if !packages_dir.exists() {
        fs::create_dir_all(&packages_dir)?;
    }

    // Write forest.json
    let mut file_path = PathBuf::from(&dir);
    file_path.push("forest.json");
    fs::write(&file_path, content.as_bytes())?;

    // Success and info messages
    success(&format!("Initialized a new project in {}", dir.display()));
    info("You can now run `forest install` to install dependencies!");

    Ok(())
}
