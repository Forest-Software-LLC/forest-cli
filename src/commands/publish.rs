use anyhow::{Context, Result};
use std::{env, fs, path::{Path}, sync::Arc};
use serde_json::Value;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use walkdir::WalkDir;
use dialoguer::{theme::ColorfulTheme, Input, Select};
use flate2::{write::GzEncoder, Compression};
use tar::Builder;
use reqwest::{multipart::{Form, Part}};

use crate::http::{self, api_request};
use crate::message::{Message, MessageType};

/// Load .forestignore patterns (or empty matcher if none).
fn load_forest_ignore(directory: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(directory);

    let ignore_file = directory.join(".forestignore");
    if ignore_file.exists() {
        // builder.add_line takes a source name & one pattern per-line,
        // but builder.add(ignore_file) will parse the file for you.
        builder.add(ignore_file);
    }

    // allow unparseable patterns to just be warnings, not panics
    builder.build().expect("Parsing .forestignore failed")
}

/// Create a gzipped tarball in-memory of the directory, honoring .forestignore.
fn create_tarball_buffer(dir: &Path, matcher: &Gitignore) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let enc = GzEncoder::new(&mut buf, Compression::default());
        let mut tar = Builder::new(enc);

        // filter_entry lets us skip recursing into ignored dirs
        let walker = WalkDir::new(dir).into_iter().filter_entry(|e| {
            // compute the path *inside* the package
            let rel = e.path().strip_prefix(dir).unwrap();
            // if the matcher says “ignore this dir”, return false to prune
            !matcher.matched(rel, e.file_type().is_dir()).is_ignore()
        });

        for entry in walker.filter_map(|e| e.ok()) {
            let path = entry.path();
            let rel = path.strip_prefix(dir).unwrap();
            // skip the root itself
            if rel.as_os_str().is_empty() {
                continue;
            }
            // only add files
            if entry.file_type().is_file() {
                println!("Adding file: {:?}", rel);
                tar.append_path_with_name(path, rel)
                    .with_context(|| format!("Failed to add file {:?} to tar", path))?;
            }
        }

        tar.finish()?;
    }
    Ok(buf)
}

/// Publish a forest package: tar up, multipart-post, and report via spinner.
pub async fn publish_command() -> Result<()> {
    let msg = Message::new("Publishing package...");
    let cwd = env::current_dir().context("Failed to get current directory")?;

    let session_resp = api_request("v1/auth/session", reqwest::Method::GET, None)
        .await
        .context("Failed to fetch session information")?;

    println!("Session: {:?}", session_resp);

    // Ensure manifest exists
    let manifest_path = cwd.join("forest.json");
    if !manifest_path.exists() {
        msg.finish(MessageType::Fail, "No forest.json found in the current directory. Please run `forest init`.");
        return Ok(());
    }

    // Read and parse manifest
    let mut manifest: Value = serde_json::from_str(&fs::read_to_string(&manifest_path)?)
        .context("Failed to parse forest.json")?;
    
    // TODO: Fetch user info from API to see what orgs they are allowed to publish to.

    if !manifest["name"].is_string() {
        // Prompt for project name with validation
        let name: String = Input::with_theme(&ColorfulTheme::default())
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

        manifest["name"] = Value::String(name);
    }

    if !manifest["description"].is_string() {
        // Prompt for description with default
        let description: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Project description")
            .default("A Forest package".into())
            .interact_text()?;

        manifest["description"] = Value::String(description);
    }

    

    let current_version = manifest["version"].as_str().unwrap_or("0.1.0").to_string();
    let version: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("What version is this? (SemVer format, e.g. 1.0.0)")
        .default(current_version.clone())
        .validate_with(|input: &String| {
            if input.is_empty() {
                Err("Version cannot be empty")
            } else if semver::Version::parse(input).is_ok() {
                Ok(())
            } else {
                Err("Invalid version. Versions should be in the SemVer format 'MAJOR.MINOR.PATCH'".into())
            }
        })
        .interact_text()?;

    manifest["version"] = Value::String(version);


    // Set public flag
    let public = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("What visibility should this package have?")
        .default(0)
        .items(&["Public", "Private"])
        .interact()?;

    manifest["public"] = Value::Bool(public == 0);

    msg.update("Got manifest, preparing tarball...");

    // Prepare tarball
    let matcher = load_forest_ignore(&cwd);
    let tar_buf = create_tarball_buffer(&cwd, &matcher)
        .context("Failed to create package tarball")?;

    // Build multipart form
    let metadata = serde_json::to_string(&manifest)?;
    let form_builder = Arc::new(move || {
        Form::new()
            .part(
                "file",
                Part::bytes(tar_buf.clone())
                    .file_name("package.tgz")
                    .mime_str("application/gzip")
                    .unwrap(),
            )
            .part(
                "metadata",
                Part::text(metadata.clone()),
            )
    });

    msg.update("Uploading package...");

    let resp = api_request("v1/package/upload", reqwest::Method::POST, Some(http::RequestBody::Multipart(form_builder)))
    .await
        .context("Failed to upload package")?;
        //     Ok(data) => data,
        //     Err(e) => {
        //         msg.finish(
        //             MessageType::Fail,
        //             &format!("Failed to upload: {}", e),
        //         );
        //         return Ok(());
        //     }
        // };

    // let file_name = resp
    //     .get("fileName")
    //     .and_then(Value::as_str)
    //     .unwrap_or("");

    // msg.finish(MessageType::Success, file_name);
    

    Ok(())
}
