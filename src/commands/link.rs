//! `forest link` / `forest unlink`: point a direct dependency at a local
//! working tree, machine-locally. State lives in gitignored
//! `.forest/links.json`; forest.json and forest-lock.json are never touched.
//! The overlay itself is applied by the platform install executor.

use std::fs;
use std::path::Path;

use anyhow::{anyhow, Result};
use serde_json::Value;
use walkdir::WalkDir;

use crate::lockfile_gen::LockFile;
use crate::links;
use crate::message::{fail, info, success, warn};
use crate::platform::Platform;
use crate::utils::{normalize_forest_deps, same_package};

/// Enter the manifest directory, mirroring install/remove discovery.
fn enter_manifest_dir() -> Result<bool> {
    if !Path::new("forest.json").exists() {
        if let Some(manifest_dir) = crate::platform::discover_manifest_dir(&std::env::current_dir()?) {
            std::env::set_current_dir(&manifest_dir)?;
            info(&format!(
                "Using manifest at {}",
                manifest_dir.join("forest.json").display()
            ));
        }
    }
    Ok(Path::new("forest.json").exists())
}

fn read_manifest() -> Result<Value> {
    Ok(serde_json::from_str(&fs::read_to_string("forest.json")?)?)
}

/// The lockfile-pinned root version for a dependency key.
fn pinned_version(lockfile: &Option<LockFile>, name: &str) -> Option<String> {
    lockfile
        .as_ref()
        .and_then(|lf| lf.pinned_version(name).map(str::to_string))
}

pub async fn link_command(path: Option<String>, list: bool) -> Result<()> {
    if !enter_manifest_dir()? {
        fail("No forest.json found. Run `forest init` first.");
        return Ok(());
    }
    let manifest = read_manifest()?;
    let platform = Platform::from_manifest(&manifest)?;

    let Some(path) = path.filter(|_| !list) else {
        print_links_list(&manifest);
        return Ok(());
    };

    if platform != Platform::Roblox {
        fail(&format!(
            "forest link is not supported on {} yet. UEFN packages can be authored directly inside the shared mount instead.",
            platform.display_name()
        ));
        return Ok(());
    }

    // The target must carry a manifest: it supplies the package's identity
    // and root; a bare folder is not linkable.
    let target = Path::new(&path);
    let target_manifest_path = target.join("forest.json");
    if !target.is_dir() {
        fail(&format!("{} is not a directory.", target.display()));
        return Ok(());
    }
    if !target_manifest_path.is_file() {
        fail(&format!(
            "{} has no forest.json; only Forest packages can be linked.",
            target.display()
        ));
        return Ok(());
    }
    let linked: Value = serde_json::from_str(&fs::read_to_string(&target_manifest_path)?)
        .map_err(|e| anyhow!("Failed to parse {}: {}", target_manifest_path.display(), e))?;

    if let Some(linked_platform) = linked.get("platform").and_then(Value::as_str) {
        if Platform::parse(linked_platform).ok() != Some(platform) {
            fail(&format!(
                "{} is a {} package; this project is {}.",
                target.display(),
                linked_platform,
                platform.as_str()
            ));
            return Ok(());
        }
    }

    let Some(linked_name) = linked.get("name").and_then(Value::as_str) else {
        fail(&format!(
            "{} has no `name` field; publish the package once (or add name/author to its forest.json) before linking it.",
            target_manifest_path.display()
        ));
        return Ok(());
    };

    // Identity: author/name when the linked manifest knows its author,
    // otherwise fall back to an unambiguous name-part match. A link is an
    // OVERRIDE; the package must already be a direct dependency.
    let root_deps = normalize_forest_deps(&manifest);
    let dep_key = match linked.get("author").and_then(Value::as_str) {
        Some(author) => {
            let full = format!("{}/{}", author, linked_name);
            root_deps.keys().find(|k| same_package(k, &full)).cloned().ok_or_else(|| {
                anyhow!(
                    "{} is not a dependency of this project. Add it first: `forest install {}`.",
                    full, full
                )
            })
        }
        None => {
            let matches: Vec<&String> = root_deps
                .keys()
                .filter(|k| k.rsplit('/').next().map_or(false, |n| n.eq_ignore_ascii_case(linked_name)))
                .collect();
            match matches.len() {
                1 => Ok(matches[0].clone()),
                0 => Err(anyhow!(
                    "No dependency named {} in forest.json. Add the package first, then link it.",
                    linked_name
                )),
                _ => Err(anyhow!(
                    "{} has no `author` field and \"{}\" matches several dependencies ({}). Add `author` to the linked forest.json.",
                    target_manifest_path.display(),
                    linked_name,
                    matches.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                )),
            }
        }
    };
    let dep_key = match dep_key {
        Ok(key) => key,
        Err(e) => {
            fail(&e.to_string());
            return Ok(());
        }
    };

    // Range mismatch is a warning, not an error: dev versions legitimately
    // run ahead of the declared range.
    let linked_version = linked.get("version").and_then(Value::as_str).unwrap_or("");
    if let Some(spec) = crate::utils::get_ci(&root_deps, &dep_key) {
        let satisfied = semver::VersionReq::parse(&spec.version)
            .ok()
            .zip(semver::Version::parse(linked_version).ok())
            .map(|(req, ver)| req.matches(&ver));
        if satisfied == Some(false) {
            warn(&format!(
                "Linked version {} does not satisfy the declared range {} for {}.",
                linked_version, spec.version, dep_key
            ));
        }
    }

    // A registry install scrubs Script/LocalScript files; a linked working
    // tree exposes them as-is, and they RUN on place load.
    warn_on_runnable_scripts(target, &linked);

    links::upsert_link(Path::new("."), &dep_key, &path)?;
    if links::ensure_gitignored(Path::new("."))? {
        info("Added .forest/ to .gitignore (link state is machine-local and must not be committed).");
    }

    // Apply immediately: the normal install pipeline picks the link up as an
    // overlay, restoring/keeping everything else registry-faithful. The
    // explicit mode makes `forest link` apply even where installs would
    // default to ignoring links (CI).
    super::install::install_command(None, None, None, false, None, Some(links::LinksMode::Apply)).await?;

    success(&format!("Linked {} → {}", dep_key, path));
    Ok(())
}

/// Count Script/LocalScript sources in the linked root dir; a registry
/// install would have scrubbed these.
fn warn_on_runnable_scripts(target: &Path, linked_manifest: &Value) {
    const RUNNABLE: [&str; 4] = [".server.lua", ".server.luau", ".client.lua", ".client.luau"];
    let root = linked_manifest
        .get("root")
        .and_then(Value::as_str)
        .unwrap_or("");
    let source_dir = match crate::utils::manifest_root_parent(root) {
        Some(parent) => target.join(parent),
        None => target.to_path_buf(),
    };
    let root_file = root.replace('\\', "/").rsplit('/').next().unwrap_or("").to_string();
    let count = WalkDir::new(&source_dir)
        .into_iter()
        .filter_entry(|e| e.file_name().to_str().map_or(true, |n| n != ".git"))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map_or(false, |n| RUNNABLE.iter().any(|s| n.ends_with(s)) && n != root_file)
        })
        .count();
    if count > 0 {
        warn(&format!(
            "The linked source contains {} Script/LocalScript file{} that a registry install would scrub; linked, they will run in your place.",
            count,
            if count == 1 { "" } else { "s" }
        ));
    }
}

pub async fn unlink_command(reference: Option<String>, all: bool) -> Result<()> {
    if !enter_manifest_dir()? {
        fail("No forest.json found here.");
        return Ok(());
    }
    let manifest = read_manifest()?;

    let stored = links::stored_links();
    if stored.is_empty() {
        info("No active links.");
        return Ok(());
    }

    let removed: Vec<links::StoredLink> = if all {
        let keys = links::remove_all(Path::new("."))?;
        stored.into_iter().filter(|l| keys.contains(&l.name)).collect()
    } else {
        let Some(reference) = reference else {
            fail("Pass a package (scope/name), a linked path, or --all.");
            return Ok(());
        };
        match links::remove_link(Path::new("."), &reference)? {
            Some(key) => stored.into_iter().filter(|l| l.name == key).collect(),
            None => {
                info(&format!("No active link matches {}; nothing to do.", reference));
                return Ok(());
            }
        }
    };

    // Clear each slot so the reinstall below restores the registry version
    // (re-extracted from the verified cache). Junctions are removed as
    // links, never through them.
    if Platform::from_manifest(&manifest)? == Platform::Roblox {
        let base = crate::roblox::packages_base(&manifest);
        let container = crate::roblox::packages_container(&manifest);
        let root_deps = normalize_forest_deps(&manifest);
        for link in &removed {
            if let Some(spec) = crate::utils::get_ci(&root_deps, &link.name) {
                let slot = crate::roblox::physical_path(
                    &base,
                    &container,
                    &crate::roblox::link_overlay::slot_plan_path(&container, &spec.alias),
                );
                crate::roblox::link_overlay::remove_slot(&slot)?;
            } else if let Ok(target) = fs::canonicalize(Path::new(&link.path)) {
                // Dep no longer declared: hunt down the orphaned link by its
                // target instead (copy-mode orphans are pruned by install).
                // The slot links the root PARENT, so probe the manifest's
                // root as well as the project dir itself.
                let mut candidates = vec![target.clone()];
                if let Some(parent) = fs::read_to_string(target.join("forest.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                    .and_then(|m| m.get("root").and_then(Value::as_str).map(str::to_string))
                    .and_then(|root| crate::utils::manifest_root_parent(&root))
                {
                    candidates.push(target.join(parent));
                }
                for candidate in candidates {
                    for slot in crate::roblox::link_overlay::find_slots_for_target(&base, &candidate) {
                        crate::roblox::link_overlay::remove_slot(&slot)?;
                    }
                }
            }
        }
    }

    for link in &removed {
        info(&format!("Unlinked {} (was → {})", link.name, link.path));
    }

    // Restore the exact registry versions via the normal pipeline. Apply
    // mode so any REMAINING links stay materialized while this one restores.
    super::install::install_command(None, None, None, false, None, Some(links::LinksMode::Apply)).await?;
    success(&format!(
        "Restored {} package{} from the registry.",
        removed.len(),
        if removed.len() == 1 { "" } else { "s" }
    ));
    Ok(())
}

/// `forest link --list` / bare `forest link`.
fn print_links_list(manifest: &Value) {
    let stored = links::stored_links();
    if stored.is_empty() {
        info("No active links. Link one with `forest link <path>`.");
        return;
    }
    let root_deps = normalize_forest_deps(manifest);
    let lockfile = LockFile::load();
    let (base, container) = if Platform::from_manifest(manifest).ok() == Some(Platform::Roblox) {
        (
            crate::roblox::packages_base(manifest),
            crate::roblox::packages_container(manifest),
        )
    } else {
        (String::new(), String::new())
    };

    println!(
        "{} link{} active:",
        stored.len(),
        if stored.len() == 1 { "" } else { "s" }
    );
    for link in &stored {
        let Some((dep_key, spec)) = root_deps.iter().find(|(k, _)| same_package(k, &link.name)) else {
            warn(&format!(
                "  {} → {} (no longer a dependency; `forest unlink {}` to clean up)",
                link.name, link.path, link.name
            ));
            continue;
        };
        let linked_manifest: Option<Value> = fs::read_to_string(Path::new(&link.path).join("forest.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok());
        let linked_version = linked_manifest
            .as_ref()
            .and_then(|m| m.get("version").and_then(Value::as_str))
            .unwrap_or("?")
            .to_string();
        let pin = pinned_version(&lockfile, dep_key).unwrap_or_else(|| "not installed".to_string());

        let mode = if base.is_empty() {
            String::new()
        } else {
            let slot = crate::roblox::physical_path(
                &base,
                &container,
                &crate::roblox::link_overlay::slot_plan_path(&container, &spec.alias),
            );
            if crate::roblox::link_overlay::is_link_dir(&slot) {
                ", live link".to_string()
            } else if slot.is_dir() {
                ", copy mode".to_string()
            } else {
                ", not applied, run `forest install`".to_string()
            }
        };
        println!(
            "  {} → {} (registry pin: {}, linked: {}{})",
            dep_key, link.path, pin, linked_version, mode
        );
        if linked_manifest.is_none() {
            warn(&format!("    target manifest unreadable at {}", link.path));
            continue;
        }

        // Dependency divergence vs the pinned registry version.
        if let Some(lf) = &lockfile {
            let pinned_deps = lf
                .root_entry(dep_key)
                .map(|e| e.dependencies.clone())
                .unwrap_or_default();
            let linked_deps = linked_manifest
                .as_ref()
                .map(crate::utils::manifest_dep_ranges)
                .unwrap_or_default();
            let probe = links::ActiveLink {
                name: dep_key.clone(),
                alias: spec.alias.clone(),
                path_display: link.path.clone(),
                source_dir: std::path::PathBuf::new(),
                root: String::new(),
                version: linked_version,
                dependencies: linked_deps,
            };
            for diff in links::dep_divergences(&probe, &pinned_deps) {
                println!("    dependency drift: {}", diff);
            }
        }
    }
}
