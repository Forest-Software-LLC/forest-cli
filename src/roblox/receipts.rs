//! Roblox tree bookkeeping: the recursive `Packages/*` (and nested
//! `*/Packages/*`) scan and the keep/stale reconcile against an install
//! plan. Receipt read/write itself is shared core (src/receipts.rs).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::receipts::{read_receipt, Receipt};
use crate::roblox::plan::{InstallPlan, POINTER_HEADER};
use crate::roblox::PACKAGES_DIR;

/// Everything forest-managed found on disk: receipts keyed by plan-format
/// path (`./Packages/...`, forward slashes) plus recognized pointer dirs.
#[derive(Debug, Default)]
pub struct TreeScan {
    pub receipts: HashMap<String, Receipt>,
    pub pointer_dirs: Vec<String>,
}

/// Walk every package position under `packages_dir` (`<container>/*`, then
/// each package's own nested container, recursively), collecting receipts
/// and pointer signatures. `consumer_container` must match the planner's
/// root prefix so keys are rendered in plan format regardless of where
/// `packages_dir` physically is and reconcile can compare strings directly.
/// `_`/`.` entries are skipped, matching the install-cleanup exemption.
pub fn scan(packages_dir: &Path, consumer_container: &str) -> TreeScan {
    let mut tree = TreeScan::default();
    walk(packages_dir, &format!("./{}", consumer_container), &mut tree);
    tree
}

fn walk(container: &Path, container_str: &str, tree: &mut TreeScan) {
    let Ok(entries) = fs::read_dir(container) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('_') || name.starts_with('.') {
            continue;
        }
        // Symlinked dirs (a `forest link` slot, or anything a user linked
        // in) are not forest's to manage: descending would record the LINKED
        // tree's receipts as this mount's, and the stale deletes computed
        // from them would reach through the link into real source files.
        if entry.file_type().map(|t| t.is_symlink()).unwrap_or(true) {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let path_str = format!("{container_str}/{name}");
        // Each receipt names its own nested container to descend into; dirs
        // without a trusted receipt use the default. Only a validated name
        // is followed, since receipts are just files on disk and must not
        // steer the scan (or the stale deletes computed from it) outside
        // the mount.
        let mut nested_name = PACKAGES_DIR.to_string();
        // Pointer check first: older builds wrote pointer init.lua into a
        // package dir without clearing it, so a dir can carry both a stale
        // receipt and a pointer header. Trusting that receipt would keep the
        // dir as if the real package were still inside it.
        if is_pointer_dir(&path) {
            tree.pointer_dirs.push(path_str.clone());
        } else if let Some(receipt) = read_receipt(&path) {
            if crate::roblox::validate_packages_dir(&receipt.container).is_ok() {
                nested_name = receipt.container.clone();
            }
            tree.receipts.insert(path_str.clone(), receipt);
        }
        let nested = path.join(&nested_name);
        if nested.is_dir() {
            walk(&nested, &format!("{path_str}/{nested_name}"), tree);
        }
    }
}

/// A pointer dir is recognized by the generated header in its init.lua. A
/// package that impersonates one could at worst get itself deleted and
/// reinstalled on the next run; never kept wrongly.
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
///   1. its dir carries a receipt with the same (integrity, root,
///      container); receipt presence implies the dir existed at scan time,
///      and
///   2. every planned ancestor package is also kept; a nested package
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
            .map(|r| r.integrity == pkg.integrity && r.root == pkg.root && r.container == pkg.packages_dir)
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
    // that switched roles (package to pointer or back) is NOT stale: the
    // installer handles it (to_install clears its target first; the pointer
    // writer wipes a dir that still carries a receipt).
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
    use crate::receipts::write;
    use crate::roblox::plan::{PlannedPackage, PlannedPointer};

    fn pkg(path: &str, integrity: &str) -> PlannedPackage {
        PlannedPackage {
            path: path.to_string(),
            name: format!("acme/{}", path.rsplit('/').next().unwrap().to_lowercase()),
            version: "1.0.0".to_string(),
            integrity: integrity.to_string(),
            root: "src/init.luau".to_string(),
            packages_dir: "Packages".to_string(),
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
                        container: p.packages_dir.clone(),
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
        // lockfile but physically lives inside A; it must reinstall too.
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
    fn scan_never_trusts_a_receipt_next_to_a_pointer_header() {
        // Older builds wrote pointer init.lua into a still populated package
        // dir. If the leftover receipt were trusted, a later install would
        // keep the dir even though its require target is gone.
        let base = std::env::temp_dir().join(format!("forest-receipts-mixed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let packages = base.join("Packages");
        let broken = packages.join("Knit").join("Packages").join("X");
        fs::create_dir_all(&broken).unwrap();
        write(&broken, &Receipt { name: "acme/x".into(), version: "1.0.0".into(), integrity: "xx".into(), root: "src/init.luau".into(), container: "Packages".into() }).unwrap();
        fs::write(broken.join("init.luau"), "return {}").unwrap();
        fs::write(
            broken.join("init.lua"),
            format!("{POINTER_HEADER}\nreturn require(script.Parent.Parent.Parent['X'])"),
        )
        .unwrap();

        let tree = scan(&packages, "Packages");

        assert!(tree.receipts.is_empty(), "stale receipt beside a pointer must not be trusted");
        assert_eq!(tree.pointer_dirs, vec!["./Packages/Knit/Packages/X".to_string()]);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn pointer_turning_back_into_a_package_reinstalls_it() {
        // forest i scope/x hoisted X to the root and left a pointer at the
        // nested spot; forest remove scope/x nests it again. The pointer dir
        // has no receipt so X must reinstall there, and the root copy goes
        // stale.
        let old = plan_of(
            vec![pkg("./Packages/Knit", "aa"), pkg("./Packages/X", "xx")],
            &["./Packages/Knit/Packages/X"],
        );
        let new = plan_of(
            vec![pkg("./Packages/Knit", "aa"), pkg("./Packages/Knit/Packages/X", "xx")],
            &[],
        );
        let rec = reconcile(&new, &tree_of(&old));
        assert_eq!(rec.to_install, vec![1], "nested X installs fresh over the pointer dir");
        assert_eq!(rec.kept, 1, "Knit is untouched");
        assert_eq!(rec.stale_dirs, vec!["./Packages/X".to_string()]);
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
            container: "Packages".into(),
        };
        write(&knit, &receipt).unwrap();
        write(&knit.join("Packages").join("Comm"), &Receipt { name: "acme/comm".into(), version: "1.0.0".into(), integrity: "bb".into(), root: "init.luau".into(), container: "Packages".into() }).unwrap();
        fs::write(
            knit.join("Packages").join("Promise").join("init.lua"),
            format!("{POINTER_HEADER}\nreturn require(script.Parent)"),
        )
        .unwrap();

        // Junk: no receipt, no signature; exempt _Index; a plain file.
        fs::create_dir_all(packages.join("random-junk")).unwrap();
        fs::create_dir_all(packages.join("_Index")).unwrap();
        fs::write(packages.join("stray.txt"), "x").unwrap();

        let tree = scan(&packages, "Packages");

        assert_eq!(tree.receipts.get("./Packages/Knit"), Some(&receipt));
        assert!(tree.receipts.contains_key("./Packages/Knit/Packages/Comm"));
        assert_eq!(tree.receipts.len(), 2, "junk and _Index must not be receipts");
        assert_eq!(tree.pointer_dirs, vec!["./Packages/Knit/Packages/Promise".to_string()]);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn scan_descends_a_receipts_own_renamed_container() {
        // Knit was published with packagesDir "knit_deps": the scan must
        // find nested Comm inside it under a key matching the planner's
        // per-hop path. The consumer's own mount is renamed too.
        let base = std::env::temp_dir().join(format!("forest-receipts-renamed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let mount = base.join("roblox_packages");

        let knit = mount.join("Knit");
        fs::create_dir_all(knit.join("knit_deps").join("Comm")).unwrap();
        write(&knit, &Receipt {
            name: "acme/knit".into(),
            version: "1.0.0".into(),
            integrity: "aa".into(),
            root: "src/init.luau".into(),
            container: "knit_deps".into(),
        }).unwrap();
        write(&knit.join("knit_deps").join("Comm"), &Receipt {
            name: "acme/comm".into(),
            version: "1.0.0".into(),
            integrity: "bb".into(),
            root: "init.luau".into(),
            container: "Packages".into(),
        }).unwrap();
        // A stray default-named subdir must not be scanned as Knit's
        // container once the receipt says otherwise.
        fs::create_dir_all(knit.join("Packages").join("Ghost")).unwrap();
        write(&knit.join("Packages").join("Ghost"), &Receipt {
            name: "acme/ghost".into(),
            version: "1.0.0".into(),
            integrity: "gg".into(),
            root: "init.luau".into(),
            container: "Packages".into(),
        }).unwrap();

        let tree = scan(&mount, "roblox_packages");

        assert!(tree.receipts.contains_key("./roblox_packages/Knit"));
        assert!(
            tree.receipts.contains_key("./roblox_packages/Knit/knit_deps/Comm"),
            "nested scan must follow the receipt's container: {:?}",
            tree.receipts.keys().collect::<Vec<_>>()
        );
        assert_eq!(tree.receipts.len(), 2, "the stray default-named subdir is not descended");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn scan_never_follows_a_traversal_shaped_container() {
        // A receipt is just a file on disk; a poisoned container name must
        // not steer the scan (and the stale deletes computed from its keys)
        // outside the mount.
        let base = std::env::temp_dir().join(format!("forest-receipts-poison-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let mount = base.join("Packages");
        let evil = mount.join("Evil");
        fs::create_dir_all(&evil).unwrap();
        write(&evil, &Receipt {
            name: "acme/evil".into(),
            version: "1.0.0".into(),
            integrity: "ee".into(),
            root: "init.luau".into(),
            container: "..".into(),
        }).unwrap();

        let tree = scan(&mount, "Packages");

        assert!(tree.receipts.contains_key("./Packages/Evil"));
        assert!(
            tree.receipts.keys().all(|k| !k.contains("..")),
            "no scan key may carry a traversal segment: {:?}",
            tree.receipts.keys().collect::<Vec<_>>()
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn scan_never_descends_a_linked_slot() {
        // A `forest link` slot is a junction/symlink into the developer's
        // real working tree. Recording ITS receipts as this mount's would
        // let reconcile compute stale deletes that reach through the link.
        let base = std::env::temp_dir().join(format!("forest-receipts-linked-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let dev_tree = base.join("dev").join("src");
        let dev_dep = dev_tree.join("Packages").join("Comm");
        fs::create_dir_all(&dev_dep).unwrap();
        write(&dev_dep, &Receipt { name: "acme/comm".into(), version: "1.0.0".into(), integrity: "cc".into(), root: "init.luau".into(), container: "Packages".into() }).unwrap();

        let mount = base.join("Packages");
        fs::create_dir_all(&mount).unwrap();
        #[cfg(windows)]
        junction::create(&dev_tree, mount.join("Knit")).unwrap();
        #[cfg(not(windows))]
        std::os::unix::fs::symlink(&dev_tree, mount.join("Knit")).unwrap();

        let tree = scan(&mount, "Packages");

        assert!(tree.receipts.is_empty(), "nothing behind the link may be recorded: {:?}", tree.receipts.keys().collect::<Vec<_>>());
        assert!(tree.pointer_dirs.is_empty());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn container_rename_forces_a_clean_reinstall() {
        // Same integrity/root, but the package was republished with a new
        // packagesDir: keeping the dir would orphan the old nested tree.
        let old = plan_of(vec![pkg("./Packages/A", "aa")], &[]);
        let mut new = plan_of(vec![pkg("./Packages/A", "aa")], &[]);
        new.packages[0].packages_dir = "a_deps".to_string();
        let rec = reconcile(&new, &tree_of(&old));
        assert_eq!(rec.to_install, vec![0], "container is part of the keep-key");
        assert_eq!(rec.kept, 0);
    }
}
