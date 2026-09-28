//! Roblox install executor: the hoisted `Packages/` tree with pointer
//! `init.lua` shims. Moved verbatim from lockfile_gen.rs when the platform
//! seam was introduced; reached only via `Platform::install`. Downloads go
//! through the shared pool (src/download_pool.rs).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::download_pool::DownloadJob;
use crate::lockfile::LockSection;
use crate::lockfile_gen::InstallSummary;
use crate::lockfile_solver::DepSpec;
use crate::mounts::Mount;
use crate::receipts;
use crate::roblox::extract::fetch_and_extract;
use crate::roblox::plan::plan_install;
use crate::roblox::scratch::{rename_with_retry, scratch_dirs, ScratchDirs, StagingArea, TrashBin};
use crate::roblox::PACKAGES_DIR;

/// A live rojo may still be draining events from a forest run that just
/// finished, and mutating the mount again before its queue drains crashes
/// it. The lockfile's mtime marks the end of the last mutating run; when it
/// is fresh and a rojo answers from this directory, wait out the remainder.
/// One-off commands never wait, and chains with no rojo attached fail the
/// probe fast. FOREST_NO_SETTLE=1 skips even the probe.
fn settle_watchers() {
    // Margin, not load-bearing: the rapid-cycle bench passes with the wait
    // disabled outright. A knit-sized tree takes a connected rojo ~2s to
    // ingest; 3s covers that while keeping chained installs snappy.
    const SETTLE: std::time::Duration = std::time::Duration::from_secs(3);
    if std::env::var("FOREST_NO_SETTLE").as_deref() == Ok("1") {
        return;
    }
    let Ok(meta) = fs::metadata("forest-lock.json") else { return };
    let Ok(modified) = meta.modified() else { return };
    let Ok(age) = modified.elapsed() else { return };
    if age >= SETTLE {
        return;
    }
    if !rojo_is_serving() {
        return;
    }
    std::thread::sleep(SETTLE - age);
}

/// Is a rojo dev server plausibly serving this project? Checks the port in
/// default.project.json's `servePort` (when present) and rojo's default
/// 34872. Raw TCP: the probe runs on the async runtime's thread, where a
/// blocking HTTP client can't be constructed.
fn rojo_is_serving() -> bool {
    let mut ports = vec![34872u16];
    if let Ok(text) = fs::read_to_string("default.project.json") {
        if let Ok(project) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(port) = project.get("servePort").and_then(|p| p.as_u64()) {
                ports.insert(0, port as u16);
            }
        }
    }
    ports.dedup();
    ports.iter().any(|&port| probe_rojo_port(port))
}

fn probe_rojo_port(port: u16) -> bool {
    use std::io::{Read, Write};
    let timeout = std::time::Duration::from_millis(250);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = std::net::TcpStream::connect_timeout(&addr, timeout) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let request = format!("GET /api/rojo HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 256];
    match stream.read(&mut buf) {
        Ok(n) if n > 0 => String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1 200"),
        _ => false,
    }
}

/// Clear a reinstall target. A junction/symlink (a slot left by an earlier
/// run while links were ignored) is removed as a link so the target tree is
/// never touched; a real dir renames into the trash bin. A target already
/// gone (its parent renamed out first) is a no-op.
fn clear_target(target: &Path, trash: &mut TrashBin) -> Result<()> {
    let Ok(meta) = fs::symlink_metadata(target) else {
        return Ok(());
    };
    if meta.file_type().is_symlink() {
        crate::roblox::link_overlay::remove_link_path(target)
            .with_context(|| format!("Failed to remove link at {}", target.display()))?;
    } else if meta.is_dir() {
        trash.remove_dir_all(target)?;
    } else {
        fs::remove_file(target)?;
    }
    Ok(())
}

/// What the installer needs per job beyond the pool's shared fields.
struct RobloxExtra {
    /// archiveRoot: picks the extraction layout, recorded in the receipt.
    root: String,
    /// The package's own nested container name, recorded in its receipt.
    container: String,
}

/// A renamed `packagesDir`, a root move within the same parent, or a mount
/// dropped from forest.json leaves a tree behind. Receipts prove which
/// leftovers are forest's and those get a direct warning; a receipt-less
/// literal `Packages/` (pre-receipt forest, or Wally's) keeps the softer
/// legacy warning. Un-nesting the root moves the old mount out of the
/// scanned parents, so that case stays silent. Runs once per install across
/// every mount, so a sibling mount is never mistaken for an old one.
/// `orphaned` were just reported by the caller.
pub fn warn_abandoned_mounts(mounts: &[Mount], orphaned: &[String]) {
    let Some(default) = mounts.iter().find(|m| m.is_default()) else { return };
    let known: Vec<String> = mounts
        .iter()
        .map(|m| m.path.to_ascii_lowercase())
        .chain(orphaned.iter().map(|p| p.to_ascii_lowercase()))
        .collect();
    let abandoned = find_abandoned_mounts(Path::new("."), &default.path, &known);
    if !abandoned.is_empty() {
        let list = abandoned
            .iter()
            .map(|d| format!("{}/", d))
            .collect::<Vec<_>>()
            .join(", ");
        crate::message::warn(&format!(
            "Found dependency folder(s) forest no longer manages: {}. Delete them if you don't need them.",
            list
        ));
    }
    let legacy_already_flagged = abandoned.iter().any(|d| d.eq_ignore_ascii_case(PACKAGES_DIR));
    let legacy_is_a_mount = known.iter().any(|p| p.eq_ignore_ascii_case(PACKAGES_DIR));
    if !legacy_is_a_mount && !legacy_already_flagged && Path::new(PACKAGES_DIR).is_dir() {
        crate::message::warn(&format!(
            "Dependencies now install to {}/; the old {}/ directory is no longer managed and can be deleted if unused.",
            default.path, PACKAGES_DIR
        ));
    }
}

pub async fn make_directories_roblox(
    section: &LockSection,
    root_deps: HashMap<String, DepSpec>,
    mount: &Mount,
    force: bool,
) -> Result<InstallSummary> {
    // The receipt scan and type pass skip `_`/`.` entries, so a package
    // installed under such a name would never be recognized as installed.
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
    // onto the physical mount only here. The mount's folder name is the
    // container prefix and its path the base, so the planner's root prefix,
    // physical_path, the receipt scan, and prune_top_level can never
    // disagree.
    let container = mount.name().to_string();
    let plan = plan_install(section, &root_deps, &container)?;
    let base = mount.path.clone();

    // All mount deletions below go through the bin (see TrashBin) so a live
    // `rojo serve` never sees a child removal under an already-gone parent.
    let ScratchDirs { trash: trash_dir, staging: staging_dir } = scratch_dirs();
    let mut trash = TrashBin::new(trash_dir);
    crate::roblox::scratch::sweep_leftovers();

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
    let link_res = crate::links::resolve_active(&root_deps, mount);
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
    let under_link = |path: &str| at_or_under(path, &linked_slots);
    to_install.retain(|&i| !under_link(&plan.packages[i].path));
    stale_dirs.retain(|dir| !under_link(dir));

    if !to_install.is_empty() || !stale_dirs.is_empty() {
        settle_watchers();
    }

    // Stale dirs go FIRST: on case-insensitive filesystems (Windows/macOS) a
    // renamed alias's old dir would otherwise delete the freshly extracted
    // new one. exists() guard: children of already-deleted parents are gone.
    for dir in &stale_dirs {
        let p = crate::roblox::physical_path(&base, &container, dir);
        if p.exists() {
            trash.remove_dir_all(&p).with_context(|| format!("Failed to remove stale {}", dir))?;
        }
    }

    // The top level of the mount stays fully managed: anything that isn't a
    // desired root alias is removed. (This is also what clears old trees on
    // --force and first-run-after-upgrade, and a Wally install's leftovers.)
    let foreign = prune_top_level(&plan, &base, &container, &mut trash)?;
    if !foreign.is_empty() {
        crate::message::info(&format!(
            "Removed {} from {}/; forest manages everything in its dependency folders.",
            foreign.join(", "),
            base
        ));
    }

    // A reinstall target may hold old content, clear it before extraction.
    // Parents first: renaming a parent out takes its nested targets with it,
    // so each cleared unit emits one watcher event (out-of-order delivery of
    // the deep removals crashes a live rojo). The target can also be a
    // junction from a run where links were ignored; clear_target removes
    // those as links.
    let mut clear_order = to_install.clone();
    clear_order.sort_by_key(|&i| plan.packages[i].path.len());
    for &i in &clear_order {
        let target = crate::roblox::physical_path(&base, &container, &plan.packages[i].path);
        clear_target(&target, &mut trash)
            .with_context(|| format!("Failed to clear {}", plan.packages[i].path))?;
    }

    // Pointer dirs written into a staged unit below, skipped by the in-place
    // pointer pass at the end.
    let mut staged_pointers: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Refused private packages. Their slots and everything under them stay empty.
    let mut denied_paths: Vec<String> = Vec::new();

    // Download + extract only what reconciliation says is missing or
    // changed; the shared pool owns prefetch, bars, and error draining.
    // Tarballs extract into staging, not the mount (see StagingArea).
    if !to_install.is_empty() {
        let mut staging = StagingArea::new(staging_dir);
        // (plan path, staged dir, final dir) for the rename-in phase.
        let mut placements: Vec<(String, PathBuf, PathBuf)> = Vec::new();
        let mut jobs: Vec<DownloadJob<RobloxExtra>> = Vec::new();
        for &i in &to_install {
            let pkg = &plan.packages[i];
            let dir_path = crate::roblox::physical_path(&base, &container, &pkg.path);
            let stage = staging.alloc(i)?;
            placements.push((pkg.path.clone(), stage.clone(), dir_path));
            jobs.push(DownloadJob {
                name: pkg.name.clone(),
                version: pkg.version.clone(),
                integrity: pkg.integrity.clone(),
                dir: stage,
                public: pkg.public,
                extra: RobloxExtra { root: pkg.root.clone(), container: pkg.packages_dir.clone() },
            });
        }
        // Script sources found during extraction. Receipted packages never
        // re-extract, so the warning fires on first install and upgrades.
        let script_findings: std::sync::Arc<std::sync::Mutex<Vec<(String, String, Vec<String>)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let findings_sink = script_findings.clone();

        // On a pool error nothing has landed in the mount: staged dirs vanish
        // with `staging`, and the tarball cache makes the retry's redownloads
        // free.
        let denied = crate::download_pool::download_all(jobs, "roblox", move |job, url, on_bytes, cache| {
            // The receipt is written into the staged dir after its extraction
            // succeeds, so what renames into the mount is always a complete,
            // receipted package.
            let report = fetch_and_extract(url, &job.integrity, &job.dir, &job.extra.root, on_bytes, cache)?;
            if !report.script_sources.is_empty() {
                findings_sink
                    .lock()
                    .expect("script findings lock")
                    .push((job.name.clone(), job.version.clone(), report.script_sources));
            }
            receipts::write(&job.dir, &receipts::Receipt {
                name: job.name.clone(),
                version: job.version.clone(),
                integrity: job.integrity.clone(),
                root: job.extra.root.clone(),
                container: job.extra.container.clone(),
            })
        })
        .await?;

        // Refused packages never extracted. Drop them and everything under
        // them, or a receipt-less folder of nested deps lands in the mount.
        denied_paths = to_install
            .iter()
            .map(|&i| &plan.packages[i])
            .filter(|p| denied.iter().any(|d| d.name == p.name && d.version == p.version))
            .map(|p| p.path.clone())
            .collect();
        placements.retain(|(path, _, _)| !at_or_under(path, &denied_paths));

        let mut findings = script_findings.lock().expect("script findings lock").split_off(0);
        findings.sort();
        for (name, version, files) in findings {
            let shown = files.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
            let more = if files.len() > 3 {
                format!(" and {} more", files.len() - 3)
            } else {
                String::new()
            };
            crate::message::warn(&format!(
                "{}@{} contains script files that can run in your place: {}{}. Review them if unexpected.",
                name, version, shown, more
            ));
        }

        // Assemble nested packages inside their installing ancestor's staged
        // dir, deepest first so a package's own children are in place before
        // it moves. Whatever has no installing ancestor renames into the
        // mount afterwards, arriving complete (nested deps, pointers,
        // receipts) in one atomic event.
        placements.sort_by_key(|(path, _, _)| std::cmp::Reverse(path.len()));
        let mut mount_moves: Vec<usize> = Vec::new();
        for i in 0..placements.len() {
            let (path, stage, _) = placements[i].clone();
            let ancestor = placements
                .iter()
                .filter(|(a, _, _)| path.starts_with(&format!("{}/", a)))
                .max_by_key(|(a, _, _)| a.len());
            match ancestor {
                Some((a_path, a_stage, _)) => {
                    let target = a_stage.join(Path::new(&path[a_path.len() + 1..]));
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    rename_with_retry(&stage, &target)
                        .with_context(|| format!("Failed to assemble {}", path))?;
                }
                None => mount_moves.push(i),
            }
        }

        // Pointer files inside a staged unit are part of the unit.
        for pointer in &plan.pointers {
            let topmost = placements
                .iter()
                .filter(|(a, _, _)| pointer.dir.starts_with(&format!("{}/", a)))
                .min_by_key(|(a, _, _)| a.len());
            if at_or_under(&pointer.dir, &denied_paths) {
                continue;
            }
            if let Some((a_path, a_stage, _)) = topmost {
                let target = a_stage.join(Path::new(&pointer.dir[a_path.len() + 1..]));
                write_pointer(&target, &pointer.init_lua, &mut trash)?;
                staged_pointers.insert(pointer.dir.clone());
            }
        }

        // Patch link files while the unit is still staged: the writes never
        // become watcher events. Links whose chains leave the unit resolve in
        // the in-mount pass at the end instead.
        for &i in &mount_moves {
            crate::roblox::type_link::relink_types_staged(&placements[i].1);
        }

        for &i in &mount_moves {
            let (path, stage, dest) = &placements[i];
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            rename_with_retry(stage, dest)
                .with_context(|| format!("Failed to move {} into place", path))?;
        }
    }

    // Remaining pointer files (dirs under kept packages, or at the mount top
    // level) are regenerated in place: tiny idempotent writes, self-healing,
    // and immune to hoist-layout drift. Pointer dirs inside a linked slot are
    // skipped (the developer's own install provides them); pointers from
    // other branches INTO a linked subtree keep their targets and resolve
    // through the link.
    for pointer in &plan.pointers {
        if staged_pointers.contains(&pointer.dir)
            || under_link(&pointer.dir)
            || at_or_under(&pointer.dir, &denied_paths)
        {
            continue;
        }
        write_pointer(&crate::roblox::physical_path(&base, &container, &pointer.dir), &pointer.init_lua, &mut trash)?;
    }

    // Materialize the links before the type pass so pointers targeting a
    // linked package pick up its live exports.
    if !link_res.active.is_empty() {
        for (name, mode) in crate::roblox::link_overlay::apply(&base, &container, &link_res.active, &mut trash)? {
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
            let pinned_deps = section
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
            section.pinned_version(name).map(str::to_string)
        });
    }

    // Luau doesn't carry `export type` through `return require(...)`, so we re-export the types
    crate::roblox::type_link::relink_types(Path::new(&base));

    let skipped = to_install
        .iter()
        .filter(|&&i| at_or_under(&plan.packages[i].path, &denied_paths))
        .count();
    Ok(InstallSummary { installed: to_install.len() - skipped, kept })
}

/// Whether a plan path is one of `roots` or nested inside one.
fn at_or_under(path: &str, roots: &[String]) -> bool {
    roots
        .iter()
        .any(|root| path == root || path.starts_with(&format!("{}/", root)))
}

/// Write a pointer module into its dir. A dir that held the physical package
/// last install (the dep got hoisted, e.g. by a top level install of the same
/// package) still has its old contents, and the pointer must not land next to
/// them: the leftover receipt would let a later install keep this dir as if
/// the real package were still inside it, and a leftover init.luau root
/// module would shadow the generated init.lua. The receipt marks exactly that
/// case, so wipe the dir when one is present.
fn write_pointer(target_dir: &Path, init_lua: &str, trash: &mut TrashBin) -> Result<()> {
    if target_dir.join(receipts::RECEIPT_FILE).exists() {
        trash.remove_dir_all(target_dir)
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
/// incrementally: anything that isn't a desired root alias is removed, files
/// included. The mount is forest's alone, so a Wally `_Index` and Wally's
/// link modules (`Packages/Promise.lua`, which would shadow forest's
/// `Promise/` in Rojo) go too. Dot entries (`.gitkeep`) are never touched.
/// Case-insensitive membership because Windows/macOS case-fold names
/// (exact-case renames are handled by the stale/reinstall path, not here).
/// `base` is the physical mount; plan paths stay in the virtual
/// `./<container>/...` format. `container` must be the one the plan was
/// built with, or every plan path gets stripped from `desired` and the whole
/// mount is deleted. Returns the names of removed entries forest never
/// installed (files and receipt-less dirs), for the caller to report.
fn prune_top_level(plan: &crate::roblox::plan::InstallPlan, base: &str, container: &str, trash: &mut TrashBin) -> Result<Vec<String>> {
    let prefix = format!("./{}/", container);
    let desired: std::collections::HashSet<String> = plan.packages.iter()
        .filter_map(|p| {
            let rest = p.path.strip_prefix(&prefix)?;
            if rest.contains('/') { None } else { Some(rest.to_ascii_lowercase()) }
        })
        .collect();

    let mut foreign = Vec::new();
    for entry in fs::read_dir(base)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
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
            if !entry.path().join(receipts::RECEIPT_FILE).is_file() {
                foreign.push(format!("{}/", name));
            }
            trash.remove_dir_all(&entry.path())?;
        } else {
            foreign.push(name);
            trash.remove_file(&entry.path())?;
        }
    }
    foreign.sort();
    Ok(foreign)
}

/// Find abandoned forest-managed mounts near the default one: an immediate
/// subdirectory of the project top level or of the default mount's parent,
/// not one of `known` (every declared mount, lowercased), whose immediate
/// children carry receipts. Returns relative paths with forward slashes.
/// The comparison is case-insensitive so a current mount is never listed on
/// case-folding filesystems.
fn find_abandoned_mounts(manifest_dir: &Path, base: &str, known: &[String]) -> Vec<String> {
    let base_norm = base.replace('\\', "/");
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
            // `_`/`.` folders are never mounts (and a `.forest-trash` left
            // by a crashed run holds receipts).
            if name.starts_with('_') || name.starts_with('.') {
                continue;
            }
            let rel = if parent.is_empty() {
                name
            } else {
                format!("{}/{}", parent, name)
            };
            if known.contains(&rel.to_ascii_lowercase()) {
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

    /// TrashBin pointed at a unique temp dir so tests never touch the cwd.
    fn test_trash(tag: &str) -> TrashBin {
        TrashBin::new(std::env::temp_dir().join(format!("forest-trash-test-{}-{}", tag, std::process::id())))
    }

    #[test]
    fn at_or_under_matches_whole_path_segments_only() {
        let roots = vec!["./Packages/Knit".to_string()];
        assert!(at_or_under("./Packages/Knit", &roots));
        assert!(at_or_under("./Packages/Knit/Packages/Promise", &roots));
        // A sibling that merely shares the prefix is not inside the slot.
        assert!(!at_or_under("./Packages/KnitUtils", &roots));
        assert!(!at_or_under("./Packages/Promise", &roots));
        assert!(!at_or_under("./Packages/Knit", &[]));
    }

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
        write_pointer(&dir, init_lua, &mut test_trash("wipe")).unwrap();

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

        write_pointer(&dir, "--Pointer file generated by Forest Package Manager.\nnew", &mut test_trash("regen")).unwrap();

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
        write_pointer(&dir, plain, &mut test_trash("keep")).unwrap();

        assert_eq!(
            fs::read_to_string(dir.join("init.lua")).unwrap(),
            patched,
            "same-target regeneration must not clobber the relinked pointer"
        );

        // A different target is a real change and must still rewrite.
        let moved = "--Pointer file generated by Forest Package Manager.\nreturn require(script.Parent.Parent['Elsewhere'])";
        write_pointer(&dir, moved, &mut test_trash("keep2")).unwrap();
        assert_eq!(fs::read_to_string(dir.join("init.lua")).unwrap(), moved);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A plan holding one top-level Knit under the given container.
    fn knit_plan(container: &str) -> InstallPlan {
        InstallPlan {
            packages: vec![PlannedPackage {
                path: format!("./{}/Knit", container),
                name: "acme/knit".to_string(),
                version: "1.0.0".to_string(),
                integrity: "aa".to_string(),
                root: "init.luau".to_string(),
                packages_dir: "Packages".to_string(),
                public: true,
            }],
            pointers: vec![],
        }
    }

    #[test]
    fn prune_manages_a_nested_mount_and_spares_dot_entries() {
        let base_dir = std::env::temp_dir().join(format!("forest-prune-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base_dir);
        let mount = base_dir.join("src").join("Packages");
        fs::create_dir_all(mount.join("Knit")).unwrap();
        fs::create_dir_all(mount.join("Junk")).unwrap();
        fs::create_dir_all(mount.join(".cache")).unwrap();
        fs::write(mount.join(".gitkeep"), "").unwrap();

        // Plan paths stay virtual ("./Packages/...") regardless of the mount.
        prune_top_level(&knit_plan("Packages"), &mount.to_string_lossy(), "Packages", &mut test_trash("prune")).unwrap();

        assert!(mount.join("Knit").exists(), "desired alias survives");
        assert!(!mount.join("Junk").exists(), "junk under the nested mount is pruned");
        assert!(mount.join(".cache").exists(), "dot entries are exempt");
        assert!(mount.join(".gitkeep").exists(), "dot files too");
        let _ = fs::remove_dir_all(&base_dir);
    }

    #[test]
    fn prune_takes_over_a_wally_packages_folder() {
        // A Wally install's leftovers: the _Index store and one link module
        // per dependency. Forest owns the folder, so all of it goes, and
        // everything forest never installed is reported.
        let base_dir = std::env::temp_dir().join(format!("forest-prune-wally-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base_dir);
        let mount = base_dir.join("Packages");
        fs::create_dir_all(mount.join("_Index").join("evaera_promise@4.0.0").join("promise")).unwrap();
        fs::write(mount.join("Promise.lua"), "return require(script.Parent._Index[\"evaera_promise@4.0.0\"][\"promise\"])\n").unwrap();
        fs::write(mount.join("Knit.lua"), "return require(script.Parent._Index[\"sleitnick_knit@1.7.0\"][\"knit\"])\n").unwrap();
        fs::create_dir_all(mount.join("Knit")).unwrap();
        // A stale forest package: pruned, but not reported as foreign.
        let stale = mount.join("Trove");
        fs::create_dir_all(&stale).unwrap();
        crate::receipts::write(&stale, &crate::receipts::Receipt {
            name: "acme/trove".into(),
            version: "1.0.0".into(),
            integrity: "cc".into(),
            root: "init.luau".into(),
            container: "Packages".into(),
        })
        .unwrap();

        let foreign = prune_top_level(&knit_plan("Packages"), &mount.to_string_lossy(), "Packages", &mut test_trash("prune-wally")).unwrap();

        assert!(mount.join("Knit").is_dir(), "the planned package stays");
        assert!(!mount.join("_Index").exists(), "wally's store is removed");
        assert!(!mount.join("Promise.lua").exists(), "wally link modules are removed");
        assert!(!mount.join("Knit.lua").exists(), "including one that would shadow Knit/");
        assert!(!stale.exists());
        assert_eq!(foreign, vec!["Knit.lua".to_string(), "Promise.lua".to_string(), "_Index/".to_string()]);
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

        prune_top_level(&knit_plan("roblox_packages"), &mount.to_string_lossy(), "roblox_packages", &mut test_trash("prune-renamed")).unwrap();

        assert!(mount.join("Knit").exists(), "desired alias survives under the renamed mount");
        assert!(!mount.join("Junk").exists(), "junk is still pruned");
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

        let found = find_abandoned_mounts(&project, "NewName", &known(&["NewName"]));

        assert_eq!(found, vec!["OldName".to_string()]);
        let _ = fs::remove_dir_all(&project);
    }

    fn known(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_ascii_lowercase()).collect()
    }

    #[test]
    fn abandoned_never_lists_a_sibling_mount() {
        // DevPackages holds receipts too, but it is a declared mount.
        let project = abandoned_fixture("sibling", "DevPackages");
        fs::create_dir_all(project.join("Packages")).unwrap();

        assert!(find_abandoned_mounts(&project, "Packages", &known(&["Packages", "DevPackages"])).is_empty());
        assert_eq!(
            find_abandoned_mounts(&project, "Packages", &known(&["Packages"])),
            vec!["DevPackages".to_string()],
            "the same folder is abandoned once the mount is gone"
        );
        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn abandoned_detects_a_nested_mount_left_behind() {
        let project = abandoned_fixture("nested", "src/OldName");
        fs::create_dir_all(project.join("src").join("NewName")).unwrap();

        let found = find_abandoned_mounts(&project, "src/NewName", &known(&["src/NewName"]));

        assert_eq!(found, vec!["src/OldName".to_string()]);
        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn abandoned_skips_a_receipt_less_packages_dir() {
        // Pre-receipt forest or Wally territory: the legacy warning owns it.
        let project = abandoned_fixture("legacy", "NewName");
        fs::create_dir_all(project.join("Packages").join("Knit")).unwrap();

        let found = find_abandoned_mounts(&project, "NewName", &known(&["NewName"]));

        assert!(found.is_empty(), "no receipts means not provably forest's: {:?}", found);
        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn abandoned_never_lists_the_current_mount() {
        let project = abandoned_fixture("current", "Packages");

        assert!(find_abandoned_mounts(&project, "Packages", &known(&["Packages"])).is_empty());
        // Casing differences on a case-insensitive filesystem still match.
        assert!(find_abandoned_mounts(&project, "packages", &known(&["packages"])).is_empty());
        let _ = fs::remove_dir_all(&project);
    }

    #[test]
    fn abandoned_skips_junk_dirs_without_receipt_children() {
        let project = abandoned_fixture("junk", "NewName");
        fs::create_dir_all(project.join("assets").join("textures")).unwrap();
        fs::write(project.join("assets").join("readme.txt"), "x").unwrap();

        let found = find_abandoned_mounts(&project, "NewName", &known(&["NewName"]));

        assert!(found.is_empty(), "receipt-less dirs are not mounts: {:?}", found);
        let _ = fs::remove_dir_all(&project);
    }
}
