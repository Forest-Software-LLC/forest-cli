//! Roblox `forest init` scaffolds. Package mode (the default) starts
//! development on a new package: name + root prompts, a starter root module,
//! `root` written into forest.json, and the Packages mount created INSIDE
//! the root dir (e.g. `src/Packages/`) — the same place a consumer's install
//! puts a package's deps, so `script.Packages.X` requires work identically
//! in development and when installed. Project mode (`--project`, and
//! install's create-on-install path) writes the bare consuming manifest.
//! When a `wally.toml` is present, both modes offer to convert from Wally by
//! importing its dependencies (they resolve as-is: every wally package is
//! mirrored on the Forest registry under the same scope/name). Reached only
//! through the `Platform` seam.

use anyhow::Result;
use dialoguer::{theme::ColorfulTheme, Input};
use serde_json::{json, Map, Value};
use std::{fs, path::Path};

use crate::message::{info, success, warn};
use crate::platform::InitMode;

pub fn init(cwd: &Path, mode: InitMode) -> Result<()> {
    if cwd.join("forest.json").exists() {
        warn("forest.json already exists in the current directory. Please remove it before initializing a new project.");
        return Ok(());
    }

    // Wally conversion offer: pull the deps straight into forest.json.
    let mut dependencies = Map::new();
    let mut license: Option<String> = None;
    let wally_path = cwd.join("wally.toml");
    if wally_path.is_file() {
        let convert = dialoguer::Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Found wally.toml. Import its dependencies into forest.json? (Wally packages are mirrored on Forest, so they resolve as-is.)")
            .default(0)
            .items(&["Yes", "No"])
            .interact();
        // "No" or a non-interactive terminal: plain scaffold.
        if matches!(convert, Ok(0)) {
            match fs::read_to_string(&wally_path)
                .map_err(anyhow::Error::from)
                .and_then(|text| crate::roblox::wally::parse_wally_manifest(&text))
            {
                Ok(import) => {
                    for skipped in &import.skipped_malformed {
                        warn(&format!("wally.toml: skipped {}", skipped));
                    }
                    for dep in &import.dependencies {
                        let value = match &dep.alias {
                            Some(alias) => json!({ "version": dep.version, "alias": alias }),
                            None => Value::String(dep.version.clone()),
                        };
                        dependencies.insert(dep.full_name.clone(), value);
                    }
                    license = import.license;
                    let mut summary = format!(
                        "Imported {} dependencies from wally.toml.",
                        import.dependencies.len()
                    );
                    if import.skipped_dev > 0 {
                        summary.push_str(&format!(
                            " ({} dev-dependencies skipped: forest manifests don't model them yet.)",
                            import.skipped_dev
                        ));
                    }
                    info(&summary);
                }
                Err(err) => warn(&format!(
                    "Could not read wally.toml ({}). Initializing without imported dependencies.",
                    err
                )),
            }
        }
    }

    match mode {
        InitMode::Project { from_install } => {
            scaffold_project(cwd, dependencies, license)?;
            success(&format!("Initialized a new project in {}", cwd.display()));
            if !from_install {
                info("You can now run `forest install` to install dependencies!");
            }
        }
        InitMode::Package => {
            // Both prompts `?`-propagate: package authoring is inherently
            // interactive; scripts and non-TTY callers use `--project`.
            let theme = ColorfulTheme::default();
            let mut name_prompt = Input::with_theme(&theme)
                .with_prompt("Package name")
                .validate_with(|input: &String| {
                    crate::roblox::publish::validate_package_name(input)
                        .map_err(|reason| anyhow::anyhow!(reason))
                });
            if let Some(dir_name) = cwd
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|n| crate::roblox::publish::validate_package_name(n).is_ok())
            {
                name_prompt = name_prompt.default(dir_name);
            }
            let name: String = name_prompt.interact_text()?;

            let root: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("Path to the package's root file (created if missing)")
                .default(default_root(cwd))
                .validate_with(|input: &String| {
                    validate_root(input).map_err(|reason| anyhow::anyhow!(reason))
                })
                .interact_text()?;
            let root = normalize_root(&root);

            let manifest = scaffold_package(cwd, &name, &root, dependencies, license)?;

            success(&format!("Initialized package \"{}\" in {}", name, cwd.display()));
            info(&format!(
                "Dependencies will install to {}/, next to your root file.",
                crate::roblox::packages_base(&manifest)
            ));
            info("Run `forest install <scope>/<name>` to add dependencies, and `forest publish` when you're ready to share!");
        }
    }

    Ok(())
}

/// Bare consuming manifest + the top-level Packages mount (install's
/// create-on-install scaffold; must never prompt).
fn scaffold_project(cwd: &Path, dependencies: Map<String, Value>, license: Option<String>) -> Result<()> {
    let mut manifest = json!({
        "dependencies": dependencies,
        "platform": "roblox",
    });
    if let Some(license) = license {
        manifest["license"] = Value::String(license);
    }

    let packages_dir = cwd.join(crate::roblox::PACKAGES_DIR);
    if !packages_dir.exists() {
        fs::create_dir_all(&packages_dir)?;
    }
    fs::write(cwd.join("forest.json"), serde_json::to_string_pretty(&manifest)?)?;
    Ok(())
}

/// Package-authoring scaffold: manifest with name/version/root, a starter
/// root module (only when the file doesn't exist yet), and the Packages
/// mount inside the root dir. Promptless so it's unit-testable; returns the
/// manifest it wrote. `root` must already be normalized (forward slashes).
fn scaffold_package(
    cwd: &Path,
    name: &str,
    root: &str,
    dependencies: Map<String, Value>,
    license: Option<String>,
) -> Result<Value> {
    let mut manifest = json!({
        "name": name,
        "version": "0.1.0",
        "dependencies": dependencies,
        "platform": "roblox",
        "root": root,
    });
    if let Some(license) = license {
        manifest["license"] = Value::String(license);
    }

    let root_path = cwd.join(root);
    if !root_path.exists() {
        if let Some(parent) = root_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let ident = crate::roblox::publish::pascal_ident(name);
        fs::write(
            &root_path,
            format!(
                "--!strict\n-- {} entry point. Everything in this folder ships with `forest publish`.\n\nlocal {} = {{}}\n\nreturn {}\n",
                name, ident, ident
            ),
        )?;
    }

    let packages_dir = cwd.join(crate::roblox::packages_base(&manifest));
    if !packages_dir.exists() {
        fs::create_dir_all(&packages_dir)?;
    }

    fs::write(cwd.join("forest.json"), serde_json::to_string_pretty(&manifest)?)?;
    Ok(manifest)
}

/// Root-prompt default: an existing top-level init.luau/init.lua wins over
/// the src/init.luau convention.
fn default_root(cwd: &Path) -> String {
    for candidate in ["init.luau", "init.lua"] {
        if cwd.join(candidate).is_file() {
            return candidate.to_string();
        }
    }
    "src/init.luau".to_string()
}

/// Forward slashes, no leading `./` — the form `root` is stored in.
fn normalize_root(input: &str) -> String {
    let normalized = input.trim().replace('\\', "/");
    normalized.strip_prefix("./").unwrap_or(&normalized).to_string()
}

/// Root-prompt rule: a relative `.luau`/`.lua` path inside the package.
/// Existence is NOT required — init creates the file.
fn validate_root(input: &str) -> std::result::Result<(), String> {
    let normalized = normalize_root(input);
    if normalized.is_empty() {
        return Err("Path cannot be empty".to_string());
    }
    if !(normalized.ends_with(".luau") || normalized.ends_with(".lua")) {
        return Err("Root file must end in .luau or .lua".to_string());
    }
    let path = Path::new(&normalized);
    // has_root() as well: on Windows "/abs" is rootful yet not "absolute".
    if path.is_absolute() || path.has_root() || normalized.contains(':') {
        return Err("Path must be relative to the package directory".to_string());
    }
    if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err("Path cannot leave the package directory (..)".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("forest-rbx-init-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn package_scaffold_writes_manifest_starter_and_nested_mount() {
        let base = fixture("pkg");
        let manifest = scaffold_package(&base, "MyPkg", "src/init.luau", Map::new(), None).unwrap();

        let on_disk: Value =
            serde_json::from_str(&fs::read_to_string(base.join("forest.json")).unwrap()).unwrap();
        assert_eq!(on_disk, manifest);
        assert_eq!(on_disk["name"], "MyPkg");
        assert_eq!(on_disk["version"], "0.1.0");
        assert_eq!(on_disk["platform"], "roblox");
        assert_eq!(on_disk["root"], "src/init.luau", "root stays forward-slashed");

        let starter = fs::read_to_string(base.join("src").join("init.luau")).unwrap();
        assert!(starter.contains("return MyPkg"));
        assert!(
            base.join("src").join("Packages").is_dir(),
            "mount lives inside the root dir, mirroring the installed layout"
        );
        assert!(!base.join("Packages").exists(), "no top-level mount in package mode");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn package_scaffold_never_overwrites_an_existing_root_file() {
        let base = fixture("keep-root");
        fs::create_dir_all(base.join("src")).unwrap();
        fs::write(base.join("src").join("init.luau"), "return \"existing\"").unwrap();

        scaffold_package(&base, "MyPkg", "src/init.luau", Map::new(), None).unwrap();

        assert_eq!(
            fs::read_to_string(base.join("src").join("init.luau")).unwrap(),
            "return \"existing\""
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn package_scaffold_pascals_hyphenated_names_in_the_starter() {
        let base = fixture("hyphen");
        scaffold_package(&base, "nav-mesh", "init.luau", Map::new(), None).unwrap();

        let starter = fs::read_to_string(base.join("init.luau")).unwrap();
        assert!(starter.contains("local NavMesh = {}"), "hyphens aren't valid in Luau identifiers");
        assert!(base.join("Packages").is_dir(), "top-level root mounts at the top level");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn project_scaffold_stays_bare() {
        let base = fixture("proj");
        scaffold_project(&base, Map::new(), None).unwrap();

        let manifest: Value =
            serde_json::from_str(&fs::read_to_string(base.join("forest.json")).unwrap()).unwrap();
        assert_eq!(manifest["platform"], "roblox");
        assert!(manifest["dependencies"].as_object().unwrap().is_empty());
        assert!(manifest.get("name").is_none(), "project manifests carry no package identity");
        assert!(manifest.get("root").is_none());
        assert!(base.join("Packages").is_dir());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn root_rule_accepts_creatable_relative_paths_only() {
        assert!(validate_root("src/init.luau").is_ok());
        assert!(validate_root("init.lua").is_ok());
        assert!(validate_root("./src/init.luau").is_ok());
        assert!(validate_root("src\\init.luau").is_ok(), "windows input normalizes");
        assert!(validate_root("").is_err());
        assert!(validate_root("src/main.rs").is_err(), "must be a luau/lua file");
        assert!(validate_root("../outside/init.luau").is_err());
        assert!(validate_root("C:/abs/init.luau").is_err());
        assert!(validate_root("/abs/init.luau").is_err());
    }

    #[test]
    fn default_root_prefers_an_existing_top_level_init() {
        let base = fixture("default-root");
        assert_eq!(default_root(&base), "src/init.luau");

        fs::write(base.join("init.lua"), "return {}").unwrap();
        assert_eq!(default_root(&base), "init.lua");

        fs::write(base.join("init.luau"), "return {}").unwrap();
        assert_eq!(default_root(&base), "init.luau", "luau wins over lua");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn normalize_root_produces_the_stored_form() {
        assert_eq!(normalize_root("src\\init.luau"), "src/init.luau");
        assert_eq!(normalize_root("./src/init.luau"), "src/init.luau");
        assert_eq!(normalize_root(" init.luau "), "init.luau");
    }
}
