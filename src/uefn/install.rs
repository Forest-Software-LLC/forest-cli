//! UEFN install executor.
//!
//! Reached only via the platform dispatch at the top of
//! lockfile_gen::make_directories. The flow mirrors the Roblox executor
//! (plan → scan → reconcile → clean → download → bookkeeping) but against
//! the flat mount with the 3-way ownership taxonomy (installed / authored /
//! unknown) and Verse marker regeneration instead of pointer files.
//! Downloads go through the shared pool (src/download_pool.rs).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::download_pool::DownloadJob;
use crate::lockfile_gen::{InstallSummary, LockFile};
use crate::fetch_and_extract::fetch_and_extract_verbatim;
use crate::lockfile_solver::DepSpec;
use crate::message::{info, warn};
use crate::receipts;

use super::plan::plan_install_uefn;

pub async fn make_directories_uefn(
    lockfile: &LockFile,
    root_deps: HashMap<String, DepSpec>,
    force: bool,
) -> Result<InstallSummary> {
    // `forest link` is Roblox-only for now; a links file here (hand-written
    // or left over) must not silently change anything.
    if !crate::links::stored_links().is_empty() {
        warn("forest link is not supported on UEFN; ignoring .forest/links.json.");
    }

    let cwd = std::env::current_dir().context("Failed to read current directory")?;
    let project = super::find_project(&cwd).ok_or_else(|| {
        anyhow!(
            "No UEFN project found: no *.uefnproject file in this directory or any parent. \
             Is forest.json inside a UEFN project's Content folder?"
        )
    })?;
    // The mount, markers, and lockfile all live at the project's Content
    // folder regardless of where install was invoked (e.g. inside an
    // authored package, where the local manifest declares the dep but
    // resolution is workspace-wide). Everything below is Content-relative,
    // and the caller's subsequent forest-lock.json write lands here too.
    if cwd != project.content_dir {
        std::env::set_current_dir(&project.content_dir).with_context(|| {
            format!("Failed to enter {}", project.content_dir.display())
        })?;
    }
    // Scoped markers embed the project's Verse path, which is why install
    // must be re-run after a project rename or first publish - regeneration
    // below self-heals the markers.
    let verse_path = super::read_verse_path(&project)
        .context("Could not read the project's VersePath (needed for package visibility markers)")?;

    let mount_name = crate::contracts::verse_rules().packages_mount.clone();
    let mount = PathBuf::from(&mount_name);
    if !mount.exists() {
        fs::create_dir_all(&mount)?;
    }

    // ALWAYS scan - even under --force. Unlike the Roblox tree, the mount is
    // shared with authored/unknown content that must never be wiped; force
    // only distrusts receipts (every planned package reinstalls), it does
    // not turn the mount into a fully-managed directory.
    let tree = receipts::scan_flat(&mount, &mount_name, super::MARKER_HEADER);

    let authored_pairs: Vec<(String, String)> = tree
        .authored
        .iter()
        .map(|a| (a.scope_dir.clone(), a.name_dir.clone()))
        .collect();

    let plan = plan_install_uefn(lockfile, &root_deps, &verse_path, &authored_pairs)?;
    for warning in &plan.warnings {
        warn(warning);
    }

    let planned_dirs: Vec<receipts::FlatPlannedDir> = plan
        .packages
        .iter()
        .map(|p| receipts::FlatPlannedDir { path: &p.path, integrity: &p.integrity, root: "" })
        .collect();
    let mut rec = receipts::reconcile_flat(&planned_dirs, &tree);

    if force {
        // Reinstall everything that isn't blocked: kept entries join
        // to_install; blocked (authored/unknown occupying a planned path)
        // stays untouchable even under force.
        let blocked: std::collections::HashSet<&str> =
            rec.blocked.iter().map(String::as_str).collect();
        rec.to_install = plan
            .packages
            .iter()
            .enumerate()
            .filter(|(_, p)| !blocked.contains(p.path.as_str()))
            .map(|(i, _)| i)
            .collect();
        rec.kept = 0;
    }

    for path in &rec.blocked {
        warn(&format!(
            "{} is expected from the lockfile but isn't Forest-managed (no install receipt). \
             Move it out or remove the dependency; skipping. --force never overwrites it either.",
            path
        ));
    }
    for path in &tree.unknown {
        warn(&format!(
            "{} is neither installed nor authored. Add a forest.json to make it an authored \
             package, or move it out of {}.",
            path, mount_name
        ));
    }

    // Stale dirs first (case-insensitive filesystems: an old-cased dir would
    // otherwise delete a freshly extracted one), then clear reinstall targets.
    for dir in &rec.stale_dirs {
        let p = Path::new(dir);
        if p.exists() {
            fs::remove_dir_all(p).with_context(|| format!("Failed to remove stale {}", dir))?;
        }
    }
    for &i in &rec.to_install {
        let target = PathBuf::from(&plan.packages[i].path);
        if target.exists() {
            fs::remove_dir_all(&target)
                .with_context(|| format!("Failed to clear {}", plan.packages[i].path))?;
        }
    }

    // Download + extract via the shared pool (verbatim extraction: the
    // folder IS the package on UEFN).
    let jobs: Vec<DownloadJob<()>> = rec
        .to_install
        .iter()
        .map(|&i| {
            let pkg = &plan.packages[i];
            DownloadJob {
                name: pkg.name.clone(),
                version: pkg.version.clone(),
                integrity: pkg.integrity.clone(),
                dir: PathBuf::from(&pkg.path),
                public: pkg.public,
                extra: (),
            }
        })
        .collect();
    crate::download_pool::download_all(jobs, "uefn", |job, url, on_bytes, cache| {
        // Receipt only after ITS dir extracted - per-package atomicity.
        fetch_and_extract_verbatim(url, &job.integrity, &job.dir, on_bytes, cache).and_then(|_| {
            receipts::write(
                &job.dir,
                &receipts::Receipt {
                    name: job.name.clone(),
                    version: job.version.clone(),
                    integrity: job.integrity.clone(),
                    // No entry point on UEFN - the folder is the package.
                    root: String::new(),
                    // No nested containers either (flat mount); keep the
                    // serde default.
                    container: receipts::default_container(),
                },
            )
        })
    })
    .await?;

    // Marker regeneration - every run, like Roblox pointer files. Deleting
    // is licensed ONLY by the header; writing is unconditional and idempotent
    // (self-heals a VersePath change after project rename/first publish).
    let markers_changed = regenerate_markers(&plan.markers, &tree.marker_files)?;

    // Emptied scope dirs (their packages went stale) get removed once their
    // marker is gone; anything non-Forest inside keeps the dir alive.
    remove_empty_scope_dirs(&mount)?;

    for warning in super::lint_root_function_defs(&project.content_dir) {
        warn(&warning);
    }

    // Discovery happens at project open: only tell the user to reopen when
    // this run actually changed something the editor hasn't seen.
    let changed = !rec.to_install.is_empty() || !rec.stale_dirs.is_empty() || markers_changed;
    if changed && super::is_editor_running(&mount)? {
        info("UEFN appears to be running: reopen the project, then Build Verse Code, so it picks up the changes.");
    }

    Ok(InstallSummary { installed: rec.to_install.len(), kept: rec.kept })
}

/// Write every desired marker; delete-by-header any previously scanned marker
/// file the plan no longer wants. Returns whether anything changed on disk.
fn regenerate_markers(
    desired: &[super::plan::PlannedMarker],
    existing_markers: &[String],
) -> Result<bool> {
    let desired_paths: std::collections::HashSet<&str> =
        desired.iter().map(|m| m.file.as_str()).collect();

    let mut changed = false;
    for stale in existing_markers.iter().filter(|f| !desired_paths.contains(f.as_str())) {
        let path = Path::new(stale);
        // Re-verify the header at deletion time - the only license to
        // remove a .verse file.
        if path.exists() && super::is_forest_marker(path) {
            fs::remove_file(path).with_context(|| format!("Failed to remove marker {}", stale))?;
            changed = true;
        }
    }
    for marker in desired {
        let path = Path::new(&marker.file);
        let current = fs::read_to_string(path).ok();
        if current.as_deref() != Some(marker.content.as_str()) {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, &marker.content)
                .with_context(|| format!("Failed to write marker {}", marker.file))?;
            changed = true;
        }
    }
    Ok(changed)
}

fn remove_empty_scope_dirs(mount: &Path) -> Result<()> {
    let Ok(entries) = fs::read_dir(mount) else { return Ok(()) };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Ok(mut contents) = fs::read_dir(&path) {
                if contents.next().is_none() {
                    fs::remove_dir(&path)?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::plan::PlannedMarker;
    use super::*;

    fn fixture(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("forest-uefn-exec-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn markers_regenerate_and_delete_by_header_only() {
        let base = fixture("markers");
        let old_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(&base).unwrap();

        fs::create_dir_all("ForestPackages/old_scope").unwrap();
        // A stale Forest marker and a user file that must survive.
        fs::write(
            "ForestPackages/old_scope.verse",
            crate::uefn::marker_content("old_scope", &crate::uefn::MarkerAccess::Public),
        )
        .unwrap();
        fs::write("ForestPackages/UserFile.verse", "# mine\nF<public>():int = 1\n").unwrap();

        let desired = vec![PlannedMarker {
            file: "./ForestPackages/new_scope.verse".to_string(),
            content: crate::uefn::marker_content("new_scope", &crate::uefn::MarkerAccess::Public),
        }];
        let existing = vec![
            "./ForestPackages/old_scope.verse".to_string(),
            // UserFile.verse is never in this list (scan only collects
            // header-carrying files), but even if it were, the deletion-time
            // header re-check protects it.
            "./ForestPackages/UserFile.verse".to_string(),
        ];
        let changed = regenerate_markers(&desired, &existing).unwrap();

        assert!(changed);
        assert!(!Path::new("ForestPackages/old_scope.verse").exists(), "stale marker deleted");
        assert!(Path::new("ForestPackages/UserFile.verse").exists(), "user file untouched");
        assert!(Path::new("ForestPackages/new_scope.verse").exists());

        // Second run: no changes.
        let changed_again = regenerate_markers(&desired, &[]).unwrap();
        assert!(!changed_again, "idempotent when nothing differs");

        std::env::set_current_dir(old_cwd).unwrap();
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn empty_scope_dirs_are_removed_nonempty_kept() {
        let base = fixture("scopes");
        let mount = base.join("ForestPackages");
        fs::create_dir_all(mount.join("empty_scope")).unwrap();
        fs::create_dir_all(mount.join("full_scope")).unwrap();
        fs::write(mount.join("full_scope").join("keep.txt"), "x").unwrap();

        remove_empty_scope_dirs(&mount).unwrap();

        assert!(!mount.join("empty_scope").exists());
        assert!(mount.join("full_scope").exists());
        let _ = fs::remove_dir_all(&base);
    }
}
