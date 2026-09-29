//! `forest link` / `forest unlink`: point a direct dependency at a local
//! working tree, machine-locally. State lives in gitignored
//! `.forest/links.json`; forest.json and forest-lock.json are never touched.
//! The overlay itself is applied by the platform install executor.

use std::fs;
use std::path::Path;

use anyhow::{anyhow, Result};
use serde_json::Value;
use walkdir::WalkDir;

use crate::lockfile::LockFile;
use crate::links;
use crate::message::{fail, info, success, warn};
use crate::mounts::Mount;
use crate::platform::Platform;
use crate::utils::same_package;

/// The lockfile-pinned root version for a dependency key in a mount.
fn pinned_version(lockfile: &Option<LockFile>, mount: &Mount, name: &str) -> Option<String> {
    lockfile
        .as_ref()
        .and_then(|lf| lf.section(mount))
        .and_then(|s| s.pinned_version(name).map(str::to_string))
}

/// How a mount is stored in links.json: None for the default.
fn link_mount_key(mount: &Mount) -> Option<&str> {
    (!mount.is_default()).then_some(mount.path.as_str())
}

pub async fn link_command(path: Option<String>, list: bool, mount: Option<String>) -> Result<()> {
    let Some(project) = super::context::load_project()? else {
        fail("No forest.json found. Run `forest init` first.");
        return Ok(());
    };
    let manifest = project.manifest;
    let platform = project.platform;
    let mounts = crate::mounts::project_mounts(&manifest, platform)?;
    let scope = mount.as_deref().map(|r| crate::mounts::find_mount(&mounts, r)).transpose()?;

    let Some(path) = path.filter(|_| !list) else {
        print_links_list(&mounts, platform);
        return Ok(());
    };

    if platform != Platform::Roblox {
        fail(&format!(
            "forest link is not supported on {} yet. UEFN packages can be authored directly inside the shared mount instead.",
            platform.display_name()
        ));
        return Ok(());
    }

    // The target must carry a manifest: it supplies the package's identity
    // and root; a bare folder is not linkable.
    let target = Path::new(&path);
    let target_manifest_path = target.join("forest.json");
    if !target.is_dir() {
        fail(&format!("{} is not a directory.{}", target.display(), backslash_hint(&path)));
        return Ok(());
    }
    if !target_manifest_path.is_file() {
        fail(&format!(
            "{} has no forest.json; only Forest packages can be linked.",
            target.display()
        ));
        return Ok(());
    }
    let linked: Value = serde_json::from_str(&fs::read_to_string(&target_manifest_path)?)
        .map_err(|e| anyhow!("Failed to parse {}: {}", target_manifest_path.display(), e))?;

    if let Some(linked_platform) = linked.get("platform").and_then(Value::as_str) {
        if Platform::parse(linked_platform).ok() != Some(platform) {
            fail(&format!(
                "{} is a {} package; this project is {}.",
                target.display(),
                linked_platform,
                platform.as_str()
            ));
            return Ok(());
        }
    }

    let Some(linked_name) = linked.get("name").and_then(Value::as_str) else {
        fail(&format!(
            "{} has no `name` field; publish the package once (or add name/author to its forest.json) before linking it.",
            target_manifest_path.display()
        ));
        return Ok(());
    };

    // Identity: author/name when the linked manifest knows its author,
    // otherwise fall back to an unambiguous name-part match. A link is an
    // OVERRIDE; the package must already be a direct dependency, of
    // exactly one mount unless --mount picks it.
    let searched: Vec<&Mount> = match scope {
        Some(m) => vec![m],
        None => mounts.iter().collect(),
    };
    let author = linked.get("author").and_then(Value::as_str);
    let full = author.map(|a| format!("{}/{}", a, linked_name));
    let candidates: Vec<(&Mount, String)> = searched
        .iter()
        .flat_map(|m| {
            m.deps
                .keys()
                .filter(|k| match &full {
                    Some(full) => same_package(k, full),
                    None => k.rsplit('/').next().map_or(false, |n| n.eq_ignore_ascii_case(linked_name)),
                })
                .map(move |k| (*m, k.clone()))
        })
        .collect();
    let (link_mount, dep_key) = match candidates.as_slice() {
        [(m, key)] => (*m, key.clone()),
        [] => {
            let place = scope.map(|m| format!(" of {}", m.path)).unwrap_or_else(|| " of this project".to_string());
            match &full {
                Some(full) => fail(&format!("{} is not a dependency{}. Add it first: `forest install {}`.", full, place, full)),
                None => fail(&format!("No dependency named {} in forest.json. Add the package first, then link it.", linked_name)),
            }
            return Ok(());
        }
        many if many.iter().all(|(_, k)| full.as_ref().map_or(false, |f| same_package(k, f))) => {
            anyhow::bail!(
                "{} is a dependency of several mounts: {}. Pick one with --mount.",
                many[0].1,
                crate::mounts::DepLocation::several_paths(many)
            );
        }
        many => {
            fail(&format!(
                "{} has no `author` field and \"{}\" matches several dependencies ({}). Add `author` to the linked forest.json.",
                target_manifest_path.display(),
                linked_name,
                many.iter().map(|(_, k)| k.as_str()).collect::<Vec<_>>().join(", ")
            ));
            return Ok(());
        }
    };

    // Range mismatch is a warning, not an error: dev versions legitimately
    // run ahead of the declared range.
    let linked_version = linked.get("version").and_then(Value::as_str).unwrap_or("");
    if let Some(spec) = crate::utils::get_ci(&link_mount.deps, &dep_key) {
        let satisfied = semver::VersionReq::parse(&spec.version)
            .ok()
            .zip(semver::Version::parse(linked_version).ok())
            .map(|(req, ver)| req.matches(&ver));
        if satisfied == Some(false) {
            warn(&format!(
                "Linked version {} does not satisfy the declared range {} for {}.",
                linked_version, spec.version, dep_key
            ));
        }
    }

    // Same heads-up a registry install gives for packaged scripts.
    warn_on_runnable_scripts(target, &linked);

    links::upsert_link(Path::new("."), &dep_key, &path, link_mount_key(link_mount))?;
    if links::ensure_gitignored(Path::new("."))? {
        info("Added .forest/ to .gitignore (link state is machine-local and must not be committed).");
    }

    // Apply immediately: the normal install pipeline picks the link up as an
    // overlay, restoring/keeping everything else registry-faithful. The
    // explicit mode makes `forest link` apply even where installs would
    // default to ignoring links (CI).
    super::install::install_command(
        None, None, None, false, None, Some(links::LinksMode::Apply), false, Some(link_mount.path.clone()),
    )
    .await?;

    let place = if mounts.len() > 1 { format!(" in {}", link_mount.path) } else { String::new() };
    success(&format!("Linked {} → {}{}", dep_key, path, place));
    Ok(())
}

/// A follow-up hint when `path` looks like a Windows path whose backslashes
/// were eaten by a POSIX shell: a drive-letter prefix with no separator
/// anywhere after it ("C:UsersthereDocuments..."). Unquoted `C:\Users\...`
/// in Git Bash delivers exactly that, because bash treats each backslash as
/// an escape character. Empty when the path doesn't match the shape.
fn backslash_hint(path: &str) -> &'static str {
    let bytes = path.as_bytes();
    let stripped = bytes.len() > 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && !path[2..].contains('/')
        && !path[2..].contains('\\');
    if stripped {
        " Your shell likely removed the backslashes (Git Bash does this to unquoted Windows paths); quote the path or use forward slashes, e.g. C:/Users/..."
    } else {
        ""
    }
}

/// Count Script/LocalScript sources in the linked root dir, mirroring
/// the warning a registry install prints.
fn warn_on_runnable_scripts(target: &Path, linked_manifest: &Value) {
    const RUNNABLE: [&str; 4] = [".server.lua", ".server.luau", ".client.lua", ".client.luau"];
    let root = linked_manifest
        .get("root")
        .and_then(Value::as_str)
        .unwrap_or("");
    let source_dir = match crate::utils::manifest_root_parent(root) {
        Some(parent) => target.join(parent),
        None => target.to_path_buf(),
    };
    let root_file = root.replace('\\', "/").rsplit('/').next().unwrap_or("").to_string();
    let count = WalkDir::new(&source_dir)
        .into_iter()
        .filter_entry(|e| e.file_name().to_str().map_or(true, |n| n != ".git"))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map_or(false, |n| RUNNABLE.iter().any(|s| n.ends_with(s)) && n != root_file)
        })
        .count();
    if count > 0 {
        warn(&format!(
            "The linked source contains {} Script/LocalScript file{} that can run in your place; review them if unexpected.",
            count,
            if count == 1 { "" } else { "s" }
        ));
    }
}

pub async fn unlink_command(reference: Option<String>, all: bool, mount: Option<String>) -> Result<()> {
    let Some(project) = super::context::load_project()? else {
        fail("No forest.json found here.");
        return Ok(());
    };
    let platform = project.platform;
    let mounts = crate::mounts::project_mounts(&project.manifest, platform)?;
    let stored_all = links::stored_links();
    // The mount a --mount flag picks, as a stored link records it (None =
    // default). It may name a folder forest.json no longer declares, to
    // clean up links into it; `forest link` lists those with that hint.
    let scope: Option<Option<String>> = match mount.as_deref() {
        None => None,
        Some(reference) => match crate::mounts::find_mount(&mounts, reference) {
            Ok(m) => Some((!m.is_default()).then(|| m.path.clone())),
            Err(e) => {
                let wanted = reference.trim().replace('\\', "/");
                let wanted = wanted.trim_end_matches('/');
                match stored_all.iter().filter_map(|l| l.mount.as_ref()).find(|p| p.eq_ignore_ascii_case(wanted)) {
                    Some(path) => Some(Some(path.clone())),
                    None => return Err(e),
                }
            }
        },
    };

    let stored: Vec<links::StoredLink> = stored_all
        .into_iter()
        .filter(|l| match (&scope, &l.mount) {
            (None, _) => true,
            (Some(None), None) => true,
            (Some(Some(want)), Some(have)) => want.eq_ignore_ascii_case(have),
            _ => false,
        })
        .collect();
    if stored.is_empty() {
        info("No active links.");
        return Ok(());
    }

    let removed: Vec<links::StoredLink> = if all {
        if scope.is_some() {
            for link in &stored {
                links::remove_stored(Path::new("."), link)?;
            }
            stored
        } else {
            links::remove_all(Path::new("."))?
        }
    } else {
        let Some(reference) = reference else {
            fail("Pass a package (scope/name), a linked path, or --all.");
            return Ok(());
        };
        match links::matching_links(Path::new("."), &stored, &reference).as_slice() {
            [link] => {
                links::remove_stored(Path::new("."), link)?;
                vec![link.clone()]
            }
            [] => {
                info(&format!(
                    "No active link matches {}; nothing to do.{}",
                    reference,
                    backslash_hint(&reference)
                ));
                return Ok(());
            }
            many => {
                let places: Vec<String> = many
                    .iter()
                    .map(|l| l.mount.clone().unwrap_or_else(|| mounts[0].path.clone()))
                    .collect();
                fail(&format!(
                    "{} is linked in several mounts: {}. Pick one with --mount.",
                    reference,
                    places.join(", ")
                ));
                return Ok(());
            }
        }
    };

    // Clear each slot so the reinstall below restores the registry version
    // (re-extracted from the verified cache). Junctions are removed as
    // links, never through them; copy-mode slots (real dirs) go through the
    // trash bin so a live rojo never sees in-place child deletions. A link
    // into a folder that is no longer a mount keeps its slot there until
    // this (or a full install) removes it.
    if platform == Platform::Roblox {
        let mut trash = crate::roblox::scratch::TrashBin::new(crate::roblox::scratch::scratch_dirs().trash);
        for link in &removed {
            let (base, container, spec) = match mounts.iter().find(|m| link.belongs_to(m)) {
                Some(m) => (m.path.clone(), m.name().to_string(), crate::utils::get_ci(&m.deps, &link.name)),
                None => match &link.mount {
                    Some(path) => (path.clone(), path.rsplit('/').next().unwrap_or(path).to_string(), None),
                    None => continue,
                },
            };
            if let Some(spec) = spec {
                let slot = crate::roblox::physical_path(
                    &base,
                    &container,
                    &crate::roblox::link_overlay::slot_plan_path(&container, &spec.alias),
                );
                crate::roblox::link_overlay::remove_slot(&slot, &mut trash)?;
            } else if let Ok(target) = fs::canonicalize(Path::new(&link.path)) {
                // Dep no longer declared: hunt down the orphaned link by its
                // target instead (copy-mode orphans are pruned by install).
                // The slot links the root PARENT, so probe the manifest's
                // root as well as the project dir itself.
                let mut candidates = vec![target.clone()];
                if let Some(parent) = fs::read_to_string(target.join("forest.json"))
                    .ok()
                    .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                    .and_then(|m| m.get("root").and_then(Value::as_str).map(str::to_string))
                    .and_then(|root| crate::utils::manifest_root_parent(&root))
                {
                    candidates.push(target.join(parent));
                }
                for candidate in candidates {
                    for slot in crate::roblox::link_overlay::find_slots_for_target(&base, &candidate) {
                        crate::roblox::link_overlay::remove_slot(&slot, &mut trash)?;
                    }
                }
            }
        }
    }

    for link in &removed {
        let place = match &link.mount {
            Some(path) => format!(" in {}", path),
            None => String::new(),
        };
        info(&format!("Unlinked {}{} (was → {})", link.name, place, link.path));
    }

    // Restore the exact registry versions via the normal pipeline. Apply
    // mode so any REMAINING links stay materialized while this one restores.
    super::install::install_command(None, None, None, false, None, Some(links::LinksMode::Apply), false, None).await?;
    success(&format!(
        "Restored {} package{} from the registry.",
        removed.len(),
        if removed.len() == 1 { "" } else { "s" }
    ));
    Ok(())
}

/// `forest link --list` / bare `forest link`.
fn print_links_list(mounts: &[Mount], platform: Platform) {
    let stored = links::stored_links();
    if stored.is_empty() {
        info("No active links. Link one with `forest link <path>`.");
        return;
    }
    let lockfile = LockFile::load();
    let roblox = platform == Platform::Roblox;

    println!(
        "{} link{} active:",
        stored.len(),
        if stored.len() == 1 { "" } else { "s" }
    );
    for link in &stored {
        let flag = link.mount.as_ref().map(|p| format!(" --mount {}", p)).unwrap_or_default();
        let Some(link_mount) = mounts.iter().find(|m| link.belongs_to(m)) else {
            warn(&format!(
                "  {} → {} (mount {} is no longer declared; `forest unlink {}{}` to clean up)",
                link.name,
                link.path,
                link.mount.as_deref().unwrap_or_default(),
                link.name,
                flag
            ));
            continue;
        };
        let Some((dep_key, spec)) = link_mount.deps.iter().find(|(k, _)| same_package(k, &link.name)) else {
            warn(&format!(
                "  {} → {} (no longer a dependency; `forest unlink {}{}` to clean up)",
                link.name, link.path, link.name, flag
            ));
            continue;
        };
        let place = if mounts.len() > 1 { format!(" in {}", link_mount.path) } else { String::new() };
        let (base, container) = if roblox {
            (link_mount.path.clone(), link_mount.name().to_string())
        } else {
            (String::new(), String::new())
        };
        let linked_manifest: Option<Value> = fs::read_to_string(Path::new(&link.path).join("forest.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok());
        let linked_version = linked_manifest
            .as_ref()
            .and_then(|m| m.get("version").and_then(Value::as_str))
            .unwrap_or("?")
            .to_string();
        let pin = pinned_version(&lockfile, link_mount, dep_key).unwrap_or_else(|| "not installed".to_string());

        let mode = if base.is_empty() {
            String::new()
        } else {
            let slot = crate::roblox::physical_path(
                &base,
                &container,
                &crate::roblox::link_overlay::slot_plan_path(&container, &spec.alias),
            );
            if crate::roblox::link_overlay::is_link_dir(&slot) {
                ", live link".to_string()
            } else if slot.is_dir() {
                ", copy mode".to_string()
            } else {
                ", not applied, run `forest install`".to_string()
            }
        };
        println!(
            "  {} → {}{} (registry pin: {}, linked: {}{})",
            dep_key, link.path, place, pin, linked_version, mode
        );
        if linked_manifest.is_none() {
            warn(&format!("    target manifest unreadable at {}", link.path));
            continue;
        }

        // Dependency divergence vs the pinned registry version.
        if let Some(section) = lockfile.as_ref().and_then(|lf| lf.section(link_mount)) {
            let pinned_deps = section
                .root_entry(dep_key)
                .map(|e| e.dependencies.clone())
                .unwrap_or_default();
            let linked_deps = linked_manifest
                .as_ref()
                .map(crate::utils::manifest_dep_ranges)
                .unwrap_or_default();
            let probe = links::ActiveLink {
                name: dep_key.clone(),
                alias: spec.alias.clone(),
                path_display: link.path.clone(),
                source_dir: std::path::PathBuf::new(),
                root: String::new(),
                version: linked_version,
                dependencies: linked_deps,
            };
            for diff in links::dep_divergences(&probe, &pinned_deps) {
                println!("    dependency drift: {}", diff);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backslash_hint_fires_only_on_the_stripped_shape() {
        // Unquoted C:\Users\... through Git Bash arrives with every
        // backslash removed.
        assert!(!backslash_hint("C:UsersthereDocumentsGitHubvaultSharedCleaner").is_empty());
        assert!(!backslash_hint("d:projectspkg").is_empty());

        // Real paths, relative or absolute, any separator: no hint.
        assert!(backslash_hint(r"C:\Users\me\pkg").is_empty());
        assert!(backslash_hint("C:/Users/me/pkg").is_empty());
        assert!(backslash_hint("../knit-dev").is_empty());
        assert!(backslash_hint("pkg").is_empty());
        assert!(backslash_hint("/c/Users/me/pkg").is_empty());
        // Bare drive roots are too short to judge.
        assert!(backslash_hint("C:").is_empty());
    }
}
