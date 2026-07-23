//! Roblox `forest init` scaffold: minimal project manifest + the Packages
//! mount. When a `wally.toml` is present, offers to convert from Wally by
//! importing its dependencies (they resolve as-is: every wally package is
//! mirrored on the Forest registry under the same scope/name). Reached only
//! through the `Platform` seam.

use anyhow::Result;
use serde_json::{json, Map, Value};
use std::{env, fs, path::Path, path::PathBuf};

use crate::message::{info, success, warn};

pub fn init(cwd: &Path) -> Result<()> {
    if PathBuf::from("./forest.json").exists() {
        warn("forest.json already exists in the current directory. Please remove it before initializing a new project.");
        return Ok(());
    }

    // Wally conversion offer: pull the deps straight into forest.json.
    let mut dependencies = Map::new();
    let mut license: Option<String> = None;
    let wally_path = cwd.join("wally.toml");
    if wally_path.is_file() {
        let convert = dialoguer::Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt("Found wally.toml. Import its dependencies into forest.json? (Wally packages are mirrored on Forest, so they resolve as-is.)")
            .default(0)
            .items(&["Yes", "No"])
            .interact();
        // "No" or a non-interactive terminal: plain scaffold.
        if matches!(convert, Ok(0)) {
            match fs::read_to_string(&wally_path)
                .map_err(anyhow::Error::from)
                .and_then(|text| crate::roblox::wally::parse_wally_manifest(&text))
            {
                Ok(import) => {
                    for skipped in &import.skipped_malformed {
                        warn(&format!("wally.toml: skipped {}", skipped));
                    }
                    for dep in &import.dependencies {
                        let value = match &dep.alias {
                            Some(alias) => json!({ "version": dep.version, "alias": alias }),
                            None => Value::String(dep.version.clone()),
                        };
                        dependencies.insert(dep.full_name.clone(), value);
                    }
                    license = import.license;
                    let mut summary = format!(
                        "Imported {} dependencies from wally.toml.",
                        import.dependencies.len()
                    );
                    if import.skipped_dev > 0 {
                        summary.push_str(&format!(
                            " ({} dev-dependencies skipped: forest manifests don't model them yet.)",
                            import.skipped_dev
                        ));
                    }
                    info(&summary);
                }
                Err(err) => warn(&format!(
                    "Could not read wally.toml ({}). Initializing without imported dependencies.",
                    err
                )),
            }
        }
    }

    // Build the manifest object
    let mut manifest = json!({
        "dependencies": dependencies,
        "platform": "roblox",
    });
    if let Some(license) = license {
        manifest["license"] = Value::String(license);
    }
    let content = serde_json::to_string_pretty(&manifest)?;

    // Determine target directory
    let dir = env::current_dir()?;
    let packages_dir = dir.clone().join(crate::roblox::PACKAGES_DIR);

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
