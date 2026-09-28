use std::fs;
use anyhow::Result;
use serde_json::{Value, Map};
use urlencoding::encode;
use reqwest::Method;

use crate::http::{api_request, packages_api_request};
use crate::message::{Message, MessageType};
use crate::lockfile_gen::{sync, sync_or_restore, Refresh, SyncOptions};
use crate::utils::normalize_forest_overrides;

/// Install dependencies for a forest package. `mount` (a mount path or an
/// unambiguous tail of one) is where a new package goes, and limits a bulk
/// install to that mount.
#[allow(clippy::too_many_arguments)]
pub async fn install_command(
    target_package: Option<String>,
    version: Option<String>,
    alias: Option<String>,
    force: bool,
    init_platform: Option<String>,
    links_mode: Option<crate::links::LinksMode>,
    frozen: bool,
    mount: Option<String>,
) -> Result<()> {
    if frozen && target_package.is_some() {
        return Err(anyhow::anyhow!("--frozen installs the lockfile as is, so it can't add a package."));
    }
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
    let mut project = super::context::load_project()?;
    let mut msg = Message::new("Installing...");

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
    if project.is_none() {
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
            // (UEFN: Content/); re-run discovery to land on it.
            project = super::context::load_project()?;
        }
        msg.resume();
        if project.is_none() {
            msg.destroy();
            return Err(anyhow::anyhow!(
                "No forest.json found. Run `forest init` to create a new package, or pass --init <platform>."
            ));
        }
    } else if init_platform.is_some() {
        msg.emit(
            MessageType::Info,
            "forest.json already exists; ignoring --init.",
        );
    }

    let project = project.expect("checked above");
    let mut info = project.manifest;
    // Ensure dependencies object exists
    if !info.get("dependencies").map_or(false, |v| v.is_object()) {
        info["dependencies"] = Value::Object(Map::new());
    }

    // The raw manifest value, for registry endpoints.
    let platform = info
        .get("platform")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();

    let plat = project.platform;
    let mounts = match crate::mounts::project_mounts(&info, plat) {
        Ok(mounts) => mounts,
        Err(e) => {
            msg.destroy();
            return Err(e);
        }
    };
    let target_mount = match mount.as_deref().map(|r| crate::mounts::find_mount(&mounts, r)).transpose() {
        Ok(found) => found,
        Err(e) => {
            msg.destroy();
            return Err(e);
        }
    };
    let only = target_mount.map(|m| m.path.clone());

    // Some platforms reject aliases outright (UEFN: Verse has no cheap
    // re-export shims). Fail before any network work; the planner backstops
    // this for manifest-declared aliases.
    if alias.is_some() {
        if let Some(reason) = plat.alias_error() {
            msg.destroy();
            return Err(anyhow::anyhow!(reason));
        }
    }

    let bulk = SyncOptions { force, frozen, announce: true, ..SyncOptions::new(only.clone(), Refresh::Stale) };

    // Read before `deps` takes its mutable borrow of the manifest.
    let manifest_overrides = normalize_forest_overrides(&info);
    let add_to = target_mount.unwrap_or(&mounts[0]);
    let normalized_root_deps = add_to.deps.clone();
    let deps = crate::mounts::deps_map_mut(&mut info, add_to)?;

    if let Some(pkg) = target_package {
        

        let package_identifiers : Vec<&str> = pkg.split("/").collect();


        if package_identifiers.len() != 2 {
            msg.destroy();
            return Err(anyhow::anyhow!("Invalid package identifier. Use format: <scope>/<name> -v [version]"));
        }

        // Validate alias
        if let Some(a) = &alias {
            // The receipt scan skips `_`/`.`-prefixed folders, so a package
            // installed under such a name would never count as installed.
            if a.starts_with('_') || a.starts_with('.') {
                msg.destroy();
                return Err(anyhow::anyhow!("Alias {} cannot start with '_' or '.'", a));
            }

            // Aliases become folder names (and `require` identifiers); path
            // separators would nest directories and break pointer files.
            if a.contains('/') || a.contains('\\') {
                msg.destroy();
                return Err(anyhow::anyhow!("Alias {} cannot contain '/' or '\\'", a));
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
                msg.destroy();
                return Err(e.context("Failed to fetch package information"));
            }
        };

        if !status_code.is_success() {
            // The gateway's 404 carries no explanation. The registry's
            // version list does when the scope is one an upstream registry
            // reserves but forest cannot mirror (a hint), so on a miss ask it
            // once and print that under the error.
            let hint = if status_code.as_u16() == 404 {
                let list_path = format!(
                    "v1/package/{}/{}/{}",
                    encode(package_identifiers[0]),
                    encode(&platform),
                    encode(package_identifiers[1])
                );
                match api_request(&list_path, Method::GET, None, None).await {
                    Ok((body, _)) => body
                        .get("hint")
                        .and_then(|h| h.get("message"))
                        .and_then(Value::as_str)
                        .map(|s| s.to_string()),
                    Err(_) => None,
                }
            } else {
                None
            };
            let text = format!(
                "Failed to fetch package information for {}: HTTP {}{}{}",
                pkg,
                status_code,
                hint.map(|hint| format!("\n  {}", hint)).unwrap_or_default(),
                crate::lockfile_solver::not_found_hint(status_code)
            );
            msg.destroy();
            return Err(anyhow::anyhow!(text));
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
            let place = if mounts.len() > 1 { format!(" (mount {})", add_to.path) } else { String::new() };
            msg.emit(
                MessageType::Info,
                &format!("Package {} is already in forest.json{}. Verifying installed packages...", existing_key, place),
            );
            install_all(&info, msg, &bulk).await?;
            return Ok(());
        }

        // Alias conflicts. Case-insensitive: aliases become folder names under packages/, and
        // Windows/macOS filesystems case-fold; `DataStream` and `datastream`
        // would silently merge into one directory.
        if normalized_root_deps.values().any(|spec| spec.alias.eq_ignore_ascii_case(&resolved_name)) {
            //TODO: Prompt for a new alias.
            msg.destroy();
            return Err(anyhow::anyhow!("Alias {} is already in use by another package.", resolved_name));
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
        let manifest_before = fs::read_to_string("forest.json")?;
        fs::write("forest.json", serde_json::to_string_pretty(&info)?)?;

        let opts = SyncOptions { force, ..SyncOptions::new(only, Refresh::Mounts(vec![add_to.path.clone()])) };
        sync_or_restore(&info, &manifest_before, &mut msg, &opts).await?;

        let place = if add_to.is_default() { String::new() } else { format!(" to {}", add_to.path) };
        msg.finish(
            MessageType::Success,
            &format!("Package {} added{}!", canonical_full, place),
        );

        // Platform-specific usage snippet, when there is one.
        if let Some(note) = plat.added_note(&canonical_scope, &canonical_name) {
            crate::message::info(&note);
        }
    } else {
        // No specific package: install all via lockfile
        install_all(&info, msg, &bulk).await?;
    }

    Ok(())
}

/// Materialize every mount in the run from the lockfile, resolving the
/// mounts whose sections are missing or outdated. Shared tail of the bulk
/// `forest install` and of a targeted install whose dependency is already
/// declared; in that case this is what restores a hand-deleted package
/// folder (its receipt died with it, so reconciliation reinstalls it).
async fn install_all(info: &Value, mut msg: Message, opts: &SyncOptions) -> Result<()> {
    let outcome = sync(info, &mut msg, opts).await?;
    let installed = if outcome.resolved {
        "dependencies".to_string()
    } else {
        format!("{} package{}", outcome.installed, if outcome.installed == 1 { "" } else { "s" })
    };
    if let Some(note) = skipped_summary(&installed) {
        msg.finish(MessageType::Warn, &note);
    } else if outcome.resolved {
        msg.finish(MessageType::Success, "Installed all dependencies!");
    } else if outcome.installed == 0 {
        msg.finish(MessageType::Success, "Already up to date!");
    } else {
        msg.finish(MessageType::Success, &format!("Installed {}!", installed));
    }
    Ok(())
}

/// The finish line when the pool skipped private packages. main lists which
/// and why once the command returns.
fn skipped_summary(installed: &str) -> Option<String> {
    let skipped = crate::install_report::count();
    (skipped > 0).then(|| format!(
        "Installed {}; {} private package{} skipped.",
        installed,
        skipped,
        if skipped == 1 { " was" } else { "s were" }
    ))
}
