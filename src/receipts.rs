//! Install receipts: the ownership record every Forest-installed package
//! directory carries, plus the platform-agnostic FLAT tree taxonomy.
//! Platform-specific tree walking lives with each platform (the recursive
//! Roblox scan/reconcile is roblox/receipts.rs); this module never imports
//! platform code.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Receipt written INSIDE each installed package directory, right after its
/// archive extracts successfully. Nothing new appears in the project root:
/// the installed tree describes itself, the way the LICENSE files forest
/// already places in package dirs do. Extension-less, so Rojo ignores it
/// (exactly like LICENSE) and it never materializes in the DataModel.
///
/// A receipt lives and dies with its directory — deleting a package dir by
/// hand deletes its receipt, so the record can never claim more than what
/// is physically present. A dir without a receipt (half-extracted install,
/// user junk, pre-receipt tree) is simply never trusted.
pub const RECEIPT_FILE: &str = ".forest-receipt";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    pub name: String,
    pub version: String,
    /// Lockfile sha256 of the source tarball — the content identity that an
    /// extracted tree cannot otherwise prove about itself (extraction renames
    /// the root module and strips the archive prefix, so re-hashing the dir
    /// can never reproduce the tarball hash).
    pub integrity: String,
    /// archiveRoot — layout-affecting on extraction, so part of the match key.
    /// Empty for platforms with verbatim extraction (UEFN).
    pub root: String,
}

pub fn write(dir: &Path, receipt: &Receipt) -> Result<()> {
    let json = serde_json::to_string_pretty(receipt).context("Failed to serialize receipt")?;
    fs::write(dir.join(RECEIPT_FILE), json)
        .with_context(|| format!("Failed to write receipt in {}", dir.display()))
}

/// Corrupt JSON reads as `None` — an untrusted dir, never an error.
pub(crate) fn read_receipt(dir: &Path) -> Option<Receipt> {
    serde_json::from_str(&fs::read_to_string(dir.join(RECEIPT_FILE)).ok()?).ok()
}

// ---------------------------------------------------------------------------
// Flat-tree taxonomy (UEFN today; any future flat-layout platform tomorrow).
//
// The flat tree is exactly two levels under the mount (`<Mount>/<Scope>/<Name>`)
// and, unlike the Roblox tree, is SHARED with user-authored content: the
// in-situ authoring convention puts hand-written packages inside the mount.
// So the binary receipt-or-junk model is replaced by a 3-way taxonomy:
//   receipt          -> Forest-managed (update/remove per plan)
//   forest.json only -> AUTHORED: never deleted, never overwritten
//   neither          -> unknown: warned about, never touched
// Nothing in this section is platform-specific - the marker header is passed in.
// ---------------------------------------------------------------------------

/// A hand-authored package dir found inside the mount (forest.json, no receipt).
#[derive(Debug, Clone)]
pub struct AuthoredDir {
    /// Plan-format path: "./<Mount>/<Scope>/<Name>"
    pub path: String,
    pub scope_dir: String,
    pub name_dir: String,
}

#[derive(Debug, Default)]
pub struct FlatTreeScan {
    pub receipts: HashMap<String, Receipt>,
    pub authored: Vec<AuthoredDir>,
    /// Dirs that are neither installed nor authored - surfaced as warnings.
    pub unknown: Vec<String>,
    /// Generated marker files (first line == the supplied header), at the
    /// mount top level and inside each scope dir. Plan-format paths.
    pub marker_files: Vec<String>,
}

/// Scan the flat mount: exactly two directory levels, no recursion into
/// package contents. Dotfile entries are skipped; unlike the Roblox tree,
/// `_`-prefixed dirs are REAL here (mapped scopes can start with '_' for
/// digit-led or reserved-word slugs).
pub fn scan_flat(mount: &Path, mount_name: &str, marker_header: &str) -> FlatTreeScan {
    let mut tree = FlatTreeScan::default();
    let is_marker = |path: &Path| {
        fs::read_to_string(path)
            .map(|text| text.lines().next() == Some(marker_header))
            .unwrap_or(false)
    };

    let Ok(scopes) = fs::read_dir(mount) else { return tree };
    for scope_entry in scopes.flatten() {
        let scope_name = scope_entry.file_name().to_string_lossy().into_owned();
        if scope_name.starts_with('.') {
            continue;
        }
        let scope_path = scope_entry.path();
        if scope_path.is_file() {
            if scope_name.ends_with(".verse") && is_marker(&scope_path) {
                tree.marker_files.push(format!("./{}/{}", mount_name, scope_name));
            }
            continue;
        }
        let Ok(names) = fs::read_dir(&scope_path) else { continue };
        for name_entry in names.flatten() {
            let name = name_entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let name_path = name_entry.path();
            let plan_path = format!("./{}/{}/{}", mount_name, scope_name, name);
            if name_path.is_file() {
                if name.ends_with(".verse") && is_marker(&name_path) {
                    tree.marker_files.push(plan_path);
                }
                continue;
            }
            if let Some(receipt) = read_receipt(&name_path) {
                tree.receipts.insert(plan_path, receipt);
            } else if name_path.join("forest.json").is_file() {
                tree.authored.push(AuthoredDir {
                    path: plan_path,
                    scope_dir: scope_name.clone(),
                    name_dir: name,
                });
            } else {
                tree.unknown.push(plan_path);
            }
        }
    }
    tree.unknown.sort_unstable();
    tree.marker_files.sort_unstable();
    tree
}

/// What the flat reconcile needs to know about each planned package dir.
pub struct FlatPlannedDir<'a> {
    pub path: &'a str,
    pub integrity: &'a str,
    pub root: &'a str,
}

pub struct FlatReconciliation {
    /// Indices into the planned slice that need download + extract.
    pub to_install: Vec<usize>,
    pub kept: usize,
    /// Receipt-carrying dirs the plan no longer wants (safe to delete).
    pub stale_dirs: Vec<String>,
    /// Planned paths occupied by a NON-managed dir (authored/unknown) -
    /// never clobbered; skipped with a loud warning.
    pub blocked: Vec<String>,
}

pub fn reconcile_flat(planned: &[FlatPlannedDir], tree: &FlatTreeScan) -> FlatReconciliation {
    let occupied: HashSet<&str> = tree
        .authored
        .iter()
        .map(|a| a.path.as_str())
        .chain(tree.unknown.iter().map(String::as_str))
        .collect();

    let mut to_install = Vec::new();
    let mut blocked = Vec::new();
    let mut kept = 0;
    for (i, dir) in planned.iter().enumerate() {
        let receipt_ok = tree
            .receipts
            .get(dir.path)
            .map(|r| r.integrity == dir.integrity && r.root == dir.root)
            .unwrap_or(false);
        if receipt_ok {
            kept += 1;
        } else if occupied.contains(dir.path) {
            blocked.push(dir.path.to_string());
        } else {
            to_install.push(i);
        }
    }

    let desired: HashSet<&str> = planned.iter().map(|d| d.path).collect();
    let mut stale_dirs: Vec<String> = tree
        .receipts
        .keys()
        .filter(|path| !desired.contains(path.as_str()))
        .cloned()
        .collect();
    stale_dirs.sort_unstable();

    FlatReconciliation { to_install, kept, stale_dirs, blocked }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_HEADER: &str = "# Generated by Forest Package Manager. Do not edit.";

    fn flat_fixture(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("forest-flat-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn scan_flat_classifies_three_ways_and_collects_markers() {
        let mount = flat_fixture("scan");

        // Installed: receipt-carrying dir.
        let installed = mount.join("cool_studio").join("MathUtil");
        fs::create_dir_all(&installed).unwrap();
        write(&installed, &Receipt { name: "cool-studio/mathutil".into(), version: "1.0.0".into(), integrity: "aa".into(), root: String::new() }).unwrap();

        // Authored: forest.json, no receipt.
        let authored = mount.join("mine").join("MyPkg");
        fs::create_dir_all(&authored).unwrap();
        fs::write(authored.join("forest.json"), "{}").unwrap();

        // Unknown: neither.
        fs::create_dir_all(mount.join("mystery").join("Thing")).unwrap();

        // Markers at both levels + a user .verse that must NOT be collected.
        fs::write(mount.join("cool_studio.verse"), format!("{}\ncool_studio<public> := module {{}}\n", TEST_HEADER)).unwrap();
        fs::write(mount.join("cool_studio").join("MathUtil.verse"), format!("{}\nMathUtil<public> := module {{}}\n", TEST_HEADER)).unwrap();
        fs::write(mount.join("mine").join("NotAMarker.verse"), "# my own file\n").unwrap();

        // `_`-prefixed scope dirs are real (digit-led/reserved mappings).
        let underscore = mount.join("_123_team").join("Pkg");
        fs::create_dir_all(&underscore).unwrap();
        write(&underscore, &Receipt { name: "123-team/pkg".into(), version: "1.0.0".into(), integrity: "bb".into(), root: String::new() }).unwrap();

        let tree = scan_flat(&mount, "ForestPackages", TEST_HEADER);

        assert!(tree.receipts.contains_key("./ForestPackages/cool_studio/MathUtil"));
        assert!(tree.receipts.contains_key("./ForestPackages/_123_team/Pkg"), "underscore scopes are not exempt");
        assert_eq!(tree.authored.len(), 1);
        assert_eq!(tree.authored[0].path, "./ForestPackages/mine/MyPkg");
        assert_eq!(tree.authored[0].scope_dir, "mine");
        assert_eq!(tree.authored[0].name_dir, "MyPkg");
        assert_eq!(tree.unknown, vec!["./ForestPackages/mystery/Thing".to_string()]);
        assert_eq!(tree.marker_files, vec![
            "./ForestPackages/cool_studio.verse".to_string(),
            "./ForestPackages/cool_studio/MathUtil.verse".to_string(),
        ]);

        let _ = fs::remove_dir_all(&mount);
    }

    #[test]
    fn reconcile_flat_keeps_blocks_installs_and_stales() {
        let tree = FlatTreeScan {
            receipts: [
                ("./M/a/Kept".to_string(), Receipt { name: "a/kept".into(), version: "1".into(), integrity: "ok".into(), root: String::new() }),
                ("./M/a/Changed".to_string(), Receipt { name: "a/changed".into(), version: "1".into(), integrity: "OLD".into(), root: String::new() }),
                ("./M/a/Gone".to_string(), Receipt { name: "a/gone".into(), version: "1".into(), integrity: "xx".into(), root: String::new() }),
            ].into(),
            authored: vec![AuthoredDir { path: "./M/mine/Authored".into(), scope_dir: "mine".into(), name_dir: "Authored".into() }],
            unknown: vec!["./M/junk/Thing".into()],
            marker_files: vec![],
        };
        let planned = [
            FlatPlannedDir { path: "./M/a/Kept", integrity: "ok", root: "" },
            FlatPlannedDir { path: "./M/a/Changed", integrity: "NEW", root: "" },
            FlatPlannedDir { path: "./M/b/Fresh", integrity: "ff", root: "" },
            FlatPlannedDir { path: "./M/mine/Authored", integrity: "zz", root: "" },
            FlatPlannedDir { path: "./M/junk/Thing", integrity: "qq", root: "" },
        ];
        let rec = reconcile_flat(&planned, &tree);
        assert_eq!(rec.kept, 1);
        assert_eq!(rec.to_install, vec![1, 2], "changed integrity + fresh install");
        assert_eq!(rec.blocked, vec!["./M/mine/Authored".to_string(), "./M/junk/Thing".to_string()]);
        assert_eq!(rec.stale_dirs, vec!["./M/a/Gone".to_string()], "authored/unknown never go stale");
    }

    #[test]
    fn reconcile_flat_authored_not_in_plan_is_untouched() {
        let tree = FlatTreeScan {
            receipts: HashMap::new(),
            authored: vec![AuthoredDir { path: "./M/mine/Solo".into(), scope_dir: "mine".into(), name_dir: "Solo".into() }],
            unknown: vec![],
            marker_files: vec![],
        };
        let rec = reconcile_flat(&[], &tree);
        assert!(rec.to_install.is_empty());
        assert!(rec.stale_dirs.is_empty());
        assert!(rec.blocked.is_empty());
    }

    #[test]
    fn receipt_round_trips_and_rejects_garbage() {
        let dir = std::env::temp_dir().join(format!("forest-receipt-rt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let receipt = Receipt { name: "a/b".into(), version: "1.2.3".into(), integrity: "cc".into(), root: "init.lua".into() };
        write(&dir, &receipt).unwrap();
        assert_eq!(read_receipt(&dir), Some(receipt));

        fs::write(dir.join(RECEIPT_FILE), "{not json").unwrap();
        assert_eq!(read_receipt(&dir), None, "corrupt receipt must read as absent");

        let _ = fs::remove_dir_all(&dir);
    }
}
