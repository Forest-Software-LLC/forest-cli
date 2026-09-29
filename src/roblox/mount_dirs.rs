//! Whole-mount folder operations for `forest mount`. Rojo-safe like
//! install: removing or moving a mount is one rename of its folder (into the
//! trash bin, or to the new path), never a stream of deletions.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::receipts::RECEIPT_FILE;
use crate::roblox::link_overlay::{is_link_dir, remove_link_path};
use crate::roblox::scratch::{rename_with_retry, scratch_dirs, TrashBin};

/// Delete a mount folder and everything in it. `forest link` slots are
/// removed as links first, so nothing is ever deleted through one.
pub fn remove_mount_dir(path: &str) -> Result<()> {
    let dir = Path::new(path);
    if !dir.is_dir() {
        return Ok(());
    }
    remove_link_slots(path)?;
    let mut trash = TrashBin::new(scratch_dirs().trash);
    trash.remove_dir_all(dir).with_context(|| format!("Failed to remove {}", path))
}

/// Remove the `forest link` slots at a folder's top level, as links. They
/// point into a developer's working tree, and some tools delete through a
/// junction (PowerShell 5.1's Remove-Item does). Returns how many went.
pub fn remove_link_slots(path: &str) -> Result<usize> {
    let Ok(entries) = fs::read_dir(path) else {
        return Ok(0);
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        if is_link_dir(&entry.path()) {
            remove_link_path(&entry.path())
                .with_context(|| format!("Failed to remove link at {}", entry.path().display()))?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Move a mount folder, creating the new parent folders. A folder that was
/// never installed has nothing to move.
pub fn move_mount_dir(from: &str, to: &str) -> Result<()> {
    let (from, to) = (Path::new(from), Path::new(to));
    if !from.is_dir() {
        return Ok(());
    }
    if let Some(parent) = to.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    rename_with_retry(from, to).with_context(|| format!("Failed to move {} to {}", from.display(), to.display()))
}

/// What a mount at `dir` would delete that neither forest nor Wally put
/// there: everything except installed packages (receipts), pointer dirs,
/// link slots, Wally's `_Index` and link modules, and dot entries. Install
/// prunes a mount's top level, so `forest mount create` refuses a folder
/// where this finds anything.
pub fn foreign_entries(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut foreign = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name == "_Index" {
            continue;
        }
        let path = entry.path();
        let managed = match entry.file_type() {
            Ok(t) if t.is_symlink() => true,
            Ok(t) if t.is_dir() => {
                path.join(RECEIPT_FILE).is_file() || crate::roblox::receipts::is_pointer_dir(&path)
            }
            Ok(t) if t.is_file() => is_wally_link_module(&path),
            _ => false,
        };
        if !managed {
            foreign.push(name);
        }
    }
    foreign.sort();
    foreign
}

/// Wally puts one module per dependency next to `_Index`, requiring into it.
fn is_wally_link_module(path: &Path) -> bool {
    let lua = path
        .extension()
        .map_or(false, |e| e.eq_ignore_ascii_case("lua") || e.eq_ignore_ascii_case("luau"));
    lua && fs::read_to_string(path).map_or(false, |text| text.contains("_Index"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("forest-mount-dirs-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn dependency_folders_have_nothing_foreign() {
        let base = fixture("deps");
        let pkg = base.join("Knit");
        fs::create_dir_all(&pkg).unwrap();
        crate::receipts::write(&pkg, &crate::receipts::Receipt {
            name: "acme/knit".into(),
            version: "1.0.0".into(),
            integrity: "aa".into(),
            root: "init.luau".into(),
            container: "Packages".into(),
        })
        .unwrap();
        fs::create_dir_all(base.join("_Index").join("evaera_promise@4.0.0")).unwrap();
        fs::write(base.join("Promise.lua"), "return require(script.Parent._Index[\"evaera_promise@4.0.0\"][\"promise\"])").unwrap();
        fs::write(base.join(".gitkeep"), "").unwrap();

        assert!(foreign_entries(&base).is_empty());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn source_folders_are_reported() {
        let base = fixture("source");
        fs::create_dir_all(base.join("Services")).unwrap();
        fs::write(base.join("Services").join("init.luau"), "return {}").unwrap();
        fs::write(base.join("Main.server.luau"), "print('hi')").unwrap();

        assert_eq!(foreign_entries(&base), vec!["Main.server.luau".to_string(), "Services".to_string()]);
        assert!(foreign_entries(&base.join("missing")).is_empty(), "a new folder has nothing to lose");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn mount_folders_move_and_remove_whole() {
        let base = fixture("move");
        let from = base.join("Dev");
        fs::create_dir_all(from.join("Pkg")).unwrap();
        fs::write(from.join("Pkg").join("init.luau"), "return 1").unwrap();

        let to = base.join("tools").join("DevPackages");
        move_mount_dir(&from.to_string_lossy(), &to.to_string_lossy()).unwrap();
        assert!(!from.exists());
        assert_eq!(fs::read_to_string(to.join("Pkg").join("init.luau")).unwrap(), "return 1");

        remove_mount_dir(&to.to_string_lossy()).unwrap();
        assert!(!to.exists());
        assert!(base.join("tools").is_dir(), "only the mount itself goes");
        remove_mount_dir(&to.to_string_lossy()).unwrap();
        let _ = fs::remove_dir_all(&base);
    }
}
