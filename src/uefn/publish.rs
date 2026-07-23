//! UEFN publish preflight: Verse naming rules, the compatVersion metadata,
//! and the pre-pack lint for files the gateway will reject. Reached only
//! through the `Platform` seam.

use anyhow::Result;
use ignore::gitignore::Gitignore;
use serde_json::Value;
use std::path::Path;
use walkdir::WalkDir;

use crate::message::warn;
use crate::platform::Preflight;

/// No entry point on UEFN: the folder IS the package (archiveRoot '').
/// A pre-existing manifest name must satisfy the Verse rules the name
/// prompt enforces for new names; the authored-against UEFN version travels
/// in the upload metadata (display/warn only, registry-side).
pub fn publish_preflight(cwd: &Path, forest_json: &mut Value, metadata: &mut Value) -> Result<Preflight> {
    // Position guard: inside a UEFN project, publish is only legal from a
    // package position (<Mount>/<scope>/<name>). Anywhere else (Content/,
    // Src/, the mount itself) the manifest describes the PROJECT, and
    // packing would ship the whole Content tree. Outside any project stays
    // allowed: publishing from a bare package repo checkout (e.g. CI) is a
    // supported workflow.
    if super::find_project(cwd).is_some() && !super::ancestor_is_mount(cwd, 2) {
        let mount = &crate::contracts::verse_rules().packages_mount;
        return Ok(Preflight::Abort(format!(
            "This is the project, not a package. Publishing here would upload the entire Content \
             tree. Run `forest publish` inside {}/<scope>/<name>, or scaffold a package with \
             `forest init` inside {}.",
            mount, mount
        )));
    }

    if let Some(name) = forest_json["name"].as_str() {
        if let Err(reason) = super::validate_uefn_package_name(name) {
            return Ok(Preflight::Abort(reason));
        }
    }
    match super::find_project(cwd).and_then(|p| super::read_compat_version(&p)) {
        Some(compat) => metadata["compatVersion"] = Value::String(compat),
        None => warn("Could not read the project's compatibilityVersion (not inside a UEFN project?); publishing without it."),
    }
    Ok(Preflight::Continue)
}

/// Folder-scope consistency: inside a project, the package's parent scope
/// folder must be the mapped form of the scope it's being published under -
/// otherwise every locally-written reference (the author's test code,
/// sibling packages' imports) names a path that won't exist for consumers.
/// The init wizard enforces this at creation; this closes the same gap at
/// the publish end (the author picker runs after preflight). Outside a
/// project there's no folder to check - publish authorization remains the
/// server-side enforcement.
pub fn validate_author(cwd: &Path, author: &str) -> Result<(), String> {
    if super::find_project(cwd).is_none() || !super::ancestor_is_mount(cwd, 2) {
        return Ok(());
    }
    let scope_dir = cwd.parent().map(super::dir_name).unwrap_or_default();
    let mapped = super::map_scope_to_verse_identifier(&author.to_lowercase());
    if mapped != scope_dir {
        return Err(format!(
            "This package lives in \"{}/\" but you're publishing as \"{}\" (whose scope folder is \"{}/\"). \
             Move the package to the matching scope folder, or publish under the scope this folder belongs to.",
            scope_dir, author, mapped
        ));
    }
    Ok(())
}

/// Pre-pack lint: files the gateway will reject (Epic digest files, binary
/// UE assets) surfaced as warnings before any bytes are uploaded. Only files
/// the ignore matcher would actually pack are reported.
pub fn prepack_warnings(dir: &Path, matcher: &Gitignore) -> Vec<String> {
    let mut warnings = Vec::new();
    for entry in WalkDir::new(dir).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(rel) = entry.path().strip_prefix(dir).ok() else { continue };
        if matcher.matched(rel, false).is_ignore() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name.ends_with(".digest.verse") {
            warnings.push(format!(
                "{} is an Epic-generated digest file; the registry will reject it. Add it to .forestignore.",
                rel.display()
            ));
        } else if name.ends_with(".uasset") || name.ends_with(".umap") {
            warnings.push(format!(
                "{} is a binary UE asset; UEFN packages are Verse-code-only and the registry will reject it.",
                rel.display()
            ));
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Preflight;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    fn project_fixture(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("forest-pubguard-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("Content").join("ForestPackages").join("myscope").join("MyPkg")).unwrap();
        fs::write(base.join("Island.uefnproject"), r#"{ "compatibilityVersion": "41.20" }"#).unwrap();
        base
    }

    fn preflight_at(cwd: &Path) -> Preflight {
        let mut manifest = json!({ "platform": "uefn", "dependencies": {} });
        let mut metadata = json!({});
        publish_preflight(cwd, &mut manifest, &mut metadata).unwrap()
    }

    #[test]
    fn project_positions_abort_package_position_continues() {
        let base = project_fixture("positions");
        let content = base.join("Content");

        for bad in [content.clone(), content.join("ForestPackages"), content.join("ForestPackages").join("myscope")] {
            match preflight_at(&bad) {
                Preflight::Abort(reason) => assert!(reason.contains("not a package"), "{}", reason),
                Preflight::Continue => panic!("{} must not be publishable", bad.display()),
            }
        }

        let pkg = content.join("ForestPackages").join("myscope").join("MyPkg");
        assert!(matches!(preflight_at(&pkg), Preflight::Continue));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn author_must_match_the_parent_scope_folder() {
        let base = project_fixture("author-match");
        let pkg = base.join("Content").join("ForestPackages").join("myscope").join("MyPkg");

        assert!(validate_author(&pkg, "myscope").is_ok());
        // Kebab scopes match through the identifier mapping.
        let mapped_pkg = base.join("Content").join("ForestPackages").join("cool_studio").join("MyPkg");
        fs::create_dir_all(&mapped_pkg).unwrap();
        assert!(validate_author(&mapped_pkg, "cool-studio").is_ok());

        let err = validate_author(&pkg, "someone-else").unwrap_err();
        assert!(err.contains("myscope"), "{}", err);
        assert!(err.contains("someone-else"), "{}", err);

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn author_check_is_skipped_outside_projects() {
        let base = std::env::temp_dir().join(format!("forest-author-bare-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        assert!(validate_author(&base, "anyone").is_ok(), "bare repo checkouts have no folder to check");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn outside_any_project_stays_publishable() {
        // Bare package repo checkout (e.g. CI): no .uefnproject anywhere.
        let base = std::env::temp_dir().join(format!("forest-pubguard-bare-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        assert!(matches!(preflight_at(&base), Preflight::Continue));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn package_position_still_sets_compat_version() {
        let base = project_fixture("compat");
        let pkg = base.join("Content").join("ForestPackages").join("myscope").join("MyPkg");
        let mut manifest = json!({ "platform": "uefn", "name": "MyPkg", "dependencies": {} });
        let mut metadata = json!({});
        assert!(matches!(publish_preflight(&pkg, &mut manifest, &mut metadata).unwrap(), Preflight::Continue));
        assert_eq!(metadata["compatVersion"], "41.20");
        let _ = fs::remove_dir_all(&base);
    }
}
