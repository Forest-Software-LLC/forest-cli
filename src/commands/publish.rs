use anyhow::{Context, Result};
use std::{env, fs, path::Path, sync::Arc};
use serde_json::Value;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use walkdir::WalkDir;
use dialoguer::{theme::ColorfulTheme, Input, Select};
use flate2::{write::GzEncoder, Compression};
use tar::Builder;
use reqwest::{multipart::{Form, Part}, StatusCode};

use crate::license_helper::{get_mit_license_text, detect_license, sanitize_spdx};
use crate::platform::{Platform, Preflight};
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

    bump_version(current_version, field)
}

fn bump_version(current_version: &str, field: usize) -> String {
    let Result::Ok(current) = semver::Version::parse(current_version) else {
        return current_version.to_string();
    };
    let (major, minor, patch) = (current.major, current.minor, current.patch);
    let pre = !current.pre.is_empty();

    match field {
        0 if pre => format!("{}.{}.{}", major, minor, patch),
        0 => format!("{}.{}.{}", major, minor, patch + 1),
        1 if pre && patch == 0 => format!("{}.{}.0", major, minor),
        1 => format!("{}.{}.0", major, minor + 1),
        2 if pre && minor == 0 && patch == 0 => format!("{}.0.0", major),
        2 => format!("{}.0.0", major + 1),
        _ => current_version.to_string(), // Fallback to current version if something goes wrong
    }
}

/// Find README.md in any casing (readme.md, Readme.md, etc).
fn find_readme(directory: &Path) -> Option<std::path::PathBuf> {
    fs::read_dir(directory).ok()?.flatten().find_map(|entry| {
        let path = entry.path();
        let is_readme = path.is_file()
            && entry
                .file_name()
                .to_str()
                .map_or(false, |n| n.eq_ignore_ascii_case("README.md"));
        is_readme.then_some(path)
    })
}

/// License for a private package with no license file. npm's proprietary
/// marker, not the SPDX `Unlicense`.
const PRIVATE_DEFAULT_LICENSE: &str = "UNLICENSED";

/// Scaffold README that publish offers to create.
const README_SCAFFOLD: &str = "# Package README\n\nThis is the README for the package.";

/// Whether a README has real content. Whitespace, markdown decoration, and
/// the scaffold text don't count, so empty files and stubs fail.
fn readme_is_substantial(contents: &str) -> bool {
    const MIN_CONTENT_CHARS: usize = 30;
    // Only count what the author actually wrote
    let authored = contents
        .replace("Package README", "")
        .replace("This is the README for the package.", "");
    let content_chars = authored
        .chars()
        .filter(|c| !c.is_whitespace() && !"#*-=_`>".contains(*c))
        .count();
    content_chars >= MIN_CONTENT_CHARS
}

/// Load ignore patterns from `.gitignore` and `.forestignore` (either may be
/// absent). `.forestignore` is applied last so its patterns override `.gitignore`.
/// `forced_patterns` go last of all so ignore files can't whitelist them back in.
fn load_forest_ignore(directory: &Path, forced_patterns: &[String]) -> Gitignore {
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

    for pattern in forced_patterns {
        if let Err(err) = builder.add_line(None, pattern) {
            warn(&format!("Failed to apply ignore pattern {}: {}", pattern, err));
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
///
/// `yes` (or CI) skips every prompt: fields come from forest.json, and
/// anything a prompt would fill in is an error.
pub async fn publish_command(yes: bool) -> Result<()> {
    let interactive = !yes && !crate::ci::is_ci();
    let cwd = env::current_dir().context("Failed to get current directory")?;

    if crate::api_token::env_api_token().is_some() {
        anyhow::bail!("API tokens are read only. Unset FOREST_TOKEN to publish with your login.");
    }
    if !interactive {
        info("Publishing without prompts (--yes or CI): everything is read from forest.json.");
    }

    // The spinner is destroyed before every prompt or printed line below -
    // dialoguer and an active spinner fight over the terminal.
    let msg = Message::new("Verifying session...");
    let session_result = api_request("v1/auth/session", reqwest::Method::GET, None, None).await;
    msg.destroy();
    let (session_resp, status_code) = session_result.context("Failed to get session information")?;

    if status_code == StatusCode::UNAUTHORIZED {
        anyhow::bail!("You must be logged in to publish a package. Please run `forest login`.");
    }

    // get user from user.username
    let current_user = session_resp.get("username")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Failed to get current user from session"))?;


    // Ensure manifest exists
    let manifest_path = cwd.join("forest.json");
    if !manifest_path.exists() {
        anyhow::bail!("No forest.json found in the current directory. Please run `forest init`.");
    }

    // Read and parse manifest
    let mut forest_json: Value = serde_json::from_str(&fs::read_to_string(&manifest_path)?)
        .context("Failed to parse forest.json")?;
    // Compared before the final write-back.
    let manifest_on_disk = forest_json.clone();

    // Overrides/excludes only apply to the project that declares them (the
    // registry strips the fields), so consumers of this package will resolve
    // its dependencies from their declared ranges alone.
    let has_overrides = !crate::utils::normalize_forest_overrides(&forest_json).is_empty();
    let has_excludes = !crate::utils::normalize_forest_excludes(&forest_json).is_empty();
    if has_overrides || has_excludes {
        let what = match (has_overrides, has_excludes) {
            (true, true) => "overrides and excludes",
            (true, false) => "overrides",
            _ => "excludes",
        };
        crate::message::warn(&format!(
            "forest.json declares {}; they apply only to this project and are ignored in the published package.",
            what
        ));
    }

    // Publishing while a dependency resolves through a local link is refused
    // outright: the tested tree is not the tree consumers will get, and the
    // linked dev version may not even be published. No override flag.
    {
        let deps = crate::utils::normalize_forest_deps(&forest_json);
        let linked: Vec<String> = crate::links::stored_links()
            .into_iter()
            .filter(|l| deps.keys().any(|k| crate::utils::same_package(k, &l.name)))
            .map(|l| l.name)
            .collect();
        if !linked.is_empty() {
            return Err(anyhow::anyhow!(
                "Cannot publish while dependencies are locally linked: {}. Run `forest unlink --all` (or unlink each) and re-test against the registry versions first.",
                linked.join(", ")
            ));
        }
    }

    // The platform owns every divergent step below (entry-point resolution,
    // naming rules, extra metadata, pre-pack lints); commands stay
    // platform-blind. A missing/unknown platform is a hard error here: the
    // registry requires it and every later call embeds it.
    let platform = Platform::from_manifest(&forest_json)?;

    let mut metadata: Value = serde_json::json!({

    });

    match platform.publish_preflight(&cwd, &mut forest_json, &mut metadata, interactive)? {
        Preflight::Continue => {}
        Preflight::Abort(reason) => anyhow::bail!(reason),
    }

    let declared_public = declared_visibility(&forest_json)?;

    // Fetch user info from API to see what orgs they are allowed to publish to.

    let msg = Message::new("Fetching account info...");
    let userdata_result = api_request(format!("v1/user/{}", current_user).as_str(), reqwest::Method::GET, None, None).await;
    msg.destroy();
    let (userdata_resp, _) = userdata_result.context("Failed to get user information")?;

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
        if !interactive {
            return Err(missing_field("name"));
        }
        // Naming rules are platform-owned (Verse identifiers on UEFN, the
        // classic letter/alnum/_/- rule on Roblox).
        let name: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Project name")
            .validate_with(move |input: &String| {
                platform
                    .validate_package_name(input)
                    .map_err(|reason| anyhow::anyhow!(reason))
            })
            .interact_text()?;

        forest_json["name"] = Value::String(name);
        did_set_name_or_author = true;
    }

    // Non-fatal naming advice (e.g. Roblox's hyphen/dot-indexing note).
    if let Some(note) = forest_json["name"].as_str().and_then(|n| platform.name_advisory(n)) {
        println!("{}", note);
    }

    if !forest_json["author"].is_string() {
        if !interactive {
            return Err(missing_field("author"));
        }
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

    // The author is settled now - make sure the package's physical location
    // agrees with it (platform-owned; UEFN checks the parent scope folder).
    if let Some(author) = forest_json["author"].as_str() {
        if let Err(reason) = platform.validate_publish_author(&cwd, author) {
            anyhow::bail!(reason);
        }
    }

    if !forest_json["description"].is_string() {
        if !interactive {
            return Err(missing_field("description"));
        }
        // Prompt for description with default
        let description: String = Input::with_theme(&ColorfulTheme::default())
            .with_prompt("Project description")
            .default("A Forest package".into())
            .interact_text()?;

        forest_json["description"] = Value::String(description);
    }

    let package_label = format!(
        "@{}/{}",
        forest_json["author"].as_str().unwrap_or_default(),
        forest_json["name"].as_str().unwrap_or_default()
    );

    let mut versions = vec![];
    // An unchanged license skips the confirm prompt.
    let mut published_license: Option<String> = None;
    if forest_json["name"].is_string() {
        let platform = platform.as_str();
        let name = forest_json["name"].as_str().unwrap().to_string();
        let msg = Message::new("Checking the registry for this package...");
        let versions_result = api_request(&format!("v1/package/{}/{}/{}", forest_json["author"].as_str().unwrap(), platform, name), reqwest::Method::GET, None, None).await;
        let latest_result = packages_api_request(&format!("v1/package/{}/{}/{}/latest", forest_json["author"].as_str().unwrap(), platform, name), reqwest::Method::GET, None, None).await;
        msg.destroy();
        let (versions_resp, status_code) = versions_result.context("Failed to fetch package versions")?;

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

        let (latest_package_data, status_code) = latest_result.context("Failed to fetch latest package data")?;

        if status_code.is_success() {
            metadata["public"] = latest_package_data["public"].clone();
            published_license = latest_package_data
                .get("license")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from);
            if let Some(public) = metadata["public"].as_bool() {
                info(&format!(
                    "Existing package is {}; this version will keep that visibility.",
                    visibility_word(public)
                ));
                // A publish never changes visibility.
                if let Some(declared) = declared_public.filter(|&d| d != public) {
                    anyhow::bail!(
                        "forest.json declares visibility \"{}\", but {} is {}. A publish never changes visibility; remove the field or set it to \"{}\".",
                        visibility_word(declared), package_label, visibility_word(public), visibility_word(public)
                    );
                }
            }
            if did_set_name_or_author {
                let version_confirm = Select::with_theme(&ColorfulTheme::default())
                    .with_prompt(format!("Package {} already exists, publish package anyways?", package_label))
                    .default(0)
                    .items(&["Yes", "No"])
                    .interact()?;

                if version_confirm == 1 {
                    fail("Publishing cancelled.");
                    return Ok(());
                }
            }
        } else {
            info("No existing package with this name. This publish will create it.");
        }
    }

    let new_version = if interactive {
        match choose_version_interactively(forest_json["version"].as_str(), &versions)? {
            Some(version) => version,
            None => {
                fail("Publishing cancelled.");
                return Ok(());
            }
        }
    } else {
        let Some(version) = forest_json["version"].as_str() else {
            return Err(missing_field("version"));
        };
        if semver::Version::parse(version).is_err() {
            anyhow::bail!("forest.json version {} isn't valid SemVer (MAJOR.MINOR.PATCH).", version);
        }
        if versions.iter().any(|v| v == version) {
            anyhow::bail!("{}@{} is already published. Bump \"version\" in forest.json.", package_label, version);
        }
        warn_if_below_newest(version, &versions);
        version.to_string()
    };
    // Set version in forest.json
    forest_json["version"] = Value::String(new_version);

    // Set public flag
    if !metadata["public"].is_boolean() {
        let public = match declared_public {
            Some(public) => public,
            None if interactive => {
                let choice = Select::with_theme(&ColorfulTheme::default())
                    .with_prompt("What visibility should this package have?")
                    .default(0)
                    .items(&["Public", "Private"])
                    .interact()?;
                choice == 0
            }
            None => anyhow::bail!(
                "{} has no published visibility to keep. Set \"visibility\" to \"public\" or \"private\" in forest.json.",
                package_label
            ),
        };
        metadata["public"] = Value::Bool(public);
    }
    let is_public = metadata["public"] == Value::Bool(true);

    // Must run after visibility is settled since the README requirement
    // depends on it. Any earlier and metadata["public"] is still null,
    // which reads as private.
    match find_readme(&cwd) {
        Some(readme_path) => {
            let readme_contents = fs::read_to_string(&readme_path)
                .context("Failed to read README.md")?;
            if !readme_is_substantial(&readme_contents) {
                anyhow::bail!("README.md is empty or nearly empty. Please describe what the package does and how to use it, then publish again.");
            }
            metadata["readme"] = Value::String(readme_contents);
        }
        None if is_public && !interactive => {
            anyhow::bail!("Public packages need a README.md describing what the package does and how to use it.");
        }
        None if is_public => {
            warn("No README.md found. It's required to include a README for public packages.");
            let create_readme = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("Would you like Forest to insert an empty README.md?")
                .default(0)
                .items(&["Yes", "No, I'll add my own."])
                .interact()?;

            if create_readme == 0 {
                fs::write(cwd.join("README.md"), README_SCAFFOLD)
                    .context("Failed to write README.md")?;
                info("Created README.md. Fill it in with how to use your package. Publishing it unedited will be rejected.");

                return Ok(());
            } else {
                fail("Publishing cancelled. Please add a README.md and try again.");
                return Ok(());
            }
        }
        None => {
            info("No README.md found. It's recommended to include a README for private packages, but not required.");
            metadata["readme"] = Value::String(String::new());
        }
    }


    // Find license file and infer license type
    // Attempt to locate a license file and infer its type, then compare with forest.json.
    let detected = detect_license(&cwd);

    if !interactive {
        let declared = forest_json["license"].as_str();
        let license = license_without_prompts(
            declared,
            detected.as_ref().map(|(id, inferred)| (id.as_str(), *inferred)),
            published_license.as_deref(),
            is_public,
        )
        .map_err(|reason| anyhow::anyhow!(reason))?;
        forest_json["license"] = Value::String(license);
    } else if let Some((license_spdx, inferred)) = detected {
        let mut target_spdx = license_spdx.clone();
        if published_license.as_deref() == Some(license_spdx.as_str()) {
            info(&format!("License: {} (same as the published version).", license_spdx));
        } else {
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
        }

        forest_json["license"] = Value::String(target_spdx);
    } else if !is_public {
        // No license file needed, but the registry requires the field.
        let license = private_license(forest_json["license"].as_str()).unwrap_or_else(|| {
            info("No license file found. Private packages don't need one, so this version is marked UNLICENSED.");
            PRIVATE_DEFAULT_LICENSE.to_string()
        });
        forest_json["license"] = Value::String(license);
    } else {
        let license_option = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("No license file found. Forest requires public packages to have a license file. What would you like to do?")
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
                // The manifest must declare it too - the registry requires
                // the field, and it must agree with the packaged text.
                forest_json["license"] = Value::String("MIT".to_string());
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

    // Prepare tarball. Platform-mandated exclusions (Roblox: the Packages/
    // mount + forest-lock.json when deps are declared) ride along here.
    let matcher = load_forest_ignore(&cwd, &platform.publish_ignores(&forest_json));

    // Platform pre-pack lint: the gateway hard-rejects these files, but
    // warning BEFORE the upload is the better error location.
    for warning in platform.prepack_warnings(&cwd, &matcher) {
        msg.emit(MessageType::Warn, &warning);
    }

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
        msg.destroy();
        anyhow::bail!(error_msg.to_string());
    }

    if !upload_status.is_success() {
        let error_msg = upload_response
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or(upload_status.as_str());
        msg.destroy();
        anyhow::bail!("Failed to upload package: {}", error_msg);
    }

    // Registry lint warnings (e.g. a uefn package exporting nothing) ride
    // the success response - surface them for every platform (the field is
    // simply absent for roblox today).
    if let Some(warnings) = upload_response.get("warnings").and_then(Value::as_array) {
        for warning in warnings.iter().filter_map(Value::as_str) {
            msg.emit(MessageType::Warn, warning);
        }
    }

    msg.finish(MessageType::Success, "Package uploaded successfully!");

    // Write back what publishing filled in (version, author, license).
    if forest_json != manifest_on_disk {
        fs::write(&manifest_path, serde_json::to_string_pretty(&forest_json)?)
            .context("Failed to write updated forest.json")?;
    }

    Ok(())
}

/// The error for a field a prompt would have asked for.
fn missing_field(field: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "forest.json has no \"{}\". Publishing without prompts reads everything from forest.json; add it and publish again.",
        field
    )
}

fn visibility_word(public: bool) -> &'static str {
    if public { "public" } else { "private" }
}

/// forest.json's optional `visibility`, read only by publish (the registry
/// drops it). Settles a new package's visibility, must match an existing one.
fn declared_visibility(forest_json: &Value) -> Result<Option<bool>> {
    match forest_json.get("visibility") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s == "public" => Ok(Some(true)),
        Some(Value::String(s)) if s == "private" => Ok(Some(false)),
        Some(other) => anyhow::bail!(
            "Invalid visibility {} in forest.json: expected \"public\" or \"private\".",
            other
        ),
    }
}

/// Where an interactive publish's version comes from.
#[derive(Debug, PartialEq)]
enum VersionSource {
    /// Unpublished forest.json version: confirm it as is.
    Manifest(String),
    /// Taken, or absent with versions published: run the bump questions.
    BumpFrom(String),
    /// First publish, no version in forest.json.
    Initial,
}

fn version_source(manifest_version: Option<&str>, published: &[String]) -> VersionSource {
    match manifest_version {
        Some(v) if published.iter().any(|p| p == v) => VersionSource::BumpFrom(v.to_string()),
        Some(v) => VersionSource::Manifest(v.to_string()),
        None => newest_published(published)
            .map(|newest| VersionSource::BumpFrom(newest.to_string()))
            .unwrap_or(VersionSource::Initial),
    }
}

fn newest_published(published: &[String]) -> Option<semver::Version> {
    published.iter().filter_map(|p| semver::Version::parse(p).ok()).max()
}

/// Lower than the newest is legal (a backport) but usually a stale forest.json.
fn warn_if_below_newest(version: &str, published: &[String]) {
    let (Ok(version), Some(newest)) = (semver::Version::parse(version), newest_published(published)) else {
        return;
    };
    if version < newest {
        warn(&format!("{} is lower than the newest published version, {}.", version, newest));
    }
}

/// Settle the version interactively. None means the user cancelled.
fn choose_version_interactively(manifest_version: Option<&str>, published: &[String]) -> Result<Option<String>> {
    let (proposed, prompt) = match version_source(manifest_version, published) {
        VersionSource::Manifest(v) if semver::Version::parse(&v).is_err() => {
            warn(&format!("forest.json version {} isn't valid SemVer.", v));
            return enter_version_manually(published).map(Some);
        }
        VersionSource::Manifest(v) => {
            warn_if_below_newest(&v, published);
            let prompt = format!("forest.json version {} isn't published yet. Publish it as the new version?", v);
            (v, prompt)
        }
        VersionSource::BumpFrom(v) => {
            let bumped = version_builder(&v);
            let prompt = format!("Version will be: {} Accept this version?", bumped);
            (bumped, prompt)
        }
        VersionSource::Initial => {
            let v = "0.1.0".to_string();
            let prompt = format!("Version will be: {} Accept this version?", v);
            (v, prompt)
        }
    };

    let version_confirm = Select::with_theme(&ColorfulTheme::default())
        .with_prompt(prompt)
        .default(0)
        .items(&["Yes", "No (Manually enter version)"])
        .interact()?;
    if version_confirm == 0 {
        return Ok(Some(proposed));
    }
    warn("Entering a custom version is NOT recommended, as it can lead to unexpected behavior for developers using your package.");
    enter_version_manually(published).map(Some)
}

fn enter_version_manually(published: &[String]) -> Result<String> {
    let published = published.to_vec();
    let version: String = Input::with_theme(&ColorfulTheme::default())
        .with_prompt("What version is this? (SemVer format, e.g. 1.0.0)")
        .validate_with(move |input: &String| {
            if input.is_empty() {
                Err(anyhow::anyhow!("Version cannot be empty"))
            } else if published.iter().any(|v| v == input) {
                Err(anyhow::anyhow!("Version already exists. Please choose a different version."))
            } else if semver::Version::parse(input).is_ok() {
                Ok(())
            } else {
                Err(anyhow::anyhow!("Invalid version. Versions should be in the SemVer format 'MAJOR.MINOR.PATCH'"))
            }
        })
        .interact_text()?;
    Ok(version)
}

/// A private package's declared license, when it has one.
fn private_license(declared: Option<&str>) -> Option<String> {
    declared.map(str::trim).filter(|l| !l.is_empty()).map(String::from)
}

/// License for a publish without prompts. forest.json's `license` wins, a
/// recognized license file fills it in, anything else is an error. Two
/// standard ids that disagree are an error too; the registry rejects them.
fn license_without_prompts(
    declared: Option<&str>,
    detected: Option<(&str, bool)>,
    published: Option<&str>,
    is_public: bool,
) -> Result<String, String> {
    let declared = private_license(declared).map(|d| sanitize_spdx(&d).to_string());
    match (declared, detected) {
        (Some(declared), Some((file_id, true))) => {
            let declared_is_spdx = crate::contracts::licenses().spdx_licenses.iter().any(|id| *id == declared);
            if declared_is_spdx && declared != file_id {
                return Err(format!(
                    "forest.json declares license {}, but the license file looks like {}. Make them agree and publish again.",
                    declared, file_id
                ));
            }
            Ok(declared)
        }
        (Some(declared), _) => {
            if detected.is_none() && is_public {
                return Err(public_license_file_required());
            }
            Ok(declared)
        }
        (None, Some((file_id, inferred))) if inferred || published == Some(file_id) => Ok(file_id.to_string()),
        (None, Some((file_id, _))) => Err(format!(
            "forest.json has no \"license\" and Forest doesn't recognize the license file. Set \"license\" in forest.json to an SPDX id, or to \"{}\" for a custom license.",
            file_id
        )),
        (None, None) if is_public => Err(public_license_file_required()),
        (None, None) => Ok(PRIVATE_DEFAULT_LICENSE.to_string()),
    }
}

fn public_license_file_required() -> String {
    "Public packages need a license file (LICENSE, LICENSE.txt, or LICENSE.md). Add one and publish again.".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tarball_entries(dir: &Path, forced: &[String]) -> Vec<String> {
        let matcher = load_forest_ignore(dir, forced);
        let buf = create_tarball_buffer(dir, &matcher).unwrap();
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(buf.as_slice()));
        let mut entries: Vec<String> = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().replace('\\', "/"))
            .collect();
        entries.sort();
        entries
    }

    fn published(list: &[&str]) -> Vec<String> {
        list.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn unpublished_manifest_version_skips_the_bump_questionnaire() {
        let taken = published(&["1.0.0", "1.1.0"]);
        assert_eq!(version_source(Some("1.2.0"), &taken), VersionSource::Manifest("1.2.0".into()));
        assert_eq!(version_source(Some("1.1.0"), &taken), VersionSource::BumpFrom("1.1.0".into()));
        // First publish: whatever forest.json says is the version.
        assert_eq!(version_source(Some("0.1.0"), &[]), VersionSource::Manifest("0.1.0".into()));
        // No manifest version: bump from the newest by SemVer, not list order.
        assert_eq!(
            version_source(None, &published(&["1.10.0", "1.9.0"])),
            VersionSource::BumpFrom("1.10.0".into())
        );
        assert_eq!(version_source(None, &[]), VersionSource::Initial);
    }

    #[test]
    fn visibility_field_accepts_public_or_private_only() {
        assert_eq!(declared_visibility(&serde_json::json!({})).unwrap(), None);
        assert_eq!(declared_visibility(&serde_json::json!({ "visibility": "public" })).unwrap(), Some(true));
        assert_eq!(declared_visibility(&serde_json::json!({ "visibility": "private" })).unwrap(), Some(false));
        assert!(declared_visibility(&serde_json::json!({ "visibility": "Public" })).is_err());
        assert!(declared_visibility(&serde_json::json!({ "visibility": true })).is_err());
    }

    #[test]
    fn license_without_prompts_prefers_the_manifest_and_errors_where_a_prompt_would_ask() {
        // Declared wins, canonicalized; a matching recognized file is fine.
        assert_eq!(license_without_prompts(Some("mit"), Some(("MIT", true)), None, true).unwrap(), "MIT");
        // Two standard ids that disagree: the registry would reject it.
        assert!(license_without_prompts(Some("Apache-2.0"), Some(("MIT", true)), None, true).is_err());
        // A custom declaration alongside an unrecognized file stands.
        assert_eq!(
            license_without_prompts(Some("LicenseRef-Acme"), Some(("SEE LICENSE IN LICENSE", false)), None, true).unwrap(),
            "LicenseRef-Acme"
        );
        // Undeclared: a recognized file fills it in, an unrecognized one only
        // when it matches what's already published.
        assert_eq!(license_without_prompts(None, Some(("MIT", true)), None, true).unwrap(), "MIT");
        assert!(license_without_prompts(None, Some(("SEE LICENSE IN LICENSE", false)), None, true).is_err());
        assert_eq!(
            license_without_prompts(None, Some(("SEE LICENSE IN LICENSE", false)), Some("SEE LICENSE IN LICENSE"), true).unwrap(),
            "SEE LICENSE IN LICENSE"
        );
        // No file: public is an error even with a declaration, private isn't.
        assert!(license_without_prompts(Some("MIT"), None, None, true).is_err());
        assert!(license_without_prompts(None, None, None, true).is_err());
        assert_eq!(license_without_prompts(Some("MIT"), None, None, false).unwrap(), "MIT");
        assert_eq!(license_without_prompts(None, None, None, false).unwrap(), PRIVATE_DEFAULT_LICENSE);
    }

    #[test]
    fn bump_version_increments_stable_versions() {
        assert_eq!(bump_version("1.2.3", 0), "1.2.4");
        assert_eq!(bump_version("1.2.3", 1), "1.3.0");
        assert_eq!(bump_version("1.2.3", 2), "2.0.0");
    }

    #[test]
    fn bump_version_finalizes_prereleases_instead_of_double_bumping() {
        // This used to panic: the old digit split hit "0-rc".parse::<u32>().
        assert_eq!(bump_version("1.0.0-rc.1", 0), "1.0.0");
        assert_eq!(bump_version("1.0.0-rc.1", 1), "1.0.0");
        assert_eq!(bump_version("1.0.0-rc.1", 2), "1.0.0");

        // Lower components already set: the bump is real, tag still drops.
        assert_eq!(bump_version("1.2.3-rc.1", 0), "1.2.3");
        assert_eq!(bump_version("1.2.3-rc.1", 1), "1.3.0");
        assert_eq!(bump_version("1.2.3-rc.1", 2), "2.0.0");
        assert_eq!(bump_version("1.2.0-rc.1", 1), "1.2.0");
        assert_eq!(bump_version("1.2.0-rc.1", 2), "2.0.0");
    }

    #[test]
    fn bump_version_returns_unparseable_versions_unchanged() {
        assert_eq!(bump_version("not-a-version", 0), "not-a-version");
        assert_eq!(bump_version("1.2", 1), "1.2");
    }

    #[test]
    fn readme_substance_check_rejects_empty_and_stub_readmes() {
        assert!(!readme_is_substantial(""));
        assert!(!readme_is_substantial("   \n\n  \t"));
        assert!(!readme_is_substantial("# MyPackage"));
        assert!(!readme_is_substantial("# MyPackage\n\n---\n\n> \n"));
        assert!(!readme_is_substantial(README_SCAFFOLD), "unedited scaffold must fail");
        // Padding the scaffold with markdown decoration must not sneak past.
        assert!(!readme_is_substantial(&format!("{}\n\n----\n####\n", README_SCAFFOLD)));

        assert!(readme_is_substantial(
            "# NavMesh\n\nGrid-based pathfinding for Roblox. Call NavMesh.new(grid) and :FindPath(a, b)."
        ));
    }

    #[test]
    fn forced_ignores_exclude_install_artifacts_and_beat_forestignore() {
        let base = std::env::temp_dir().join(format!("forest-publish-ignore-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("src")).unwrap();
        fs::create_dir_all(base.join("Packages").join("dep")).unwrap();
        fs::write(base.join("forest.json"), "{}").unwrap();
        fs::write(base.join("forest-lock.json"), "{}").unwrap();
        fs::write(base.join("src").join("init.luau"), "return {}").unwrap();
        fs::write(base.join("Packages").join("dep").join("init.lua"), "return {}").unwrap();
        // A whitelist in .forestignore must NOT re-include forced exclusions.
        fs::write(base.join(".forestignore"), "!Packages/\n!forest-lock.json\n").unwrap();

        let forced = vec!["/Packages/".to_string(), "/forest-lock.json".to_string()];
        assert_eq!(
            tarball_entries(&base, &forced),
            vec!["forest.json", "src/init.luau"]
        );

        // Without forced patterns the whitelist keeps them in (pre-existing
        // behavior for dep-less manifests).
        let entries = tarball_entries(&base, &[]);
        assert!(entries.contains(&"Packages/dep/init.lua".to_string()));
        assert!(entries.contains(&"forest-lock.json".to_string()));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn forced_ignores_exclude_the_nested_mount_of_a_rooted_manifest() {
        // A manifest with root src/init.luau mounts Packages inside src/;
        // the derived pattern must keep that mount out of the tarball.
        let base = std::env::temp_dir().join(format!("forest-publish-nested-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("src").join("Packages").join("dep")).unwrap();
        fs::write(base.join("forest.json"), "{}").unwrap();
        fs::write(base.join("forest-lock.json"), "{}").unwrap();
        fs::write(base.join("src").join("init.luau"), "return {}").unwrap();
        fs::write(base.join("src").join("Packages").join("dep").join("init.lua"), "return {}").unwrap();

        let manifest = serde_json::json!({
            "dependencies": { "acme/dep": "^1.0.0" },
            "root": "src/init.luau"
        });
        let forced = crate::roblox::publish::publish_ignores(&manifest);
        assert_eq!(
            tarball_entries(&base, &forced),
            vec!["forest.json", "src/init.luau"]
        );

        let _ = fs::remove_dir_all(&base);
    }
}

