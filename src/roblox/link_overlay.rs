//! Roblox overlay for `forest link`: swap a linked dependency's installed
//! slot (`<mount>/<Alias>`) for a live view of the developer's working tree.
//!
//! The install pipeline runs on the registry graph as if no links existed;
//! the executor treats each linked slot as opaque (nothing under it is
//! scanned, installed, pruned, or pointer-written) and this module fills the
//! slot last. Primary mode is a junction (Windows) or symlink (Unix) to the
//! parent of the linked root module, the same directory extraction would
//! have produced, so edits are live. Fallback is a copy, used when link
//! creation fails or the root module isn't init-named (extraction renames
//! the root on install; a link can't represent that, a copy can).
//!
//! A linked package's own deps come from its own working tree (its nested
//! packages mount rides along inside the slot), never from this project's
//! graph; installing them here would write through the link into the
//! developer's source.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::links::ActiveLink;
use crate::roblox::scratch::TrashBin;

/// The plan-format path of a linked dependency's slot. Direct deps always
/// install at the top level of the mount.
pub fn slot_plan_path(container: &str, alias: &str) -> String {
    format!("./{}/{}", container, alias)
}

/// How one link was materialized, for user-facing messages.
pub enum AppliedMode {
    Junction,
    /// Copy fallback, with the reason a live link wasn't possible.
    Copy(String),
    /// Already in place from a previous run.
    AlreadyLinked,
}

/// Materialize every active link. Runs after extraction/pointers so a
/// pre-existing registry copy in the slot is simply replaced.
pub fn apply(base: &str, container: &str, active: &[ActiveLink], trash: &mut TrashBin) -> Result<Vec<(String, AppliedMode)>> {
    let mut applied = Vec::new();
    for link in active {
        let slot = crate::roblox::physical_path(base, container, &slot_plan_path(container, &link.alias));
        let mode = apply_one(&slot, link, trash)
            .with_context(|| format!("Failed to apply link for {}", link.name))?;
        applied.push((link.name.clone(), mode));
    }
    Ok(applied)
}

fn apply_one(slot: &Path, link: &ActiveLink, trash: &mut TrashBin) -> Result<AppliedMode> {
    // Idempotence: a slot already linking to this source is left untouched.
    if is_link_dir(slot) && resolves_to(slot, &link.source_dir) {
        return Ok(AppliedMode::AlreadyLinked);
    }
    remove_slot(slot, trash)?;

    // Extraction renames a non-init root module to init.<ext>; a live link
    // cannot represent that rename, so those packages get a copy.
    let rename = root_init_rename(link);
    if rename.is_none() {
        match create_dir_link(&link.source_dir, slot) {
            Ok(()) => return Ok(AppliedMode::Junction),
            Err(e) => {
                copy_tree(&link.source_dir, slot, None)?;
                return Ok(AppliedMode::Copy(format!(
                    "creating a directory link failed ({}); copied instead. Re-run `forest install` after editing the source.",
                    e
                )));
            }
        }
    }

    copy_tree(&link.source_dir, slot, rename.as_ref())?;
    Ok(AppliedMode::Copy(format!(
        "root module {} is not named init.*, so a live link can't mirror the install rename; copied instead. Re-run `forest install` after editing the source.",
        if link.root.is_empty() { "(none)".to_string() } else { link.root.clone() }
    )))
}

/// When the linked root module needs the install-time init rename, returns
/// (root file name, init.<ext>); None means a live link is representable.
fn root_init_rename(link: &ActiveLink) -> Option<(String, String)> {
    let root_file = link.root.rsplit('/').next().unwrap_or("");
    if root_file.is_empty() {
        // No declared root: linkable only if the tree already has an init
        // module (otherwise the copy can't fix it either; extraction of
        // such a package would have failed the same way).
        return None;
    }
    let stem = root_file.split('.').next().unwrap_or("");
    if stem == "init" {
        return None;
    }
    let ext = root_file.rsplit('.').next().filter(|e| *e != root_file).unwrap_or("luau");
    Some((root_file.to_string(), format!("init.{}", ext)))
}

/// Is this path a symlink / directory junction (without following it)?
pub fn is_link_dir(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Does the link at `path` resolve to the same directory as `target`?
fn resolves_to(path: &Path, target: &Path) -> bool {
    match (fs::canonicalize(path), fs::canonicalize(target)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Remove whatever occupies a slot: a junction/symlink is removed WITHOUT
/// following it (the developer's source must never be touched), a real
/// directory goes through the trash bin (in-place deletion streams
/// child-before-parent removal events, which crash a live rojo).
pub fn remove_slot(slot: &Path, trash: &mut TrashBin) -> Result<()> {
    let Ok(meta) = fs::symlink_metadata(slot) else {
        return Ok(());
    };
    if meta.file_type().is_symlink() {
        remove_link_path(slot)
            .with_context(|| format!("Failed to remove link at {}", slot.display()))?;
    } else if meta.is_dir() {
        trash.remove_dir_all(slot)
            .with_context(|| format!("Failed to clear {}", slot.display()))?;
    } else {
        fs::remove_file(slot)
            .with_context(|| format!("Failed to clear {}", slot.display()))?;
    }
    Ok(())
}

/// Delete a symlink/junction itself, never its target's contents.
pub fn remove_link_path(path: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        // Directory links (junctions and symlinkd) remove via remove_dir;
        // file symlinks via remove_file.
        fs::remove_dir(path).or_else(|_| fs::remove_file(path))
    }
    #[cfg(not(windows))]
    {
        fs::remove_file(path)
    }
}

fn create_dir_link(target: &Path, link: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        junction::create(target, link)
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(target, link)
    }
}

/// Copy the linked working tree into the slot. `.git` is skipped (huge and
/// meaningless in a mount); everything else copies verbatim so copy mode
/// exposes the same working tree a junction would, except the top-level
/// root module which is renamed to init.<ext> exactly like extraction does.
fn copy_tree(src: &Path, dst: &Path, rename: Option<&(String, String)>) -> Result<()> {
    fs::create_dir_all(dst).with_context(|| format!("Failed to create {}", dst.display()))?;
    for entry in fs::read_dir(src).with_context(|| format!("Failed to read {}", src.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let from = entry.path();
        let file_type = entry.file_type()?;
        let to_name = match rename {
            Some((root_file, init_name)) if *root_file == name && file_type.is_file() => init_name.clone(),
            _ => name,
        };
        let to = dst.join(&to_name);
        if file_type.is_dir() {
            // Recurse; the rename only applies at the top level.
            copy_tree(&from, &to, None)?;
        } else if file_type.is_file() {
            fs::copy(&from, &to)
                .with_context(|| format!("Failed to copy {}", from.display()))?;
        }
        // Symlinks inside the source tree are skipped: following them could
        // wander anywhere, and a mount copy has no use for them.
    }
    Ok(())
}

/// Find top-level mount entries that are directory links resolving to
/// `target`; used by unlink when the dependency (and thus its alias) is no
/// longer declared.
pub fn find_slots_for_target(base: &str, target: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(base) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| is_link_dir(p) && resolves_to(p, target))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn fixture(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("forest-overlay-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn test_trash(tag: &str) -> TrashBin {
        TrashBin::new(std::env::temp_dir().join(format!("forest-overlay-trash-{}-{}", tag, std::process::id())))
    }

    fn active(target: &Path, root: &str) -> ActiveLink {
        let source_dir = match root.rsplit_once('/') {
            Some((parent, _)) if !parent.is_empty() => target.join(parent),
            _ => target.to_path_buf(),
        };
        ActiveLink {
            name: "acme/knit".into(),
            alias: "Knit".into(),
            path_display: target.display().to_string(),
            source_dir,
            root: root.to_string(),
            version: "1.0.0".into(),
            dependencies: HashMap::new(),
        }
    }

    #[test]
    fn junction_mode_links_the_root_parent_and_is_idempotent() {
        let base = fixture("junction");
        let mut trash = test_trash("junction");
        let pkg = base.join("pkg");
        fs::create_dir_all(pkg.join("src")).unwrap();
        fs::write(pkg.join("forest.json"), "{}").unwrap();
        fs::write(pkg.join("src").join("init.luau"), "return 1").unwrap();
        let mount = base.join("Packages");
        fs::create_dir_all(&mount).unwrap();

        let link = active(&pkg, "src/init.luau");
        let slot = mount.join("Knit");

        let mode = apply_one(&slot, &link, &mut trash).unwrap();
        assert!(matches!(mode, AppliedMode::Junction), "expected a live link");
        assert!(is_link_dir(&slot));
        assert_eq!(fs::read_to_string(slot.join("init.luau")).unwrap(), "return 1");

        // Live edit is visible through the link.
        fs::write(pkg.join("src").join("init.luau"), "return 2").unwrap();
        assert_eq!(fs::read_to_string(slot.join("init.luau")).unwrap(), "return 2");

        // Re-applying is a no-op.
        let mode = apply_one(&slot, &link, &mut trash).unwrap();
        assert!(matches!(mode, AppliedMode::AlreadyLinked));

        // Removing the slot removes the link only, never the source.
        remove_slot(&slot, &mut trash).unwrap();
        assert!(!slot.exists());
        assert_eq!(fs::read_to_string(pkg.join("src").join("init.luau")).unwrap(), "return 2");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn junction_replaces_a_registry_installed_dir() {
        let base = fixture("replace");
        let mut trash = test_trash("replace");
        let pkg = base.join("pkg");
        fs::create_dir_all(pkg.join("src")).unwrap();
        fs::write(pkg.join("src").join("init.luau"), "return 'dev'").unwrap();
        let mount = base.join("Packages");
        let slot = mount.join("Knit");
        fs::create_dir_all(&slot).unwrap();
        fs::write(slot.join("init.luau"), "return 'registry'").unwrap();
        crate::receipts::write(&slot, &crate::receipts::Receipt {
            name: "acme/knit".into(),
            version: "1.0.0".into(),
            integrity: "aa".into(),
            root: "src/init.luau".into(),
            container: "Packages".into(),
        })
        .unwrap();

        apply_one(&slot, &active(&pkg, "src/init.luau"), &mut trash).unwrap();
        assert!(is_link_dir(&slot));
        assert_eq!(fs::read_to_string(slot.join("init.luau")).unwrap(), "return 'dev'");
        assert!(!slot.join(crate::receipts::RECEIPT_FILE).exists(), "registry receipt must not survive under the link");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn non_init_root_falls_back_to_a_renaming_copy() {
        let base = fixture("copy-rename");
        let mut trash = test_trash("copy-rename");
        let pkg = base.join("pkg");
        fs::create_dir_all(pkg.join("src")).unwrap();
        fs::write(pkg.join("src").join("Module.lua"), "return 'root'").unwrap();
        fs::write(pkg.join("src").join("Helper.lua"), "return 'helper'").unwrap();
        let mount = base.join("Packages");
        fs::create_dir_all(&mount).unwrap();
        let slot = mount.join("Knit");

        let mode = apply_one(&slot, &active(&pkg, "src/Module.lua"), &mut trash).unwrap();
        assert!(matches!(mode, AppliedMode::Copy(_)));
        assert!(!is_link_dir(&slot), "copy mode is a real directory");
        assert_eq!(fs::read_to_string(slot.join("init.lua")).unwrap(), "return 'root'");
        assert!(!slot.join("Module.lua").exists(), "root file is renamed, not duplicated");
        assert_eq!(fs::read_to_string(slot.join("Helper.lua")).unwrap(), "return 'helper'");

        // Edits are NOT live in copy mode until reapplied.
        fs::write(pkg.join("src").join("Module.lua"), "return 'edited'").unwrap();
        assert_eq!(fs::read_to_string(slot.join("init.lua")).unwrap(), "return 'root'");
        apply_one(&slot, &active(&pkg, "src/Module.lua"), &mut trash).unwrap();
        assert_eq!(fs::read_to_string(slot.join("init.lua")).unwrap(), "return 'edited'");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn copy_skips_git_and_only_renames_the_top_level_root() {
        let base = fixture("copy-git");
        let mut trash = test_trash("copy-git");
        let pkg = base.join("pkg");
        fs::create_dir_all(pkg.join(".git")).unwrap();
        fs::write(pkg.join(".git").join("HEAD"), "ref").unwrap();
        fs::create_dir_all(pkg.join("Nested")).unwrap();
        fs::write(pkg.join("Module.lua"), "return 'root'").unwrap();
        // Same file name nested must NOT be renamed.
        fs::write(pkg.join("Nested").join("Module.lua"), "return 'nested'").unwrap();
        let slot = base.join("Packages").join("Knit");
        fs::create_dir_all(slot.parent().unwrap()).unwrap();

        apply_one(&slot, &active(&pkg, "Module.lua"), &mut trash).unwrap();

        assert!(!slot.join(".git").exists());
        assert_eq!(fs::read_to_string(slot.join("init.lua")).unwrap(), "return 'root'");
        assert_eq!(fs::read_to_string(slot.join("Nested").join("Module.lua")).unwrap(), "return 'nested'");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn find_slots_for_target_spots_the_orphaned_link() {
        let base = fixture("orphan");
        let mut trash = test_trash("orphan");
        let pkg = base.join("pkg");
        fs::create_dir_all(pkg.join("src")).unwrap();
        fs::write(pkg.join("src").join("init.luau"), "return 1").unwrap();
        let mount = base.join("Packages");
        fs::create_dir_all(mount.join("Other")).unwrap();
        let slot = mount.join("Knit");
        apply_one(&slot, &active(&pkg, "src/init.luau"), &mut trash).unwrap();

        let found = find_slots_for_target(&mount.to_string_lossy(), &pkg.join("src"));
        assert_eq!(found, vec![slot.clone()]);
        assert!(find_slots_for_target(&mount.to_string_lossy(), &pkg).is_empty(), "project dir is not the link target; root parent is");
        let _ = fs::remove_dir_all(&base);
    }
}
