//! `forest mount`: list, create, remove, and rename a project's mounts.
//! Folder moves go through the platform (single rojo-safe renames),
//! forest.json edits through mounts.rs, and forest-lock.json edits through
//! lockfile_gen.rs.

use std::fs;
use std::path::Path;

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

use crate::lockfile_gen::{move_mount_section, sync_or_restore, Refresh, SyncOptions};
use crate::message::{info, success, warn, Message, MessageType};
use crate::mounts::{self, Mount};
use crate::platform::Platform;

fn load() -> Result<(Value, Platform, Vec<Mount>)> {
    let Some(project) = super::context::load_project()? else {
        bail!("No forest.json found. Run `forest init` first.");
    };
    let mounts = mounts::project_mounts(&project.manifest, project.platform)?;
    Ok((project.manifest, project.platform, mounts))
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{} {}", n, if n == 1 { one } else { many })
}

/// The first few names, then a count of the rest.
fn short_list(names: &[String]) -> String {
    let shown: Vec<&str> = names.iter().take(5).map(String::as_str).collect();
    match names.len() {
        n if n > 5 => format!("{} and {} more", shown.join(", "), n - 5),
        _ => shown.join(", "),
    }
}

/// The Rojo project line that maps a mount into the game.
fn rojo_hint(path: &str) {
    let name = path.rsplit('/').next().unwrap_or(path);
    info(&format!(
        "Map it in your Rojo project, e.g. \"{}\": {{ \"$path\": \"{}\" }}",
        name, path
    ));
}

pub fn mount_list() -> Result<()> {
    let (_, _, mounts) = load()?;
    let width = mounts.iter().map(|m| m.label().len()).max().unwrap_or(0);
    println!("{}:", plural(mounts.len(), "mount", "mounts"));
    for mount in &mounts {
        println!(
            "  {:<width$}  {}",
            mount.label(),
            plural(mount.deps.len(), "dependency", "dependencies")
        );
    }
    if mounts.len() == 1 {
        info("Add a mount with `forest mount create <path>`.");
    }
    Ok(())
}

pub async fn mount_create(path: String) -> Result<()> {
    let (mut manifest, platform, _) = load()?;
    if !platform.supports_mounts() {
        bail!("Mounts are not supported on {}.", platform.display_name());
    }
    let path = mounts::normalize_mount_path(&path)
        .map_err(|reason| anyhow!("Invalid mount path {}: {}", path, reason))?;
    mounts::check_new_mount(&manifest, platform, &path)?;

    // Install replaces everything at a mount's top level, so never adopt a
    // folder holding someone's files.
    let dir = Path::new(&path);
    if dir.exists() {
        if !dir.is_dir() {
            bail!("{} is a file, not a folder.", path);
        }
        let foreign = platform.foreign_mount_entries(dir);
        if !foreign.is_empty() {
            bail!(
                "{}/ already holds files forest didn't install: {}. Forest manages everything in a mount, so pick an empty or new folder.",
                path,
                short_list(&foreign)
            );
        }
    }

    let manifest_before = fs::read_to_string("forest.json")?;
    mounts::insert_mount(&mut manifest, &path, serde_json::Map::new());
    fs::write("forest.json", serde_json::to_string_pretty(&manifest)?)?;

    let mut msg = Message::new("Creating mount...");
    let opts = SyncOptions::new(Some(path.clone()), Refresh::Stale);
    sync_or_restore(&manifest, &manifest_before, &mut msg, &opts).await?;
    msg.finish(MessageType::Success, &format!("Created mount {}.", path));
    info(&format!("Add packages to it with `forest install <scope/name> --mount {}`.", path));
    rojo_hint(&path);
    Ok(())
}

pub async fn mount_remove(reference: String, yes: bool) -> Result<()> {
    let (mut manifest, platform, mounts) = load()?;
    let mount = mounts::find_mount(&mounts, &reference)?;
    if mount.is_default() {
        bail!(
            "{} is the default mount; it holds forest.json's top-level dependencies and can't be removed.",
            mount.path
        );
    }

    let prompt = format!(
        "Remove mount {}, its {}, and the {}/ folder?",
        mount.path,
        plural(mount.deps.len(), "dependency", "dependencies"),
        mount.path
    );
    if !yes {
        // Deleting a folder: Enter alone must not do it, and a script that
        // can't answer gets an error, not a silent no.
        let confirmed = dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt(&prompt)
            .default(false)
            .interact()
            .map_err(|_| anyhow!("Can't ask for confirmation here. Pass --yes to remove mount {}.", mount.path))?;
        if !confirmed {
            info("Mount not removed. Pass --yes to skip the prompt.");
            return Ok(());
        }
    }

    // The folder goes first: if that fails, nothing else has changed.
    platform.remove_mount_dir(&mount.path)?;
    mounts::remove_mount(&mut manifest, mount);
    fs::write("forest.json", serde_json::to_string_pretty(&manifest)?)?;
    let dropped = crate::links::drop_mount_links(Path::new("."), &mount.path)?;
    move_mount_section(&mount.path, None)?;

    success(&format!("Removed mount {}.", mount.path));
    if !dropped.is_empty() {
        info(&format!("Dropped {} into it.", plural(dropped.len(), "local link", "local links")));
    }
    info(&format!("Remove the $path for {} from your Rojo project too.", mount.path));
    Ok(())
}

pub async fn mount_rename(reference: String, new_path: String) -> Result<()> {
    let (manifest, platform, mounts) = load()?;
    let mount = mounts::find_mount(&mounts, &reference)?;
    let new_path = mounts::normalize_mount_path(&new_path)
        .map_err(|reason| anyhow!("Invalid mount path {}: {}", new_path, reason))?;
    if new_path == mount.path {
        info(&format!("Mount {} is already there.", mount.path));
        return Ok(());
    }
    let case_only = new_path.eq_ignore_ascii_case(&mount.path);
    if !case_only && new_path.to_ascii_lowercase().starts_with(&format!("{}/", mount.path.to_ascii_lowercase())) {
        bail!("A mount can't move inside itself.");
    }
    let new_name = new_path.rsplit('/').next().expect("normalized paths have a name").to_string();

    let mut renamed = manifest.clone();
    if mount.is_default() {
        let parent = |p: &str| p.rsplit_once('/').map(|(dir, _)| dir.to_ascii_lowercase()).unwrap_or_default();
        if parent(&new_path) != parent(&mount.path) {
            let sibling = match mount.path.rsplit_once('/') {
                Some((dir, _)) => format!("{}/{}", dir, new_name),
                None => new_name.clone(),
            };
            bail!(
                "The default mount stays next to your root file, so only its name can change, e.g. `forest mount rename {} {}`.",
                mount.path,
                sibling
            );
        }
        platform.set_default_mount_name(&mut renamed, &new_name)?;
    } else {
        mounts::rename_mount(&mut renamed, mount, &new_path);
    }
    // The same checks a hand-edited forest.json would get.
    mounts::project_mounts(&renamed, platform)?;

    let target = Path::new(&new_path);
    if !case_only && target.exists() {
        // An empty folder is fine to take over; anything else is someone's.
        let empty = target.is_dir() && fs::read_dir(target)?.next().is_none();
        if !empty {
            bail!("{} already exists. Pick a new folder, or empty it first.", new_path);
        }
        fs::remove_dir(target)?;
    }

    // The folder moves first: if that fails, nothing else has changed.
    platform.move_mount_dir(&mount.path, &new_path)?;
    fs::write("forest.json", serde_json::to_string_pretty(&renamed)?)?;
    if !mount.is_default() {
        crate::links::rename_mount_links(Path::new("."), &mount.path, &new_path)?;
        move_mount_section(&mount.path, Some(&new_path))?;
    }

    success(&format!("Moved mount {} to {}.", mount.path, new_path));
    if new_name != mount.name() {
        warn(&format!(
            "Code that requires the folder by name must now use {} instead of {}.",
            new_name,
            mount.name()
        ));
    }
    info("Update its $path in your Rojo project.");
    if mount.is_default() && manifest.get("name").is_some() {
        info(&format!(
            "From your next publish, projects that install this package put its dependencies in {}/ too.",
            new_name
        ));
    }
    Ok(())
}
