use anyhow::{Context, Ok, Result};
use std::{env, fs, path::{Path, PathBuf}, sync::Arc};
use serde_json::Value;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use walkdir::WalkDir;
use dialoguer::{theme::ColorfulTheme, Input, Select};
use flate2::{write::GzEncoder, Compression};
use tar::Builder;
use reqwest::{multipart::{Form, Part}, StatusCode};

use crate::licensce_helper::{get_mit_license_text, detect_license, sanitize_spdx};
use crate::{http::{self, api_request, packages_api_request}, message::{fail, warn, info}};
use crate::message::{Message, MessageType};

fn open_url(url: &str) -> anyhow::Result<()> {
    open::that(url)?;
    Ok(())
}

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

/// Load ignore patterns from `.gitignore` and `.forestignore` (either may be
/// absent). `.forestignore` is applied last so its patterns override `.gitignore`.
fn load_forest_ignore(directory: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(directory);

    for ignore_name in [".gitignore", ".forestignore"] {
        let ignore_file = directory.join(ignore_name);
        if ignore_file.exists() {
            // builder.add parses the whole file and returns Some(err) on failure.
            if let Some(err) = builder.add(&ignore_file) {
                warn(&format!("Failed to parse {}: {}", ignore_name, err));
            }
        }
    }

    // allow unparseable patterns to just be warnings, not panics
    builder.build().expect("Parsing ignore files failed")
}

/// Create a gzipped tarball in-memory of the directory, honoring .gitignore /
/// .forestignore and skipping dotfiles/dot-directories by default.
fn create_tarball_buffer(dir: &Path, matcher: &Gitignore) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let enc = GzEncoder::new(&mut buf, Compression::default());
        let mut tar = Builder::new(enc);

        // filter_entry lets us skip recursing into ignored dirs
        let walker = WalkDir::new(dir).into_iter().filter_entry(|e| {
            // compute the path *inside* the package
            let rel = e.path().strip_prefix(dir).unwrap();
            // never prune the root of the walk itself
            if rel.as_os_str().is_empty() {
                return true;
            }
            // ignore dotfiles/dot-directories by default (.git, .gitignore,
            // .forestignore, .DS_Store, ...) so they never reach the tarball.
            // `.gitkeep` is an exception: it's how empty directories are
            // preserved, and tar only stores files, so dropping it would lose
            // the directory entirely.
            if e.file_name()
                .to_str()
                .map_or(false, |n| n.starts_with('.') && n != ".gitkeep")
            {
                return false;
            }
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
        
    });


    // Find the package's init file at the first or second level of the directory.
    // Roblox uses `.luau`, but `.lua` is still valid, so accept either.
    const INIT_FILES: [&str; 2] = ["init.luau", "init.lua"];

    // Honor an explicit `root` from forest.json; otherwise auto-detect the init file.
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
            'search: for entry in fs::read_dir(&cwd)? {
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
        let target_root: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Root file (init.luau or init.lua) not found. Please provide the relative path to your root file. (e.g. src/init.luau)")
            .validate_with(|input: &String| {
                if input.is_empty() {
                    Err(anyhow::anyhow!("Path cannot be empty"))
                } else if fs::metadata(cwd.join(input)).is_ok() {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("File does not exist at the provided path"))
                }
            })
            .interact_text()?;

        forest_json["root"] = Value::String(target_root);
    } else {
        forest_json["root"] = Value::String(init_lua_path.strip_prefix(&cwd).unwrap().to_string_lossy().to_string());
    }


    // Get readme if exists
    let readme_path = cwd.join("README.md");
    if readme_path.exists() {
        let readme_contents = fs::read_to_string(&readme_path)
            .context("Failed to read README.md")?;
        metadata["readme"] = Value::String(readme_contents);
    } else {
        if metadata["public"] == Value::Bool(true) {
            warn("No README.md found. It's required to include a README for public packages.");
            let create_readme = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("Would you like Forest to insert an empty README.md?")
                .default(0)
                .items(&["Yes", "No, I'll add my own."])
                .interact()?;

            if create_readme == 0 {
                fs::write(cwd.join("README.md"), "# Package README\n\nThis is the README for the package.".to_string())
                    .context("Failed to write README.md")?;
                info("Created empty README.md. Please edit it to include information about how to use your package.");
                
                return Ok(());
            } else {
                fail("Publishing cancelled. Please add a README.md and try again.");
                return Ok(());
            }
        } else {
            info("No README.md found. It's recommended to include a README for private packages, but not required.");
            metadata["readme"] = Value::String(String::new());
        }
    }
    

    // Fetch user info from API to see what orgs they are allowed to publish to.

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
        
        // TODO: actually check write permissions if not admin/owner

        if org_rank == "admin" || org_rank == "owner" {
            // Only allow orgs where user is admin or owner
            author_options.push(org_name.to_string());
        }
    }

    let mut did_set_name_or_author = false;
    if !forest_json["name"].is_string() {
        // Validate name
        let name: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Project name")
            .validate_with(|input: &String| {
                let mut chars = input.chars();
                let starts_with_letter = chars.next().map_or(false, |c| c.is_ascii_alphabetic());
                if !starts_with_letter {
                    Err(anyhow::anyhow!("Invalid package name. Names must start with a letter."))
                } else if chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("Invalid package name. Only letters, numbers, underscores, and hyphens are allowed."))
                }
            })
            .interact_text()?;

        forest_json["name"] = Value::String(name);
        did_set_name_or_author = true;
    }

    // hyphenated names can't be dot-indexed in Luau, so discourage without rejecting.
    if let Some(name) = forest_json["name"].as_str() {
        if name.contains('-') {
            println!(
                "warning: hyphenated package names can't be dot-indexed in Luau requires; consider PascalCase (e.g. \"{}\")",
                name.split('-')
                    .map(|part| {
                        let mut c = part.chars();
                        match c.next() {
                            Some(first) => first.to_ascii_uppercase().to_string() + c.as_str(),
                            None => String::new(),
                        }
                    })
                    .collect::<String>()
            );
        }
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

        did_set_name_or_author = true;
    }

    if !forest_json["description"].is_string() {
        // Prompt for description with default
        let description: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Project description")
            .default("A Forest package".into())
            .interact_text()?;

        forest_json["description"] = Value::String(description);
    }

    let mut versions = vec![];
    if forest_json["name"].is_string() && forest_json["platform"].is_string() {
        let platform = forest_json["platform"].as_str().unwrap().to_lowercase();
        let name = forest_json["name"].as_str().unwrap().to_string();
        let (versions_resp, status_code) = api_request(&format!("v1/package/{}/{}/{}", forest_json["author"].as_str().unwrap(), platform, name), reqwest::Method::GET, None, None)
            .await
            .context("Failed to fetch package versions")?;

        if status_code.is_success() {
            let versions_array = versions_resp.get("versions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();

            // Versions array is {version : string, createdAt: string}[]

            versions = versions_array.iter()
                .filter_map(|v| v.get("version").and_then(Value::as_str))
                .map(String::from)
                .collect::<Vec<String>>();
        }

        let (latest_package_data, status_code) = packages_api_request(&format!("v1/package/{}/{}/{}/latest", forest_json["author"].as_str().unwrap(), platform, name), reqwest::Method::GET, None, None)
            .await
            .context("Failed to fetch latest package data")?;

        if status_code.is_success() {
            metadata["public"] = latest_package_data["public"].clone();
            println!("Latest package visibility: {:?}", metadata["public"]);
            // Do something with latest_package_data
            if did_set_name_or_author {
                let version_confirm = Select::with_theme(&ColorfulTheme::default())
                    .with_prompt(format!("Package @{}/{} already exists, publish package anyways?", forest_json["author"].as_str().unwrap(), forest_json["name"].as_str().unwrap()))
                    .default(0)
                    .items(&["Yes", "No"])
                    .interact()?;

                if version_confirm == 1 {
                    fail("Publishing cancelled.");
                    return Ok(());
                }
            }
        } else {
            println!("No existing package found, defaulting to public visibility.");
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
            } else if versions.iter().any(|v| v == input) {
                Err(anyhow::anyhow!("Version already exists. Please choose a different version."))
            } else if semver::Version::parse(input).is_ok() {
                Ok(())
            } else {
                Err(anyhow::anyhow!("Invalid version. Versions should be in the SemVer format 'MAJOR.MINOR.PATCH'"))
            }
        })
        .interact_text()?;

        new_version = Value::String(version).as_str().unwrap().to_string();
        
    }
    // Set version in forest.json
    forest_json["version"] = Value::String(new_version);

    // Set public flag
    if !metadata["public"].is_boolean() {
        // Prompt for public/private
        let public = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("What visibility should this package have?")
            .default(0)
            .items(&["Public", "Private"])
            .interact()?;

        metadata["public"] = Value::Bool(public == 0);
    }


    // Find license file and infer license type
    // Attempt to locate a license file and infer its type, then compare with forest.json.

    if let Some((license_spdx, inferred )) = detect_license(&cwd) {
        let mut target_spdx = license_spdx.clone();
        if inferred {
            let correct_license = Select::with_theme(&ColorfulTheme::default())
                .with_prompt(format!("Detected license: '{}' Is this correct?", license_spdx))
                .default(0)
                .items(&["Yes", "No"])
                .interact()?;

            if correct_license == 1 {
                target_spdx.clear();
            }
        }

        if target_spdx.is_empty() {
            let identifier: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("Forest does not recognize the license in your license file. Please provide a valid SPDX License identifier.")
                .default(license_spdx.to_string())
                .interact_text()?;

            target_spdx = sanitize_spdx(identifier.as_str()).to_string();
        }

        forest_json["license"] = Value::String(target_spdx);
    } else {
        let license_option = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("No license file found. Forest requires PUBLIC packages to have a license file. What would you like to do?")
            .default(2)
            .items(&["Generate MIT License (Permissive & minimal conditions)", "Cancel and manually add a license", "Find a license (Open in browser)"])
            .interact()?;

        match license_option {
            0 => {
                let copyright_holder: String = Input::with_theme(&ColorfulTheme::default())
                    .with_prompt("Copyright Holder Name")
                    .default(current_user.to_string())
                    .interact_text()?;
                // Generate MIT license file
                let mit_text = get_mit_license_text(&copyright_holder);

                fs::write(cwd.join("LICENSE"), mit_text)
                    .context("Failed to write LICENSE file")?;

                info("Generated LICENSE file with MIT license.");
            }
            1 => {
                fail("Publishing cancelled. Please add a license file and try again.");
                return Ok(());
            }
            2 => {
                // Open browser to license info page
                if open_url("https://choosealicense.com/").is_ok() {
                    info("Opened browser to https://choosealicense.com/");
                } else {
                    fail("Failed to open browser.");
                }
                return Ok(());
            }
            _ => {}
        }
        
    }

    

    

    let mut msg = Message::new("Got manifest, preparing tarball...");

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

    let (upload_response, upload_status) = packages_api_request("v1/package/upload", reqwest::Method::POST, Some(http::RequestBody::Multipart(form_builder)), Some(hdrs))
        .await
        .context("Failed to upload package")?;
    
    if upload_status == StatusCode::TOO_MANY_REQUESTS {
        // The API's 429 message says why and how long until the next publish is allowed.
        let error_msg = upload_response
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("You're publishing too frequently. Please try again later.");
        msg.finish(MessageType::Fail, error_msg);
        return Ok(());
    }

    if !upload_status.is_success() {
        let error_msg = upload_response
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or(upload_status.as_str());
        msg.finish(MessageType::Fail, &format!("Failed to upload package: {}", error_msg));
        return Ok(());
    }

    msg.finish(MessageType::Success, "Package uploaded successfully!");

    // Write forest.json with new version
    
    fs::write(&manifest_path, serde_json::to_string_pretty(&forest_json)?)
        .context("Failed to write updated forest.json")?;

    Ok(())
}
