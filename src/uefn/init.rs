//! UEFN `forest init` scaffolds, inferred from where the command runs
//! (docs/uefn-adapter.md §7 authoring workflow). Reached only through the
//! `Platform` seam.
//!
//! Position triage:
//!   `<Mount>/<Scope>/<Name>`  -> package scaffold, name from the folder
//!   `<Mount>/<Scope>`         -> NEW package here: prompt for the name
//!   `<Mount>`                 -> NEW package: pick a scope (your account /
//!                                studios, same as the publish author list),
//!                                then a name
//!   project root / Content    -> project manifest (+ the mount dir)
//!   anywhere else             -> project manifest with a warning

use anyhow::Result;
use dialoguer::{theme::ColorfulTheme, Input, Select};
use reqwest::Method;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

use crate::http::api_request;
use crate::message::{info, success, warn};

/// What a `forest init -p uefn` should scaffold at `cwd`.
enum UefnInitTarget {
    /// cwd is `<Mount>/<Scope>/<Name>`: package scaffold, name from folder.
    Package { name: String },
    /// cwd is the mount or a scope dir: create a new package underneath.
    /// `scope_dir` is known when cwd is already a scope directory.
    NewPackage { scope_dir: Option<String> },
    /// Project manifest at `dir` (Content/, possibly descended into).
    Project { dir: PathBuf, found_project: bool },
}

use super::{ancestor_is_mount, dir_name};

fn infer_uefn_target(cwd: &Path) -> UefnInitTarget {
    let mount_name = &crate::contracts::verse_rules().packages_mount;

    if super::find_project(cwd).is_some() {
        // Deepest first: <Mount>/<Scope>/<Name> beats <Mount>/<Scope> beats <Mount>.
        if ancestor_is_mount(cwd, 2) {
            return UefnInitTarget::Package { name: dir_name(cwd) };
        }
        if ancestor_is_mount(cwd, 1) {
            return UefnInitTarget::NewPackage { scope_dir: Some(dir_name(cwd)) };
        }
        if dir_name(cwd) == *mount_name {
            return UefnInitTarget::NewPackage { scope_dir: None };
        }
    }

    if let Some(project) = super::find_project(cwd) {
        if project.project_root == cwd {
            return UefnInitTarget::Project { dir: project.content_dir, found_project: true };
        }
        return UefnInitTarget::Project { dir: cwd.to_path_buf(), found_project: true };
    }
    UefnInitTarget::Project { dir: cwd.to_path_buf(), found_project: false }
}

pub async fn init(cwd: &Path) -> Result<()> {
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
                    "{} Package folders become Verse module identifiers. Rename the folder and re-run.",
                    reason
                ));
                return Ok(());
            }
            scaffold_package(cwd, &name)?;
        }
        UefnInitTarget::NewPackage { scope_dir } => {
            // Creating a package claims a scope folder, so the scope must be
            // one the user can actually publish under - which requires being
            // logged in.
            let Some(scopes) = fetch_author_scopes().await else {
                warn("Creating a UEFN package needs a logged-in account so the scope can be verified. Run `forest login`, then re-run `forest init`.");
                return Ok(());
            };
            let scope_dir = match scope_dir {
                Some(existing) => {
                    let legit = scopes
                        .iter()
                        .any(|s| super::map_scope_to_verse_identifier(s) == existing);
                    if !legit {
                        warn(&format!(
                            "\"{}\" doesn't match a scope you can publish under ({}). Create the package under one of your scopes instead.",
                            existing,
                            scopes.join(", ")
                        ));
                        return Ok(());
                    }
                    existing
                }
                None => {
                    let mut labels = scopes.clone();
                    labels[0] = format!("{} (You)", labels[0]);
                    let selection = Select::with_theme(&ColorfulTheme::default())
                        .with_prompt("Scope for the new package")
                        .default(0)
                        .items(&labels)
                        .interact()?;
                    super::map_scope_to_verse_identifier(&scopes[selection])
                }
            };

            let name: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("Package name")
                .validate_with(|input: &String| {
                    super::validate_uefn_package_name(input).map_err(|reason| anyhow::anyhow!(reason))
                })
                .interact_text()?;

            let package_dir = if dir_name(cwd) == scope_dir {
                cwd.join(&name)
            } else {
                cwd.join(&scope_dir).join(&name)
            };
            if package_dir.exists() {
                warn(&format!("{} already exists. Pick another name or initialize inside it.", package_dir.display()));
                return Ok(());
            }
            fs::create_dir_all(&package_dir)?;
            scaffold_package(&package_dir, &name)?;
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
                warn("No *.uefnproject found in this directory or any parent; initializing here anyway. UEFN manifests normally live in the project's Content folder.");
            }
            if !dir.exists() {
                fs::create_dir_all(&dir)?;
            }

            let manifest = json!({
                "dependencies": serde_json::Map::<String, serde_json::Value>::new(),
                "platform": "uefn",
            });
            fs::write(dir.join("forest.json"), serde_json::to_string_pretty(&manifest)?)?;
            // Pre-create the mount so authoring a package is one `cd` away.
            let mount = dir.join(&crate::contracts::verse_rules().packages_mount);
            if !mount.exists() {
                fs::create_dir_all(&mount)?;
            }

            success(&format!("Initialized a new UEFN project manifest in {}", dir.display()));
            info("You can now run `forest install <scope>/<name>` to add packages!");
        }
    }
    Ok(())
}

/// Write the standard package scaffold into an existing directory.
fn scaffold_package(dir: &Path, name: &str) -> Result<()> {
    let manifest = json!({
        "name": name,
        "version": "0.1.0",
        "dependencies": serde_json::Map::<String, serde_json::Value>::new(),
        "platform": "uefn",
    });
    fs::write(dir.join("forest.json"), serde_json::to_string_pretty(&manifest)?)?;
    fs::write(dir.join("README.md"), format!("# {}\n\nA Forest package for UEFN.\n", name))?;
    // Starter module: one exported function, in the exact form the
    // compiler-verified evidence log used.
    fs::write(
        dir.join(format!("{}.verse", name)),
        format!("Hello<public>():void =\n    Print(\"Hello from {}\")\n", name),
    )?;

    success(&format!("Initialized package \"{}\" in {}", name, dir.display()));
    info("Author your Verse here, then run `forest publish` from this folder.");
    Ok(())
}

/// Scopes the logged-in user may publish under: their username plus studios
/// where they're admin/owner (the same author list the publish flow offers).
/// None when not authenticated (or the registry is unreachable) - creating
/// a package requires a verifiable scope, so there is deliberately no
/// free-text fallback.
async fn fetch_author_scopes() -> Option<Vec<String>> {
    let (session, status) = api_request("v1/auth/session", Method::GET, None, None).await.ok()?;
    if !status.is_success() {
        return None;
    }
    let user = session.get("username").and_then(Value::as_str)?.to_string();
    let mut scopes = vec![user.clone()];
    if let Ok((userdata, org_status)) =
        api_request(&format!("v1/user/{}", user), Method::GET, None, None).await
    {
        if org_status.is_success() {
            for org in userdata.get("orgs").and_then(Value::as_array).cloned().unwrap_or_default() {
                let name = org.get("name").and_then(Value::as_str);
                let rank = org.get("rank").and_then(Value::as_str);
                if let (Some(name), Some(rank)) = (name, rank) {
                    if rank == "admin" || rank == "owner" {
                        scopes.push(name.to_string());
                    }
                }
            }
        }
    }
    Some(scopes)
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
    fn infer_scope_dir_is_new_package_with_known_scope() {
        let base = fixture("scope-pos");
        make_project(&base);
        let scope = base.join("Content").join("ForestPackages").join("myscope");
        fs::create_dir_all(&scope).unwrap();
        match infer_uefn_target(&scope) {
            UefnInitTarget::NewPackage { scope_dir } => assert_eq!(scope_dir.as_deref(), Some("myscope")),
            _ => panic!("expected new-package target"),
        }
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn infer_mount_is_new_package_with_scope_to_pick() {
        let base = fixture("mount-pos");
        make_project(&base);
        let mount = base.join("Content").join("ForestPackages");
        fs::create_dir_all(&mount).unwrap();
        match infer_uefn_target(&mount) {
            UefnInitTarget::NewPackage { scope_dir } => assert!(scope_dir.is_none()),
            _ => panic!("expected new-package target"),
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
        let pkg = base.join("MyPkg");
        fs::create_dir_all(&pkg).unwrap();

        scaffold_package(&pkg, "MyPkg").unwrap();

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

    #[tokio::test]
    async fn package_position_rejects_invalid_folder_name() {
        let base = fixture("bad-name");
        make_project(&base);
        let pkg = base.join("Content").join("ForestPackages").join("myscope").join("bad-name");
        fs::create_dir_all(&pkg).unwrap();

        init(&pkg).await.unwrap();
        assert!(!pkg.join("forest.json").exists(), "invalid folder name must not scaffold");
        let _ = fs::remove_dir_all(&base);
    }

    #[tokio::test]
    async fn project_scaffold_creates_the_mount_but_not_packages() {
        let base = fixture("proj-scaffold");
        make_project(&base);

        init(&base).await.unwrap();

        let content = base.join("Content");
        assert!(content.join("forest.json").exists());
        assert!(content.join("ForestPackages").exists(), "the mount is pre-created so authoring is one cd away");
        assert!(!content.join("Packages").exists(), "no Roblox mount on a UEFN project");
        let _ = fs::remove_dir_all(&base);
    }
}
