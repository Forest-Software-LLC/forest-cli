use std::{fs, path::Path};
use anyhow::Result;
use serde_json::{Value, Map};
use urlencoding::encode;
use reqwest::Method;

use crate::http::packages_api_request;
use crate::message::{Message, MessageType};
use crate::lockfile_gen::{lockfile_gen, lockfile_satisfies_manifest, make_directories, LockFile};
use crate::utils::{normalize_forest_deps, normalize_forest_excludes, normalize_forest_overrides};

/// Install dependencies for a forest package.
pub async fn install_command(
    target_package: Option<String>,
    version: Option<String>,
    alias: Option<String>,
    force: bool,
    init_platform: Option<String>,
    links_mode: Option<crate::links::LinksMode>,
) -> Result<()> {
    match links_mode {
        Some(crate::links::LinksMode::Apply) => {
            crate::links::set_policy(crate::links::LinkPolicy::Apply);
        }
        Some(crate::links::LinksMode::Ignore) => {
            crate::links::set_policy(crate::links::LinkPolicy::Ignore("--links=ignore".to_string()));
        }
        // Forbid is enforced below (after manifest discovery, where the
        // links file lives); None keeps the default: ignore under CI,
        // apply otherwise.
        Some(crate::links::LinksMode::Forbid) | None => {}
    }
    let mut msg = Message::new("Installing...");

    // Some platforms keep the manifest away from the project root (UEFN:
    // inside Content/). When there's no forest.json here, ask the platform
    // seam whether one lives nearby; a local forest.json always wins.
    if !Path::new("forest.json").exists() {
        if let Some(manifest_dir) = crate::platform::discover_manifest_dir(&std::env::current_dir()?) {
            std::env::set_current_dir(&manifest_dir)?;
            msg.emit(
                MessageType::Info,
                &format!("Using manifest at {}", manifest_dir.join("forest.json").display()),
            );
        }
    }

    // Strict mode for CI: a links file with entries is a hard failure, so a
    // leaked .forest/links.json can never produce a non-lockfile install
    // silently. (Without the flag, links are ignored by default under CI;
    // see links::policy.)
    if links_mode == Some(crate::links::LinksMode::Forbid) {
        let stored = crate::links::stored_links();
        if !stored.is_empty() {
            msg.destroy();
            return Err(anyhow::anyhow!(
                "--links=forbid: {} local link{} configured in {} ({}). Unlink them or remove the file.",
                stored.len(),
                if stored.len() == 1 { " is" } else { "s are" },
                crate::links::LINKS_FILE,
                stored.iter().map(|l| l.name.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
    }

    // No manifest anywhere: create one on the spot, so `forest install`
    // works as someone's very first command. `--init <platform>` is the
    // non-interactive path; otherwise offer interactively. The platform
    // scaffold writes a minimal manifest (dependencies + platform, no name)
    // and knows where it belongs (UEFN: the project's Content folder).
    if !Path::new("forest.json").exists() {
        msg.pause();
        let chosen_platform = if let Some(p) = &init_platform {
            Some(crate::platform::Platform::parse(p)?)
        } else {
            let create = dialoguer::Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
                .with_prompt("No forest.json found. Create one in the current directory?")
                .default(0)
                .items(&["Yes", "No"])
                .interact();
            match create {
                Ok(0) => Some(crate::platform::Platform::detect_or_prompt(&std::env::current_dir()?)?),
                // "No" or a non-interactive terminal: keep the old behavior.
                _ => None,
            }
        };
        if let Some(plat) = chosen_platform {
            plat.init(&std::env::current_dir()?, crate::platform::InitMode::Project { from_install: true }, None).await?;
            // The scaffold may have placed the manifest elsewhere
            // (UEFN: Content/) - re-run discovery to land on it.
            if !Path::new("forest.json").exists() {
                if let Some(manifest_dir) = crate::platform::discover_manifest_dir(&std::env::current_dir()?) {
                    std::env::set_current_dir(&manifest_dir)?;
                }
            }
        }
        msg.resume();
        if !Path::new("forest.json").exists() {
            msg.emit(
                MessageType::Fail,
                "No forest.json found. Run `forest init` to create a new package, or pass --init <platform>.",
            );
            return Ok(());
        }
    } else if init_platform.is_some() {
        msg.emit(
            MessageType::Info,
            "forest.json already exists; ignoring --init.",
        );
    }

    // Read and parse forest.json
    let mut info: Value = serde_json::from_str(&fs::read_to_string("forest.json")?)?;
    // Ensure dependencies object exists
    if !info.get("dependencies").map_or(false, |v| v.is_object()) {
        info["dependencies"] = Value::Object(Map::new());
    }

    let platform = info
        .get("platform")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing platform in forest.json"))?
        .to_string(); // clone the value so we don't hold a borrow

    let plat = crate::platform::Platform::parse(&platform)?;

    // Some platforms reject aliases outright (UEFN: Verse has no cheap
    // re-export shims). Fail before any network work; the planner backstops
    // this for manifest-declared aliases.
    if alias.is_some() {
        if let Some(reason) = plat.alias_error() {
            msg.finish(MessageType::Fail, &reason);
            return Ok(());
        }
    }

    // Read before `deps` takes its mutable borrow of the manifest.
    let manifest_overrides = normalize_forest_overrides(&info);
    let normalized_root_deps = normalize_forest_deps(&info);
    let deps = info.get_mut("dependencies").unwrap().as_object_mut().unwrap();

    if let Some(pkg) = target_package {
        

        let package_identifiers : Vec<&str> = pkg.split("/").collect();


        if package_identifiers.len() != 2 {
            msg.finish(
                MessageType::Fail,
                "Invalid package identifier. Use format: <scope>/<name> -v [version]",
            );
            return Ok(());
        }        

        // Validate alias
        if let Some(a) = &alias {
            // `_`/`.`-prefixed folders in packages/ are exempt from install
            // cleanup (e.g. Wally's `_Index`), so aliases must not claim
            // those names.
            if a.starts_with('_') || a.starts_with('.') {
                msg.finish(
                    MessageType::Fail,
                    &format!("Alias {} cannot start with '_' or '.'", a),
                );
                return Ok(());
            }

            // Aliases become folder names (and `require` identifiers); path
            // separators would nest directories and break pointer files.
            if a.contains('/') || a.contains('\\') {
                msg.finish(
                    MessageType::Fail,
                    &format!("Alias {} cannot contain '/' or '\\'", a),
                );
                return Ok(());
            }
        }

        // Fetch package info
        let ver: String = version.unwrap_or_else(|| "latest".to_string());
        let endpoint = format!(
            "v1/package/{}/{}/{}/{}",
            encode(package_identifiers[0]), // scope
            encode(&platform), // platform
            encode(package_identifiers[1]), // name 
            encode(&ver) // version
        );
        let (package_info, status_code) = match packages_api_request(&endpoint, Method::GET, None, None).await {
            Ok(data) => data,
            Err(e) => {
                msg.emit(
                    MessageType::Fail,
                    &format!("Failed to fetch package information: {}", e),
                );
                return Ok(());
            }
        };

        if !status_code.is_success() {
            msg.emit(
                MessageType::Fail,
                &format!(
                    "Failed to fetch package information for {}: HTTP {}",
                    pkg, status_code
                ),
            );
            return Ok(());
        }

        // fall back to what was typed if fields are missing in the response
        let canonical_scope = package_info
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or(package_identifiers[0])
            .to_string();
        let canonical_name = package_info
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(package_identifiers[1])
            .to_string();
        let canonical_full = format!("{}/{}", canonical_scope, canonical_name);

        // Target name for the installed package.
        let resolved_name = alias.clone().unwrap_or_else(|| canonical_name.clone());

        if canonical_full != pkg {
            msg.emit(
                MessageType::Info,
                &plat.resolved_note(&pkg, &canonical_full, &resolved_name),
            );
        }

        // The override command refuses direct deps, but the reverse path
        // (installing a package that is already overridden) lands in a
        // split: transitive edges follow the override, this new direct dep
        // follows its own range. Legal, but never silently. (Exclusions
        // need no warning: they filter this new direct dep too.)
        if let Some(override_key) = manifest_overrides
            .keys()
            .find(|k| crate::utils::same_package(k, &canonical_full))
            .cloned()
        {
            msg.emit(
                MessageType::Warn,
                &format!(
                    "{} is overridden in forest.json; the override applies only to transitive occurrences, not this direct dependency. Remove it with `forest override {} --remove` if that is not intended.",
                    override_key, override_key
                ),
            );
        }

        // Already declared (case-insensitive: a hand-edited manifest key
        // that differs only by case is still the same package)? Declared is
        // not the same as ON DISK - a hand-deleted folder loses its receipt,
        // so materializing the lockfile below restores it. When everything
        // is present this ends in "Already up to date!".
        if let Some(existing_key) = deps.keys().find(|k| k.eq_ignore_ascii_case(&canonical_full)).cloned() {
            msg.emit(
                MessageType::Info,
                &format!("Package {} is already in forest.json. Verifying installed packages...", existing_key),
            );
            sync_from_lockfile(&info, msg, force).await?;
            return Ok(());
        }

        // Alias conflicts. Case-insensitive: aliases become folder names under packages/, and
        // Windows/macOS filesystems case-fold; `DataStream` and `datastream`
        // would silently merge into one directory.
        if normalized_root_deps.values().any(|spec| spec.alias.eq_ignore_ascii_case(&resolved_name)) {
            //TODO: Prompt for a new alias.
            msg.emit(
                MessageType::Fail,
                &format!("Alias {} is already in use by another package.", resolved_name),
            );
            return Ok(());
        }

        // Add dependency
        let pkg_version = package_info
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();


        if alias.is_some() {
            deps.insert(canonical_full.clone(), Value::Object({
                let mut map = Map::new();
                map.insert("version".to_string(), Value::String(format!("^{}", pkg_version)));
                map.insert("alias".to_string(), Value::String(resolved_name));
                map
            }));
        } else {
            deps.insert(canonical_full.clone(), Value::String(format!("^{}", pkg_version)));
        }
        fs::write("forest.json", serde_json::to_string_pretty(&info)?)?;

        // Generate and write lockfile using blocking context
        let info_clone = info.clone();
        let lockfile_content = lockfile_gen(&info_clone, &mut msg, force).await?;
        // Convert content to string
        let lockfile_content = serde_json::to_string_pretty(&lockfile_content)?;
        fs::write("forest-lock.json", lockfile_content)?;

        msg.finish(
            MessageType::Success,
            &format!("Package {} added!", canonical_full),
        );

        // Platform-specific usage snippet, when there is one.
        if let Some(note) = plat.added_note(&canonical_scope, &canonical_name) {
            crate::message::info(&note);
        }
    } else {
        // No specific package: install all via lockfile
        sync_from_lockfile(&info, msg, force).await?;
    }

    Ok(())
}

/// Materialize the tree from the lockfile (regenerating it when missing or
/// outdated). Shared tail of the bulk `forest install` and of a targeted
/// install whose dependency is already declared - in that case this is what
/// restores a hand-deleted package folder (its receipt died with it, so
/// reconciliation reinstalls it).
async fn sync_from_lockfile(
    info: &Value,
    mut msg: Message,
    force: bool,
) -> Result<()> {
    let lock_content: Option<Value> = if Path::new("forest-lock.json").exists() {
        Some(serde_json::from_str(&fs::read_to_string("forest-lock.json")?)?)
    } else {
        msg.emit(
            MessageType::Warn,
            "No lockfile found. Commit forest-lock.json to avoid inconsistencies.",
        );
        None
    };

    // Only the current format installs straight from the lockfile; anything
    // else (older format, unknown version, unparseable) is re-resolved like a
    // missing one.
    let lockfile: Option<LockFile> = match &lock_content {
        Some(c) if c.get("file_version").and_then(Value::as_u64) == Some(2) => {
            serde_json::from_value(c.clone()).ok()
        }
        _ => None,
    };

    // The lockfile is only trusted while it still satisfies forest.json's
    // declared ranges - a hand-edited range (^1.5.0 -> ^2.0.0) or a removed
    // dependency re-resolves instead of silently keeping the old pin.
    let lockfile = match lockfile {
        Some(lf) => {
            let roots = crate::platform::Platform::from_manifest(info)?
                .resolution_roots(normalize_forest_deps(info))?;
            if lockfile_satisfies_manifest(&lf, &roots, &normalize_forest_overrides(info), &normalize_forest_excludes(info)) {
                Some(lf)
            } else {
                msg.emit(
                    MessageType::Info,
                    "forest.json dependencies changed; updating forest-lock.json.",
                );
                None
            }
        }
        None => {
            if lock_content.is_some() {
                msg.emit(
                    MessageType::Warn,
                    "Lockfile format is out of date; regenerating forest-lock.json.",
                );
            }
            None
        }
    };

    if let Some(lockfile) = lockfile {
        msg.destroy();
        let summary = make_directories(&lockfile, normalize_forest_deps(&info.clone()), info, force).await?;

        msg = Message::new("");
        if summary.installed == 0 {
            msg.finish(MessageType::Success, "Already up to date!");
        } else {
            msg.finish(MessageType::Success, &format!(
                "Installed {} package{}!",
                summary.installed,
                if summary.installed == 1 { "" } else { "s" }
            ));
        }
        return Ok(());
    }

    let info_clone = info.clone();
    let lockfile_content = lockfile_gen(&info_clone, &mut msg, force).await?;
    // Convert content to string
    let lockfile_content = serde_json::to_string_pretty(&lockfile_content)?;

    fs::write("forest-lock.json", lockfile_content)?;

    msg.finish(MessageType::Success, "Installed all dependencies!");
    Ok(())
}
