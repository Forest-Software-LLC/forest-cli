//! Roblox install executor: the hoisted `Packages/` tree with pointer
//! `init.lua` shims. Moved verbatim from lockfile_gen.rs when the platform
//! seam was introduced; reached only via `Platform::install`. Downloads go
//! through the shared pool (src/download_pool.rs).

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{anyhow, Context, Result};

use crate::download_pool::DownloadJob;
use crate::lockfile_gen::{InstallSummary, LockFile};
use crate::lockfile_solver::DepSpec;
use crate::receipts;
use crate::roblox::extract::fetch_and_extract;
use crate::roblox::plan::plan_install;
use crate::roblox::PACKAGES_DIR;

/// What the installer needs per job beyond the pool's shared fields.
struct RobloxExtra {
    /// archiveRoot: picks the extraction layout, recorded in the receipt.
    root: String,
    /// The package's own nested container name, recorded in its receipt.
    container: String,
}

pub async fn make_directories_roblox(
    lockfile: &LockFile,
    root_deps: HashMap<String, DepSpec>,
    manifest: &serde_json::Value,
    force: bool,
) -> Result<InstallSummary> {
    // `_`/`.`-prefixed folders in packages/ are exempt from install cleanup
    // (e.g. Wally's `_Index`), so aliases must not claim those names.
    for (pkg_name, spec) in &root_deps {
        if spec.alias.starts_with('_') || spec.alias.starts_with('.') {
            return Err(anyhow!(
                "Alias '{}' for {} cannot start with '_' or '.'; rename it in forest.json",
                spec.alias, pkg_name
            ));
        }
    }

    // All path/pointer computation is pure and lives in roblox/plan.rs; plan
    // paths stay in the virtual `./<container>/...` format and are mapped
    // onto the physical mount (derived from the manifest's `root` and
    // `packagesDir`) only here. One manifest read feeds the planner's root
    // prefix, physical_path, the receipt scan, and prune_top_level, so their
    // prefixes can never mismatch.
    let container = crate::roblox::packages_container(manifest);
    let plan = plan_install(lockfile, &root_deps, &container)?;
    let base = crate::roblox::packages_base(manifest);

    // A renamed `packagesDir` or a root move within the same parent leaves
    // the old tree behind. Receipts prove which leftovers are forest's and
    // those get a direct warning; a receipt-less literal `Packages/`
    // (pre-receipt forest, or Wally's) keeps the softer legacy warning.
    // Un-nesting the root moves the old mount out of the scanned parents,
    // so that case stays silent.
    let abandoned = find_abandoned_mounts(Path::new("."), &base);
    if !abandoned.is_empty() {
        let list = abandoned
            .iter()
            .map(|d| format!("{}/", d))
            .collect::<Vec<_>>()
            .join(", ");
        crate::message::warn(&format!(
            "Found old dependency folder(s) no longer managed by forest: {}. Dependencies now install to {}/. The old folder(s) can be deleted.",
            list, base
        ));
    }
    let legacy_already_flagged = abandoned.iter().any(|d| d.eq_ignore_ascii_case(PACKAGES_DIR));
    if base != PACKAGES_DIR && !legacy_already_flagged && Path::new(PACKAGES_DIR).is_dir() {
        crate::message::warn(&format!(
            "Dependencies now install to {}/; the old {}/ directory is no longer managed and can be deleted if unused.",
            base, PACKAGES_DIR
        ));
    }

    if !Path::new(&base).exists() {
        fs::create_dir_all(&base)?;
    }

    // The tree describes itself: every installed dir carries a
    // `.forest-receipt` (written after its extraction succeeded) and pointer
    // dirs are recognized by their generated header; no bookkeeping outside
    // the mount. `--force` simply refuses to trust any of it, and a tree from
    // an older forest (no receipts) reinstalls everything the same way.
    let tree = if force {
        crate::roblox::receipts::TreeScan::default()
    } else {
        crate::roblox::receipts::scan(Path::new(&base), &container)
    };
    let rec = crate::roblox::receipts::reconcile(&plan, &tree);
    let (mut to_install, kept, mut stale_dirs) = (rec.to_install, rec.kept, rec.stale_dirs);

    // Local link overrides (`forest link`). The plan above is pure registry
    // graph; from here on each linked slot is opaque: nothing at or under it
    // is installed, deleted, or pointer-written, since writes there would
    // reach the developer's working tree through the junction. The overlay
    // itself is applied after extraction, below.
    let link_res = crate::links::resolve_active(&root_deps);
    for warning in &link_res.warnings {
        crate::message::warn(warning);
    }
    if let Some((count, reason)) = &link_res.ignored {
        crate::message::info(&format!(
            "Ignoring {} local link{}: {}.",
            count,
            if *count == 1 { "" } else { "s" },
            reason
        ));
    }
    let linked_slots: Vec<String> = link_res
        .active
        .iter()
        .map(|l| crate::roblox::link_overlay::slot_plan_path(&container, &l.alias))
        .collect();
    let under_link = |path: &str| {
        linked_slots
            .iter()
            .any(|slot| path == slot || path.starts_with(&format!("{}/", slot)))
    };
    to_install.retain(|&i| !under_link(&plan.packages[i].path));
    stale_dirs.retain(|dir| !under_link(dir));

    // Stale dirs go FIRST: on case-insensitive filesystems (Windows/macOS) a
    // renamed alias's old dir would otherwise delete the freshly extracted
    // new one. exists() guard: children of already-deleted parents are gone.
    for dir in &stale_dirs {
        let p = crate::roblox::physical_path(&base, &container, dir);
        if p.exists() {
            fs::remove_dir_all(&p).with_context(|| format!("Failed to remove stale {}", dir))?;
        }
    }

    // The top level of the mount stays fully managed: any non-exempt dir that
    // isn't a desired root alias is junk or a pre-receipt leftover. (This is
    // also what clears old trees on --force and first-run-after-upgrade.)
    prune_top_level(&plan, &base, &container)?;

    // A reinstall target may hold old content (integrity/root changed);
    // clear it before extraction. Link-aware: when links are being ignored
    // (CI), the target can still BE a junction from an earlier run, which
    // must be removed as a link, never recursed into.
    for &i in &to_install {
        let target = crate::roblox::physical_path(&base, &container, &plan.packages[i].path);
        crate::roblox::link_overlay::remove_slot(&target)
            .with_context(|| format!("Failed to clear {}", plan.packages[i].path))?;
    }

    // Download + extract only what reconciliation says is missing or
    // changed; the shared pool owns prefetch, bars, and error draining.
    let jobs: Vec<DownloadJob<RobloxExtra>> = to_install
        .iter()
        .map(|&i| {
            let pkg = &plan.packages[i];
            DownloadJob {
                name: pkg.name.clone(),
                version: pkg.version.clone(),
                integrity: pkg.integrity.clone(),
                dir: crate::roblox::physical_path(&base, &container, &pkg.path),
                public: pkg.public,
                extra: RobloxExtra { root: pkg.root.clone(), container: pkg.packages_dir.clone() },
            }
        })
        .collect();
    crate::download_pool::download_all(jobs, "roblox", |job, url, on_bytes, cache| {
        // The receipt is written only after ITS dir extracted successfully;
        // per-package atomicity: a dir without a receipt (crash, partial
        // extract) is never trusted.
        fetch_and_extract(url, &job.integrity, &job.dir, &job.extra.root, on_bytes, cache).and_then(
            |_| {
                receipts::write(&job.dir, &receipts::Receipt {
                    name: job.name.clone(),
                    version: job.version.clone(),
                    integrity: job.integrity.clone(),
                    root: job.extra.root.clone(),
                    container: job.extra.container.clone(),
                })
            },
        )
    })
    .await?;

    // Pointer files are always regenerated: a few tiny idempotent writes,
    // self-healing, and immune to hoist-layout drift. Pointer dirs inside a
    // linked slot are skipped (the developer's own install provides them);
    // pointers from other branches INTO a linked subtree keep their targets
    // and resolve through the link.
    for pointer in &plan.pointers {
        if under_link(&pointer.dir) {
            continue;
        }
        write_pointer(&crate::roblox::physical_path(&base, &container, &pointer.dir), &pointer.init_lua)?;
    }

    // Materialize the links before the type pass so pointers targeting a
    // linked package pick up its live exports.
    if !link_res.active.is_empty() {
        for (name, mode) in crate::roblox::link_overlay::apply(&base, &container, &link_res.active)? {
            match mode {
                crate::roblox::link_overlay::AppliedMode::Copy(reason) => {
                    crate::message::warn(&format!("{} linked in copy mode: {}", name, reason));
                }
                crate::roblox::link_overlay::AppliedMode::Junction
                | crate::roblox::link_overlay::AppliedMode::AlreadyLinked => {}
            }
        }
        // Deps the linked working tree declares beyond the pinned registry
        // version come from ITS OWN tree; surface the drift explicitly.
        for link in &link_res.active {
            let pinned_deps = lockfile
                .root_entry(&link.name)
                .map(|e| e.dependencies.clone())
                .unwrap_or_default();
            for diff in crate::links::dep_divergences(link, &pinned_deps) {
                crate::message::info(&format!(
                    "{} (linked) {}; satisfied by its own working tree. Run `forest install` in {} if requires fail.",
                    link.name, diff, link.path_display
                ));
            }
        }
        crate::links::print_banner(&link_res.active, |name| {
            lockfile.pinned_version(name).map(str::to_string)
        });
    }

    // Luau doesn't carry `export type` through `return require(...)`, so we re-export the types
    crate::roblox::type_link::relink_types(Path::new(&base));

    Ok(InstallSummary { installed: to_install.len(), kept })
}

/// Write a pointer module into its dir. A dir that held the physical package
/// last install (the dep got hoisted, e.g. by a top level install of the same
/// package) still has its old contents, and the pointer must not land next to
/// them: the leftover receipt would let a later install keep this dir as if
/// the real package were still inside it, and a leftover init.luau root
/// module would shadow the generated init.lua. The receipt marks exactly that
/// case, so wipe the dir when one is present.
fn write_pointer(target_dir: &Path, init_lua: &str) -> Result<()> {
    if target_dir.join(receipts::RECEIPT_FILE).exists() {
        fs::remove_dir_all(target_dir)
            .with_context(|| format!("Failed to clear former package dir {}", target_dir.display()))?;
    }
    if !target_dir.exists() {
        fs::create_dir_all(target_dir)?;
    }
    let init_path = target_dir.join("init.lua");
    
    // Reduce noise for rojo watchers by skipping the write when the on-disk file is already the same pointer.
    if let Ok(existing) = fs::read_to_string(&init_path) {
        let same_target = matches!(
            (crate::roblox::type_link::require_expr(&existing), crate::roblox::type_link::require_expr(init_lua)),
            (Some(old), Some(new)) if old == new
        );
        if existing.starts_with(crate::roblox::plan::POINTER_HEADER) && same_target {
            return Ok(());
        }
    }
    fs::write(init_path, init_lua)?;
    Ok(())
}

/// Keep the top level of the mount fully managed even when installing
/// incrementally: any non-exempt dir that isn't a desired root alias is junk
/// or a pre-receipt leftover and gets removed. `_`/`.` entries are exempt:
/// a project mid-migration may share this directory with Wally's own
/// `Packages`, whose `_Index` must survive (only DIRS are removed, so
/// wally's root link scripts survive too). Case-insensitive membership
/// because Windows/macOS case-fold names (exact-case renames are handled by
/// the stale/reinstall path, not here). `base` is the physical mount; plan
/// paths stay in the virtual `./<container>/...` format. `container` must be
/// the one the plan was built with, or every plan path gets stripped from
/// `desired` and the whole mount is deleted.
fn prune_top_level(plan: &crate::roblox::plan::InstallPlan, base: &str, container: &str) -> Result<()> {
    let prefix = format!("./{}/", container);
    let desired: std::collections::HashSet<String> = plan.packages.iter()
        .filter_map(|p| {
            let rest = p.path.strip_prefix(&prefix)?;
            if rest.contains('/') { None } else { Some(rest.to_ascii_lowercase()) }
        })
        .collect();

    for entry in fs::read_dir(base)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('_') || name.starts_with('.') {
            continue;
        }
        if desired.contains(&name.to_ascii_lowercase()) {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            // An orphaned link slot (dep removed while still linked): delete
            // the link itself, never through it.
            crate::roblox::link_overlay::remove_link_path(&entry.path())?;
        } else if file_type.is_dir() {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}

/// Find abandoned forest-managed mounts near the current one: an immediate
/// subdirectory of the project top level or of the mount's parent, not the
/// mount itself, whose immediate children carry receipts. Returns relative
/// paths with forward slashes. Comparison against the mount is
/// case-insensitive so the current mount is never listed on case-folding
/// filesystems.
fn find_abandoned_mounts(manifest_dir: &Path, base: &str) -> Vec<String> {
    let base_norm = base.replace('\\', "/");
    let mount = base_norm.to_ascii_lowercase();
    let mut parents: Vec<String> = vec![String::new()];
    if let Some((parent, _)) = base_norm.rsplit_once('/') {
        if !parent.is_empty() {
            parents.push(parent.to_string());
        }
    }

    let mut found = Vec::new();
    for parent in &parents {
        let dir = if parent.is_empty() {
            manifest_dir.to_path_buf()
        } else {
            manifest_dir.join(parent)
        };
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if parent.is_empty() {
                name
            } else {
                format!("{}/{}", parent, name)
            };
            if rel.to_ascii_lowercase() == mount {
                continue;
            }
            if has_receipt_child(&entry.path()) {
                found.push(rel);
            }
        }
    }
    found.sort_unstable();
    found.dedup();
    found
}

/// Does any immediate child of `dir` carry a receipt directly? One level
/// only: receipts sit inside package dirs, one below their mount.
fn has_receipt_child(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else { return false };
    entries
        .flatten()
        .any(|e| e.path().join(receipts::RECEIPT_FILE).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use crate::roblox::plan::{InstallPlan, PlannedPackage};

    #[test]
    fn write_pointer_wipes_a_former_package_dir() {
        // The dep was physical here last install, then got hoisted. Its old
        // root module and receipt must not survive next to the pointer.
        let dir = std::env::temp_dir().join(format!("forest-ptr-wipe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        crate::receipts::write(&dir, &crate::receipts::Receipt {
            name: "acme/x".into(),
            version: "1.0.0".into(),
            integrity: "xx".into(),
            root: "src/init.luau".into(),
            container: "Packages".into(),
        })
        .unwrap();
        fs::write(dir.join("init.luau"), "return {}").unwrap();
        fs::write(dir.join("Helper.luau"), "return {}").unwrap();

        let init_lua = "--Pointer file generated by Forest Package Manager.\nreturn require(script.Parent.Parent.Parent['X'])";
        write_pointer(&dir, init_lua).unwrap();

        assert!(!dir.join("init.luau").exists(), "old root module would shadow the pointer");
        assert!(!dir.join("Helper.luau").exists());
        assert!(!dir.join(crate::receipts::RECEIPT_FILE).exists(), "stale receipt must not survive");
        assert_eq!(fs::read_to_string(dir.join("init.lua")).unwrap(), init_lua);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_pointer_regenerates_over_a_plain_pointer_dir() {
        let dir = std::env::temp_dir().join(format!("forest-ptr-regen-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("init.lua"), "--Pointer file generated by Forest Package Manager.\nold").unwrap();

        write_pointer(&dir, "--Pointer file generated by Forest Package Manager.\nnew").unwrap();

        assert!(fs::read_to_string(dir.join("init.lua")).unwrap().ends_with("new"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_pointer_preserves_a_patched_pointer_with_the_same_target() {
        // Steady state: the pointer on disk carries the type linker's
        // re-exports. Regenerating the same-target pointer must be a no-op,
        // or every install churns the file twice for Rojo watchers.
        let dir = std::env::temp_dir().join(format!("forest-ptr-keep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let patched = "--Pointer file generated by Forest Package Manager.\nlocal MODULE = require(script.Parent.Parent.Parent['X'])\nexport type Foo = MODULE.Foo\nreturn MODULE\n";
        fs::write(dir.join("init.lua"), patched).unwrap();

        let plain = "--Pointer file generated by Forest Package Manager.\nreturn require(script.Parent.Parent.Parent['X'])";
        write_pointer(&dir, plain).unwrap();

        assert_eq!(
            fs::read_to_string(dir.join("init.lua")).unwrap(),
            patched,
            "same-target regeneration must not clobber the relinked pointer"
        );

        // A different target is a real change and must still rewrite.
        let moved = "--Pointer file generated by Forest Package Manager.\nreturn require(script.Parent.Parent['Elsewhere'])";
        write_pointer(&dir, moved).unwrap();
        assert_eq!(fs::read_to_string(dir.join("init.lua")).unwrap(), moved);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_manages_a_nested_mount_and_spares_exempt_entries() {
        let base_dir = std::env::temp_dir().join(format!("forest-prune-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base_dir);
        let mount = base_dir.join("src").join("Packages");
        fs::create_dir_all(mount.join("Knit")).unwrap();
        fs::create_dir_all(mount.join("Junk")).unwrap();
        fs::create_dir_all(mount.join("_Index")).unwrap();
        fs::create_dir_all(mount.join(".cache")).unwrap();

        // Plan paths stay virtual ("./Packages/...") regardless of the mount.
        let plan = InstallPlan {
            packages: vec![PlannedPackage {
                path: "./Packages/Knit".to_string(),
                name: "acme/knit".to_string(),
                version: "1.0.0".to_string(),
                integrity: "aa".to_string(),
                root: "init.luau".to_string(),
                packages_dir: "Packages".to_string(),
                public: true,
            }],
            pointers: vec![],
        };

        prune_top_level(&plan, &mount.to_string_lossy(), "Packages").unwrap();

        assert!(mount.join("Knit").exists(), "desired alias survives");
        assert!(!mount.join("Junk").exists(), "junk under the nested mount is pruned");
        assert!(mount.join("_Index").exists(), "wally's _Index is exempt");
        assert!(mount.join(".cache").exists(), "dot entries are exempt");
        let _ = fs::remove_dir_all(&base_dir);
    }

    #[test]
    fn prune_manages_a_renamed_mount() {
        // Plan paths carry the renamed virtual prefix; prune must strip that
        // prefix or every desired alias looks like junk.
        let base_dir = std::env::temp_dir().join(format!("forest-prune-renamed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base_dir);
        let mount = base_dir.join("roblox_packages");
        fs::create_dir_all(mount.join("Knit")).unwrap();
        fs::create_dir_all(mount.join("Junk")).unwrap();
        fs::create_dir_all(mount.join("_Index")).unwrap();

        let plan = InstallPlan {
            packages: vec![PlannedPackage {
                path: "./roblox_packages/Knit".to_string(),
                name: "acme/knit".to_string(),
                version: "1.0.0".to_string(),
                integrity: "aa".to_string(),
                root: "init.luau".to_string(),
                packages_dir: "Packages".to_string(),
                public: true,
            }],
            pointers: vec![],
        };

        prune_top_level(&plan, &mount.to_string_lossy(), "roblox_packages").unwrap();

        assert!(mount.join("Knit").exists(), "desired alias survives under the renamed mount");
        assert!(!mount.join("Junk").exists(), "junk is still pruned");
        assert!(mount.join("_Index").exists(), "exempt entries still survive");
        let _ = fs::remove_dir_all(&base_dir);
    }

    /// Fresh project dir with a receipt-bearing package at `mount/Pkg`.
    fn abandoned_fixture(tag: &str, mount: &str) -> PathBuf {
        let project = std::env::temp_dir().join(format!("forest-abandoned-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&project);
        let pkg = project.join(mount).join("Pkg");
        fs::create_dir_all(&pkg).unwrap();
        crate::receipts::write(&pkg, &crate::receipts::Receipt {
            name: "acme/pkg".into(),
            version: "1.0.0".into(),
            integrity: "aa".into(),
            root: "init.luau".into(),
            container: "Packages".into(),
        })
        .unwrap();
        project
    }

    #[test]
    fn abandoned_detects_a_renamed_top_level_mount() {
        let project = abandoned_fixture("rename", "OldName");
        fs::create_dir_all(project.join("NewName")).unwrap();

        let found = find_abandoned_mounts(&project, "NewName");

        assert_eq!(found, vec!["OldName".to_string()]);
        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn abandoned_detects_a_nested_mount_left_behind() {
        let project = abandoned_fixture("nested", "src/OldName");
        fs::create_dir_all(project.join("src").join("NewName")).unwrap();

        let found = find_abandoned_mounts(&project, "src/NewName");

        assert_eq!(found, vec!["src/OldName".to_string()]);
        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn abandoned_skips_a_receipt_less_packages_dir() {
        // Pre-receipt forest or Wally territory: the legacy warning owns it.
        let project = abandoned_fixture("legacy", "NewName");
        fs::create_dir_all(project.join("Packages").join("Knit")).unwrap();

        let found = find_abandoned_mounts(&project, "NewName");

        assert!(found.is_empty(), "no receipts means not provably forest's: {:?}", found);
        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn abandoned_never_lists_the_current_mount() {
        let project = abandoned_fixture("current", "Packages");

        assert!(find_abandoned_mounts(&project, "Packages").is_empty());
        // Casing differences on a case-insensitive filesystem still match.
        assert!(find_abandoned_mounts(&project, "packages").is_empty());
        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn abandoned_skips_junk_dirs_without_receipt_children() {
        let project = abandoned_fixture("junk", "NewName");
        fs::create_dir_all(project.join("assets").join("textures")).unwrap();
        fs::write(project.join("assets").join("readme.txt"), "x").unwrap();

        let found = find_abandoned_mounts(&project, "NewName");

        assert!(found.is_empty(), "receipt-less dirs are not mounts: {:?}", found);
        let _ = fs::remove_dir_all(&project);
    }
}
