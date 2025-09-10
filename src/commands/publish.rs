use anyhow::{Context, Ok, Result};
use std::{env, fs, path::{Path}, sync::Arc};
use serde_json::Value;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use walkdir::WalkDir;
use dialoguer::{theme::ColorfulTheme, Input, Select};
use flate2::{write::GzEncoder, Compression};
use tar::Builder;
use reqwest::{multipart::{Form, Part}, StatusCode};

use crate::{http::{self, api_request}, message::{fail, warn}};
use crate::message::{Message, MessageType};

fn version_builder(current_version: &str) -> String {
    let mut field = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("What is the most significant update you made in this version?")
        .default(0)
        .items(&[
            "A bugfix",
            "A new feature that adds functionality",
            "A breaking change that changes how existing functions are used"
        ])
        .interact().unwrap_or(2);

    if field != 2 {
        let breaking_change  = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("If someone was already using this package in their code, would they have to change anything after your update?")
            .default(1)
            .items(&[
                "Yes",
                "No"
            ])
            .interact().unwrap_or_default();

        if breaking_change == 0 {
            field = 2;
        }
    }

    let current_version_parts = current_version.split('.').collect::<Vec<&str>>();
    let major = current_version_parts[0].parse::<u32>().unwrap();
    let minor = current_version_parts[1].parse::<u32>().unwrap();
    let patch = current_version_parts[2].parse::<u32>().unwrap();

    let new_version = match field {
        0 => format!("{}.{}.{}", major, minor, patch + 1),
        1 => format!("{}.{}.0", major, minor + 1),
        2 => format!("{}.0.0", major + 1),
        _ => current_version.to_string(), // Fallback to current version if something goes wrong

    };

    return new_version;

}

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
                //println!("Adding file: {:?}", rel);
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
    let cwd = env::current_dir().context("Failed to get current directory")?;

    let (session_resp, status_code) = api_request("v1/auth/session", reqwest::Method::GET, None, None)
        .await
        .context("Failed to get session information")?;
    
    if status_code == StatusCode::UNAUTHORIZED {
        fail("You must be logged in to publish a package. Please run `forest login`.");
        return Ok(());
    }

    // get user from user.username
    let current_user = session_resp.get("username")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Failed to get current user from session"))?;


    // Ensure manifest exists
    let manifest_path = cwd.join("forest.json");
    if !manifest_path.exists() {
        fail("No forest.json found in the current directory. Please run `forest init`.");
        return Ok(());
    }

    // Read and parse manifest
    let mut forest_json: Value = serde_json::from_str(&fs::read_to_string(&manifest_path)?)
        .context("Failed to parse forest.json")?;

    let mut metadata: Value = serde_json::json!({
        "public": true,
    });
    // TODO: Fetch user info from API to see what orgs they are allowed to publish to.

    let (userdata_resp, _) = api_request(format!("v1/user/{}", current_user).as_str(), reqwest::Method::GET, None, None)
        .await
        .context("Failed to get user information")?;

    let org_authors = userdata_resp.get("orgs") // "orgs" is an array of org data with { "name" : string, "rank" : string}
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("Failed to parse user org data"))?;

    let mut author_options = vec![format!("{} (You)", current_user)];
    for org in org_authors {
        let org_name = org.get("name").and_then(Value::as_str).unwrap();
        let org_rank  = org.get("rank").and_then(Value::as_str).unwrap();
        
        if org_rank == "admin" || org_rank == "owner" {
            // Only allow orgs where user is admin or owner
            author_options.push(org_name.to_string());
        }
    }

    if !forest_json["name"].is_string() {
        // Prompt for project name with validation
        let name: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Project name")
            .validate_with(|input: &String| {
                if input.is_empty() {
                    Err(anyhow::anyhow!("Package name cannot be empty"))
                } else if input.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("Invalid package name. Only lowercase letters, numbers, and hyphens are allowed."))
                }
            })
            .interact_text()?;

        forest_json["name"] = Value::String(name);
    }
    
    if !forest_json["author"].is_string() {
        let authors = author_options;
        let author = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Author name")
            .default(0)
            .items(&authors)
            .interact()?;

        forest_json["author"] = if author == 0 {
            // Use the default author name
            Value::String(current_user.to_string())
        } else {
            Value::String(authors[author].to_string())
        };
    }

    if !forest_json["description"].is_string() {
        // Prompt for description with default
        let description: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Project description")
            .default("A Forest package".into())
            .interact_text()?;

        forest_json["description"] = Value::String(description);
    }

    if forest_json["name"].is_string() && forest_json["platform"].is_string() {
        let platform = forest_json["platform"].as_str().unwrap().to_lowercase();
        let name = forest_json["name"].as_str().unwrap().to_lowercase();
        let (_latest_package_data, status_code) = api_request(&format!("v1/package/newuser1/{}/{}/latest", platform, name), reqwest::Method::GET, None, None)
            .await
            .context("Failed to fetch latest package data")?;

        if status_code.is_success() {
            // Do something with latest_package_data
        }
    }

   
    
 
    let mut new_version = if forest_json["version"].is_string() {
        version_builder(&forest_json["version"].as_str().unwrap())
    } else {
        "0.1.0".to_string()
    };

    let version_confirm = Select::with_theme(&ColorfulTheme::default())
        .with_prompt(format!("Version will be: {} Accept this version?", new_version))
        .default(0)
        .items(&["Yes", "No (Manually enter version)"])
        .interact()?;


    if version_confirm == 1 {
        warn("Entering a custom version is NOT recommended, as it can lead to unexpected behavior for developers using your package.");
        let version: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("What version is this? (SemVer format, e.g. 1.0.0)")
        .validate_with(|input: &String| {
            if input.is_empty() {
                Err(anyhow::anyhow!("Version cannot be empty"))
            } else if semver::Version::parse(input).is_ok() {
                Ok(())
            } else {
                Err(anyhow::anyhow!("Invalid version. Versions should be in the SemVer format 'MAJOR.MINOR.PATCH'"))
            }
        })
        .interact_text()?;

        new_version = Value::String(version).as_str().unwrap().to_string();
    }
    

    println!("New version: {}", new_version);

    // Set public flag
    let public = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("What visibility should this package have?")
        .default(0)
        .items(&["Public", "Private"])
        .interact()?;

    metadata["public"] = Value::Bool(public == 0);


     // Write forest.json with new version
    forest_json["version"] = Value::String(new_version);
    fs::write(&manifest_path, serde_json::to_string_pretty(&forest_json)?)
        .context("Failed to write updated forest.json")?;


    let msg = Message::new("Got manifest, preparing tarball...");

    // Prepare tarball
    let matcher = load_forest_ignore(&cwd);
    let tar_buf = create_tarball_buffer(&cwd, &matcher)
        .context("Failed to create package tarball")?;

    let file_size_bytes = tar_buf.len();
    // Build multipart form
    let forestjson_string = serde_json::to_string(&forest_json)
        .context("Failed to serialize forest.json")?;
    let metadata_string = serde_json::to_string(&metadata)
        .context("Failed to serialize metadata")?;
    let form_builder = Arc::new(move || {
        Form::new() // ORDER IS IMPORTANT. FILE MUST GO LAST.
            .part(
                "metadata",
                Part::text(metadata_string.clone()),
            )
            .part(
                "forestJson",
                Part::text(forestjson_string.clone())
            )
            .part(
                "file",
                Part::bytes(tar_buf.clone())
                    .file_name("package.tgz")
                    .mime_str("application/gzip")
                    .unwrap(),
            )
            
    });

    msg.update("Uploading package...");

    let mut hdrs = reqwest::header::HeaderMap::new();
    hdrs.insert("x-file-size", file_size_bytes.to_string().parse().unwrap());

    let (upload_response, upload_status) = api_request("v1/package/upload", reqwest::Method::POST, Some(http::RequestBody::Multipart(form_builder)), Some(hdrs))
        .await
        .context("Failed to upload package")?;

    println!("Upload response: {:?}", upload_response);
    
    if !upload_status.is_success() {
        msg.finish(MessageType::Fail, &format!("Failed to upload package: HTTP {}", upload_status));
        fail(&format!("Upload failed with status: {}", upload_status));
        return Ok(());
    }

    msg.finish(MessageType::Success, "Package uploaded successfully!");

    Ok(())
}
