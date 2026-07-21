use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::install_plan::{InstallPlan, POINTER_HEADER};
use crate::lockfile_gen::PACKAGES_DIR;

/// Receipt written INSIDE each installed package directory, right after its
/// archive extracts successfully. Nothing new appears in the project root:
/// the `Packages/` tree describes itself, the way the LICENSE files forest
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
    pub root: String,
}

pub fn write(dir: &Path, receipt: &Receipt) -> Result<()> {
    let json = serde_json::to_string_pretty(receipt).context("Failed to serialize receipt")?;
    fs::write(dir.join(RECEIPT_FILE), json)
        .with_context(|| format!("Failed to write receipt in {}", dir.display()))
}

fn read(dir: &Path) -> Option<Receipt> {
    serde_json::from_str(&fs::read_to_string(dir.join(RECEIPT_FILE)).ok()?).ok()
}

/// Everything forest-managed found on disk: receipts keyed by plan-format
/// path (`./Packages/...`, forward slashes) plus recognized pointer dirs.
#[derive(Debug, Default)]
pub struct TreeScan {
    pub receipts: HashMap<String, Receipt>,
    pub pointer_dirs: Vec<String>,
}

/// Walk every package position under `packages_dir` (`Packages/*`, then each
/// `*/Packages/*`, recursively), collecting receipts and pointer signatures.
/// Keys are rendered in plan format regardless of where `packages_dir`
/// physically is, so reconcile can compare strings directly. `_`/`.` entries
/// are skipped, matching the install-cleanup exemption.
pub fn scan(packages_dir: &Path) -> TreeScan {
    let mut tree = TreeScan::default();
    walk(packages_dir, &format!("./{}", PACKAGES_DIR), &mut tree);
    tree
}

fn walk(container: &Path, container_str: &str, tree: &mut TreeScan) {
    let Ok(entries) = fs::read_dir(container) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('_') || name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let path_str = format!("{container_str}/{name}");
        if let Some(receipt) = read(&path) {
            tree.receipts.insert(path_str.clone(), receipt);
        } else if is_pointer_dir(&path) {
            tree.pointer_dirs.push(path_str.clone());
        }
        let nested = path.join(PACKAGES_DIR);
        if nested.is_dir() {
            walk(&nested, &format!("{path_str}/{PACKAGES_DIR}"), tree);
        }
    }
}

/// A pointer dir is recognized by the generated header in its init.lua. A
/// package that impersonates one could at worst get itself deleted and
/// reinstalled on the next run — never kept wrongly.
fn is_pointer_dir(dir: &Path) -> bool {
    fs::read_to_string(dir.join("init.lua"))
        .map(|s| s.starts_with(POINTER_HEADER))
        .unwrap_or(false)
}

/// What an install run must actually do, given a plan and the scanned tree.
pub struct Reconciliation {
    /// Indices into `plan.packages` that need download + extract.
    pub to_install: Vec<usize>,
    /// Number of planned packages skipped because they're already on disk.
    pub kept: usize,
    /// Forest-managed dirs on disk that the plan no longer wants.
    pub stale_dirs: Vec<String>,
}

/// Diff the plan against what the tree says about itself.
///
/// A planned package is KEPT (skipped entirely) only when:
///   1. its dir carries a receipt with the same (integrity, root) — receipt
///      presence implies the dir existed at scan time, and
///   2. every planned ancestor package is also kept — a nested package
///      physically lives INSIDE its parent's directory, so a re-extracted
///      parent wipes the child no matter what the child's receipt says.
pub fn reconcile(plan: &InstallPlan, tree: &TreeScan) -> Reconciliation {
    // Parents first, so each package's ancestors are classified before it.
    let mut order: Vec<usize> = (0..plan.packages.len()).collect();
    order.sort_by_key(|&i| plan.packages[i].path.len());

    let mut kept_paths: HashSet<&str> = HashSet::new();
    let mut to_install: Vec<usize> = Vec::new();
    for i in order {
        let pkg = &plan.packages[i];
        let receipt_ok = tree
            .receipts
            .get(pkg.path.as_str())
            .map(|r| r.integrity == pkg.integrity && r.root == pkg.root)
            .unwrap_or(false);
        let ancestors_ok = plan.packages.iter().all(|other| {
            !pkg.path.starts_with(&format!("{}/", other.path))
                || kept_paths.contains(other.path.as_str())
        });
        if receipt_ok && ancestors_ok {
            kept_paths.insert(pkg.path.as_str());
        } else {
            to_install.push(i);
        }
    }
    to_install.sort_unstable();

    let desired_pkg_paths: HashSet<&str> = plan.packages.iter().map(|p| p.path.as_str()).collect();
    let desired_ptr_dirs: HashSet<&str> = plan.pointers.iter().map(|p| p.dir.as_str()).collect();

    // Anything forest-managed on disk that the plan no longer wants. A dir
    // that switched roles (package↔pointer) is NOT stale: the installer
    // rewrites it in place (to_install clears its target first; pointer
    // init.lua is regenerated over the dir).
    let mut stale_dirs: Vec<String> = tree
        .receipts
        .keys()
        .map(String::as_str)
        .chain(tree.pointer_dirs.iter().map(String::as_str))
        .filter(|d| !desired_pkg_paths.contains(d) && !desired_ptr_dirs.contains(d))
        .map(str::to_string)
        .collect();
    stale_dirs.sort_unstable();
    stale_dirs.dedup();

    Reconciliation {
        to_install,
        kept: kept_paths.len(),
        stale_dirs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::install_plan::{PlannedPackage, PlannedPointer};

    fn pkg(path: &str, integrity: &str) -> PlannedPackage {
        PlannedPackage {
            path: path.to_string(),
            name: format!("acme/{}", path.rsplit('/').next().unwrap().to_lowercase()),
            version: "1.0.0".to_string(),
            integrity: integrity.to_string(),
            root: "src/init.luau".to_string(),
            public: true,
        }
    }

    fn plan_of(packages: Vec<PlannedPackage>, pointer_dirs: &[&str]) -> InstallPlan {
        InstallPlan {
            packages,
            pointers: pointer_dirs
                .iter()
                .map(|d| PlannedPointer { dir: d.to_string(), init_lua: "return".to_string() })
                .collect(),
        }
    }

    /// TreeScan as if `plan` had been fully installed.
    fn tree_of(plan: &InstallPlan) -> TreeScan {
        TreeScan {
            receipts: plan
                .packages
                .iter()
                .map(|p| {
                    (p.path.clone(), Receipt {
                        name: p.name.clone(),
                        version: p.version.clone(),
                        integrity: p.integrity.clone(),
                        root: p.root.clone(),
                    })
                })
                .collect(),
            pointer_dirs: plan.pointers.iter().map(|p| p.dir.clone()).collect(),
        }
    }

    #[test]
    fn empty_tree_installs_everything() {
        let plan = plan_of(vec![pkg("./Packages/A", "aa"), pkg("./Packages/B", "bb")], &[]);
        let rec = reconcile(&plan, &TreeScan::default());
        assert_eq!(rec.to_install, vec![0, 1]);
        assert_eq!(rec.kept, 0);
        assert!(rec.stale_dirs.is_empty());
    }

    #[test]
    fn matching_tree_is_a_full_noop() {
        let plan = plan_of(
            vec![pkg("./Packages/A", "aa"), pkg("./Packages/A/Packages/B", "bb")],
            &["./Packages/A/Packages/C"],
        );
        let rec = reconcile(&plan, &tree_of(&plan));
        assert!(rec.to_install.is_empty());
        assert_eq!(rec.kept, 2);
        assert!(rec.stale_dirs.is_empty());
    }

    #[test]
    fn integrity_change_reinstalls_just_that_package() {
        let old = plan_of(vec![pkg("./Packages/A", "aa"), pkg("./Packages/B", "bb")], &[]);
        let new = plan_of(vec![pkg("./Packages/A", "aa"), pkg("./Packages/B", "bb-NEW")], &[]);
        let rec = reconcile(&new, &tree_of(&old));
        assert_eq!(rec.to_install, vec![1]);
        assert_eq!(rec.kept, 1);
        assert!(rec.stale_dirs.is_empty());
    }

    #[test]
    fn alias_rename_is_stale_old_path_plus_fresh_install() {
        let old = plan_of(vec![pkg("./Packages/knit", "aa")], &[]);
        let new = plan_of(vec![pkg("./Packages/Knit", "aa")], &[]);
        let rec = reconcile(&new, &tree_of(&old));
        assert_eq!(rec.to_install, vec![0], "case-only rename must reinstall");
        assert_eq!(rec.stale_dirs, vec!["./Packages/knit".to_string()]);
    }

    #[test]
    fn removed_package_and_pointer_go_stale() {
        let old = plan_of(
            vec![pkg("./Packages/A", "aa"), pkg("./Packages/B", "bb")],
            &["./Packages/A/Packages/B"],
        );
        let new = plan_of(vec![pkg("./Packages/A", "aa")], &[]);
        let rec = reconcile(&new, &tree_of(&old));
        assert!(rec.to_install.is_empty());
        assert_eq!(
            rec.stale_dirs,
            vec!["./Packages/A/Packages/B".to_string(), "./Packages/B".to_string()]
        );
    }

    #[test]
    fn child_of_reinstalled_parent_cannot_be_kept() {
        // Parent A changes integrity; nested child B is untouched in the
        // lockfile but physically lives inside A — it must reinstall too.
        let old = plan_of(
            vec![pkg("./Packages/A", "aa"), pkg("./Packages/A/Packages/B", "bb")],
            &[],
        );
        let new = plan_of(
            vec![pkg("./Packages/A", "aa-NEW"), pkg("./Packages/A/Packages/B", "bb")],
            &[],
        );
        let rec = reconcile(&new, &tree_of(&old));
        assert_eq!(rec.to_install, vec![0, 1]);
        assert_eq!(rec.kept, 0);
    }

    #[test]
    fn scan_reads_receipts_pointers_and_ignores_junk() {
        let base = std::env::temp_dir().join(format!("forest-receipts-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let packages = base.join("Packages");

        // Real package with a receipt, nesting a child package + a pointer.
        let knit = packages.join("Knit");
        fs::create_dir_all(knit.join("Packages").join("Comm")).unwrap();
        fs::create_dir_all(knit.join("Packages").join("Promise")).unwrap();
        let receipt = Receipt {
            name: "acme/knit".into(),
            version: "1.0.0".into(),
            integrity: "aa".into(),
            root: "src/init.luau".into(),
        };
        write(&knit, &receipt).unwrap();
        write(&knit.join("Packages").join("Comm"), &Receipt { name: "acme/comm".into(), version: "1.0.0".into(), integrity: "bb".into(), root: "init.luau".into() }).unwrap();
        fs::write(
            knit.join("Packages").join("Promise").join("init.lua"),
            format!("{POINTER_HEADER}\nreturn require(script.Parent)"),
        )
        .unwrap();

        // Junk: no receipt, no signature; exempt _Index; a plain file.
        fs::create_dir_all(packages.join("random-junk")).unwrap();
        fs::create_dir_all(packages.join("_Index")).unwrap();
        fs::write(packages.join("stray.txt"), "x").unwrap();

        let tree = scan(&packages);

        assert_eq!(tree.receipts.get("./Packages/Knit"), Some(&receipt));
        assert!(tree.receipts.contains_key("./Packages/Knit/Packages/Comm"));
        assert_eq!(tree.receipts.len(), 2, "junk and _Index must not be receipts");
        assert_eq!(tree.pointer_dirs, vec!["./Packages/Knit/Packages/Promise".to_string()]);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn receipt_round_trips_and_rejects_garbage() {
        let dir = std::env::temp_dir().join(format!("forest-receipt-rt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let receipt = Receipt { name: "a/b".into(), version: "1.2.3".into(), integrity: "cc".into(), root: "init.lua".into() };
        write(&dir, &receipt).unwrap();
        assert_eq!(read(&dir), Some(receipt));

        fs::write(dir.join(RECEIPT_FILE), "{not json").unwrap();
        assert_eq!(read(&dir), None, "corrupt receipt must read as absent");

        let _ = fs::remove_dir_all(&dir);
    }
}
