use std::fs;
use anyhow::Result;

use crate::lockfile_gen::{sync_or_restore, Refresh, SyncOptions};
use crate::message::{Message, MessageType};
use crate::mounts::DepLocation;

/// Remove a dependency from a forest package. Without `mount`, from the one
/// mount that declares it.
pub async fn remove_command(target_package: String, mount: Option<String>) -> Result<()> {
    let Some(project) = super::context::load_project()? else {
        crate::message::info("No forest.json found, nothing to remove.");
        return Ok(());
    };
    let mut info = project.manifest;
    let mounts = crate::mounts::project_mounts(&info, project.platform)?;
    let scope = mount.as_deref().map(|r| crate::mounts::find_mount(&mounts, r)).transpose()?;
    let mut msg = Message::new("Removing...");

    // The reference may be the full scope/name, the alias, or the bare name.
    let (from, key) = match crate::mounts::locate_dep(&mounts, scope, &target_package) {
        DepLocation::NotFound => {
            let place = scope.map(|m| format!(" in {}", m.path)).unwrap_or_default();
            msg.finish(
                MessageType::Info,
                &format!("Package {} is not installed{}.", target_package, place),
            );
            return Ok(());
        }
        DepLocation::Ambiguous(candidates) => {
            msg.finish(
                MessageType::Warn,
                &format!(
                    "\"{}\" matches more than one installed package: {}. Use the full <scope>/<name>.",
                    target_package,
                    candidates.join(", ")
                ),
            );
            return Ok(());
        }
        DepLocation::InSeveral(found) => {
            msg.destroy();
            anyhow::bail!(
                "{} is a dependency of several mounts: {}. Pick one with --mount.",
                target_package,
                DepLocation::several_paths(&found)
            );
        }
        DepLocation::Found(mount, key) => (mount, key),
    };

    crate::mounts::deps_map_mut(&mut info, from)?.remove(&key);

    let manifest_before = fs::read_to_string("forest.json")?;
    fs::write("forest.json", serde_json::to_string_pretty(&info)?)?;

    let only = scope.map(|m| m.path.clone());
    let opts = SyncOptions::new(only, Refresh::Mounts(vec![from.path.clone()]));
    sync_or_restore(&info, &manifest_before, &mut msg, &opts).await?;

    let place = if from.is_default() { String::new() } else { format!(" from {}", from.path) };
    msg.finish(
        MessageType::Success,
        &format!("Package {} removed{}!", key, place),
    );

    Ok(())
}
