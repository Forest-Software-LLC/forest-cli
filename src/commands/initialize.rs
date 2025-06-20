use anyhow::Result;
use dialoguer::{Input, Select};
use serde_json::json;
use std::{env, fs, path::PathBuf};

use crate::message::{success, info};

/// Initialize a new Forest package in a subdirectory.
pub async fn init_command() -> Result<()> {
    // Prompt for project name with validation
    let name: String = Input::new()
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

    // Prompt for description with default
    let description: String = Input::new()
        .with_prompt("Project description")
        .default("A new Forest package".into())
        .interact_text()?;

    // Prompt for platform select
    let platforms = &["Roblox", "UEFN"];
    let selection = Select::new()
        .with_prompt("Platform")
        .items(platforms)
        .default(0)
        .interact()?;
    let platform = platforms[selection].to_lowercase();


    //▂▃▅▆▇█ ▓▒░
    // Build the manifest object
    let manifest = json!({
        "name": name,
        "description": description,
        "version": "0.1.0",
        "platform": platform,
        "main": "init.lua",
        "dependencies": serde_json::Map::<String, serde_json::Value>::new(),
    });
    let content = serde_json::to_string_pretty(&manifest)?;

    // Determine target directory
    let mut dir = env::current_dir()?;
    dir.push(&manifest["name"].as_str().unwrap());
    if !dir.exists() {
        fs::create_dir_all(&dir)?;
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
