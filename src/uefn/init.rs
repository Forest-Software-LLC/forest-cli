//! UEFN `forest init` scaffolds, inferred from where the command runs
//! (docs/uefn-adapter.md §7 authoring workflow). Reached only through the
//! `Platform` seam.

use anyhow::Result;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};

use crate::message::{info, success, warn};

/// What a `forest init -p uefn` should scaffold at `cwd`.
enum UefnInitTarget {
    /// cwd is `<Mount>/<Scope>/<Name>` inside a project: package scaffold.
    Package { name: String },
    /// Project manifest at `dir` (Content/, possibly descended into).
    Project { dir: PathBuf, found_project: bool },
}

fn infer_uefn_target(cwd: &Path) -> UefnInitTarget {
    let mount_name = &crate::contracts::verse_rules().packages_mount;

    // Package position: .../<Mount>/<Scope>/<Name> == cwd, inside a real project.
    if super::find_project(cwd).is_some() {
        let is_package_pos = cwd
            .ancestors()
            .nth(2) // grandparent should be the mount dir
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy() == *mount_name)
            .unwrap_or(false);
        if is_package_pos {
            let name = cwd
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            return UefnInitTarget::Package { name };
        }
    }

    // Project position: cwd has the .uefnproject, so the manifest goes in
    // Content/; cwd IS Content/ (or any other dir): manifest goes here.
    if let Some(project) = super::find_project(cwd) {
        if project.project_root == cwd {
            return UefnInitTarget::Project { dir: project.content_dir, found_project: true };
        }
        return UefnInitTarget::Project { dir: cwd.to_path_buf(), found_project: true };
    }
    UefnInitTarget::Project { dir: cwd.to_path_buf(), found_project: false }
}

pub fn init(cwd: &Path) -> Result<()> {
    match infer_uefn_target(cwd) {
        UefnInitTarget::Package { name } => {
            if cwd.join("forest.json").exists() {
                warn("forest.json already exists here. Please remove it before initializing.");
                return Ok(());
            }
            // The folder name becomes the Verse module identifier consumers
            // import - it must be valid as-is (never auto-renamed).
            if let Err(reason) = super::validate_uefn_package_name(&name) {
                warn(&format!(
                    "{} Package folders become Verse module identifiers - rename the folder and re-run.",
                    reason
                ));
                return Ok(());
            }

            let manifest = json!({
                "name": name,
                "version": "0.1.0",
                "dependencies": serde_json::Map::<String, serde_json::Value>::new(),
                "platform": "uefn",
            });
            fs::write(cwd.join("forest.json"), serde_json::to_string_pretty(&manifest)?)?;
            fs::write(
                cwd.join("README.md"),
                format!("# {}\n\nA Forest package for UEFN.\n", name),
            )?;
            // Starter module: one exported function, in the exact form the
            // compiler-verified evidence log used.
            fs::write(
                cwd.join(format!("{}.verse", name)),
                format!("Hello<public>():void =\n    Print(\"Hello from {}\")\n", name),
            )?;

            success(&format!("Initialized package \"{}\" in {}", name, cwd.display()));
            info("Author your Verse here, then run `forest publish` from this folder.");
        }
        UefnInitTarget::Project { dir, found_project } => {
            if dir.join("forest.json").exists() {
                warn(&format!(
                    "forest.json already exists at {}. Please remove it before initializing.",
                    dir.display()
                ));
                return Ok(());
            }
            if !found_project {
                warn("No *.uefnproject found in this directory or any parent - initializing here anyway. UEFN manifests normally live in the project's Content folder.");
            }
            if !dir.exists() {
                fs::create_dir_all(&dir)?;
            }

            let manifest = json!({
                "dependencies": serde_json::Map::<String, serde_json::Value>::new(),
                "platform": "uefn",
            });
            fs::write(dir.join("forest.json"), serde_json::to_string_pretty(&manifest)?)?;
            // No directory pre-creation: `forest install` creates the mount.

            success(&format!("Initialized a new UEFN project manifest in {}", dir.display()));
            info("You can now run `forest install <scope>/<name>` to add packages!");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("forest-init-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    fn make_project(base: &Path) {
        fs::create_dir_all(base.join("Content")).unwrap();
        fs::write(base.join("Island.uefnproject"), "{}").unwrap();
    }

    #[test]
    fn infer_package_position_inside_mount() {
        let base = fixture("pkg-pos");
        make_project(&base);
        let pkg = base.join("Content").join("ForestPackages").join("myscope").join("MyPkg");
        fs::create_dir_all(&pkg).unwrap();
        match infer_uefn_target(&pkg) {
            UefnInitTarget::Package { name } => assert_eq!(name, "MyPkg"),
            _ => panic!("expected package target"),
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn infer_project_root_descends_to_content() {
        let base = fixture("proj-root");
        make_project(&base);
        match infer_uefn_target(&base) {
            UefnInitTarget::Project { dir, found_project } => {
                assert!(found_project);
                assert_eq!(dir, base.join("Content"));
            }
            _ => panic!("expected project target"),
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn infer_content_dir_stays_put() {
        let base = fixture("content-pos");
        make_project(&base);
        let content = base.join("Content");
        match infer_uefn_target(&content) {
            UefnInitTarget::Project { dir, found_project } => {
                assert!(found_project);
                assert_eq!(dir, content);
            }
            _ => panic!("expected project target"),
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn infer_bare_dir_warns_but_targets_cwd() {
        let base = fixture("bare");
        match infer_uefn_target(&base) {
            UefnInitTarget::Project { dir, found_project } => {
                assert!(!found_project);
                assert_eq!(dir, base);
            }
            _ => panic!("expected project target"),
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn package_scaffold_writes_manifest_readme_and_starter() {
        let base = fixture("scaffold");
        make_project(&base);
        let pkg = base.join("Content").join("ForestPackages").join("myscope").join("MyPkg");
        fs::create_dir_all(&pkg).unwrap();

        init(&pkg).unwrap();

        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(pkg.join("forest.json")).unwrap()).unwrap();
        assert_eq!(manifest["name"], "MyPkg");
        assert_eq!(manifest["platform"], "uefn");
        assert_eq!(manifest["version"], "0.1.0");
        assert!(manifest.get("root").is_none(), "uefn packages have no entry point");
        assert!(pkg.join("README.md").exists());
        let starter = fs::read_to_string(pkg.join("MyPkg.verse")).unwrap();
        assert!(starter.contains("Hello<public>()"));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn package_scaffold_rejects_invalid_folder_name() {
        let base = fixture("bad-name");
        make_project(&base);
        let pkg = base.join("Content").join("ForestPackages").join("myscope").join("bad-name");
        fs::create_dir_all(&pkg).unwrap();

        init(&pkg).unwrap();
        assert!(!pkg.join("forest.json").exists(), "invalid folder name must not scaffold");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn project_scaffold_creates_no_directories() {
        let base = fixture("proj-scaffold");
        make_project(&base);

        init(&base).unwrap();

        let manifest_path = base.join("Content").join("forest.json");
        assert!(manifest_path.exists());
        assert!(!base.join("Content").join("Packages").exists());
        assert!(!base.join("Content").join("ForestPackages").exists(), "install creates the mount, not init");
        let _ = fs::remove_dir_all(&base);
    }
}
