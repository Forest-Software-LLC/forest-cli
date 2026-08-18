//! Roblox platform module: everything specific to the hoisted `Packages/`
//! tree with pointer `init.lua` shims and folder-module (init-rename)
//! extraction. Reached only through the `Platform` seam (src/platform.rs);
//! core modules never import from here.

pub mod extract;
pub mod init;
pub mod install;
pub mod plan;
pub mod publish;
pub mod receipts;
pub mod type_link;
pub mod wally;

/// The default Roblox install mount, relative to the manifest directory.
/// A manifest can rename its own mount via `packagesDir`; read the effective
/// name through `packages_container`, never this constant.
pub const PACKAGES_DIR: &str = "Packages";

/// The consumer's dependency container name: the manifest's `packagesDir`
/// when set, else `Packages`. The planner, physical mapping, receipt scan,
/// and top-level prune must all derive their root prefix from this, or a
/// mismatch reinstalls everything or prunes the whole mount.
pub fn packages_container(manifest: &serde_json::Value) -> String {
    manifest
        .get("packagesDir")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(PACKAGES_DIR)
        .to_string()
}

/// The install mount relative to the manifest dir, derived from the
/// manifest's `root` and container name: `<parent-of-root>/<container>` for
/// a nested root (root "src/init.luau" -> "src/Packages"), plain
/// `<container>` when there is no root or it sits at the top level. Deps
/// then live inside the package's own dir, mirroring the installed layout
/// (`Packages/Knit/Packages/...`). Forward slashes always; tolerates
/// backslash roots from manifests published on Windows before publish
/// normalized separators.
pub fn packages_base(manifest: &serde_json::Value) -> String {
    let container = packages_container(manifest);
    let root = manifest
        .get("root")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let root = root.replace('\\', "/");
    let root = root.strip_prefix("./").unwrap_or(&root);
    match root.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => format!("{}/{}", parent, container),
        _ => container,
    }
}

/// Map a plan-format path (`./<container>/...`) to its physical location
/// under `base`. Plan/receipt/reconcile strings stay base-agnostic so the
/// planning layer never learns where the mount physically sits; `container`
/// must come from the same manifest as `base` (see `packages_container`).
pub fn physical_path(base: &str, container: &str, plan_path: &str) -> std::path::PathBuf {
    let virtual_prefix = format!("./{}", container);
    match plan_path.strip_prefix(&virtual_prefix) {
        Some("") => std::path::PathBuf::from(base),
        Some(rest) if rest.starts_with('/') => {
            std::path::PathBuf::from(base).join(rest.trim_start_matches('/'))
        }
        _ => std::path::PathBuf::from(plan_path),
    }
}

/// `packagesDir` rule, shared by init, publish preflight, and install:
/// `^[A-Za-z][A-Za-z0-9_-]*$`, max 64 chars, Windows reserved device names
/// rejected case-insensitively. The letter start excludes path-traversal
/// characters and the cleanup-exempt `_`/`.` prefixes. Install validates
/// too because registry values flow into filesystem paths.
pub fn validate_packages_dir(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Dependency folder name cannot be empty.".to_string());
    }
    if name.len() > 64 {
        return Err("Dependency folder name cannot be longer than 64 characters.".to_string());
    }
    let mut chars = name.chars();
    if !chars.next().map_or(false, |c| c.is_ascii_alphabetic()) {
        return Err("Dependency folder name must start with a letter.".to_string());
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(
            "Dependency folder name may only contain letters, numbers, underscores, and hyphens."
                .to_string(),
        );
    }
    const WINDOWS_RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if WINDOWS_RESERVED.contains(&name.to_ascii_uppercase().as_str()) {
        return Err(format!(
            "Dependency folder name '{}' is a reserved Windows device name.",
            name
        ));
    }
    Ok(())
}

/// Does `start` look like a Roblox project? Signals: a Rojo
/// `default.project.json`, a Wally `wally.toml`, or any `*.project.json`,
/// all checked in the directory ITSELF only. No ancestor walk: a stray
/// wally.toml anywhere up the tree (home dir, drive root) would otherwise
/// poison detection for every project on the machine, and a wrong platform
/// guess is far worse than falling back to the picker.
pub fn detect_project(start: &std::path::Path) -> bool {
    if start.join("default.project.json").is_file() || start.join("wally.toml").is_file() {
        return true;
    }
    std::fs::read_dir(start)
        .map(|entries| {
            entries.flatten().any(|e| {
                e.path().is_file()
                    && e.file_name().to_string_lossy().ends_with(".project.json")
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    #[test]
    fn packages_base_defaults_without_a_nested_root() {
        assert_eq!(packages_base(&json!({})), "Packages");
        assert_eq!(packages_base(&json!({ "root": "init.luau" })), "Packages");
        assert_eq!(packages_base(&json!({ "root": "" })), "Packages");
    }

    #[test]
    fn packages_base_follows_the_root_parent() {
        assert_eq!(packages_base(&json!({ "root": "src/init.luau" })), "src/Packages");
        assert_eq!(packages_base(&json!({ "root": "a/b/init.lua" })), "a/b/Packages");
    }

    #[test]
    fn packages_base_normalizes_legacy_root_shapes() {
        // Roots published from Windows before separator normalization.
        assert_eq!(packages_base(&json!({ "root": "src\\init.luau" })), "src/Packages");
        assert_eq!(packages_base(&json!({ "root": "./src/init.luau" })), "src/Packages");
        assert_eq!(packages_base(&json!({ "root": "./init.luau" })), "Packages");
    }

    #[test]
    fn physical_path_maps_plan_paths_under_the_base() {
        assert_eq!(physical_path("Packages", "Packages", "./Packages"), PathBuf::from("Packages"));
        assert_eq!(
            physical_path("Packages", "Packages", "./Packages/Knit/Packages/Comm"),
            PathBuf::from("Packages").join("Knit").join("Packages").join("Comm")
        );
        assert_eq!(physical_path("src/Packages", "Packages", "./Packages"), PathBuf::from("src/Packages"));
        assert_eq!(
            physical_path("src/Packages", "Packages", "./Packages/Knit"),
            PathBuf::from("src/Packages").join("Knit")
        );
    }

    #[test]
    fn physical_path_maps_a_renamed_container() {
        assert_eq!(
            physical_path("roblox_packages", "roblox_packages", "./roblox_packages"),
            PathBuf::from("roblox_packages")
        );
        assert_eq!(
            physical_path("src/roblox_packages", "roblox_packages", "./roblox_packages/Knit"),
            PathBuf::from("src/roblox_packages").join("Knit")
        );
        // A mismatched prefix must not map under the base.
        assert_eq!(
            physical_path("src/roblox_packages", "roblox_packages", "./Packages/Knit"),
            PathBuf::from("./Packages/Knit")
        );
    }

    #[test]
    fn packages_container_reads_the_manifest_field() {
        assert_eq!(packages_container(&json!({})), "Packages");
        assert_eq!(packages_container(&json!({ "packagesDir": "" })), "Packages");
        assert_eq!(
            packages_container(&json!({ "packagesDir": "roblox_packages" })),
            "roblox_packages"
        );
    }

    #[test]
    fn packages_base_honors_a_custom_container() {
        assert_eq!(
            packages_base(&json!({ "packagesDir": "roblox_packages" })),
            "roblox_packages"
        );
        assert_eq!(
            packages_base(&json!({ "packagesDir": "roblox_packages", "root": "src/init.luau" })),
            "src/roblox_packages"
        );
    }

    #[test]
    fn packages_dir_rule_accepts_sane_names_only() {
        assert!(validate_packages_dir("Packages").is_ok());
        assert!(validate_packages_dir("roblox_packages").is_ok());
        assert!(validate_packages_dir("my-packages").is_ok());
        assert!(validate_packages_dir(&"a".repeat(64)).is_ok(), "64 chars is the ceiling");

        assert!(validate_packages_dir("").is_err());
        assert!(validate_packages_dir(&"a".repeat(65)).is_err());
        assert!(validate_packages_dir("..").is_err());
        assert!(validate_packages_dir("a/b").is_err());
        assert!(validate_packages_dir("a\\b").is_err());
        assert!(validate_packages_dir("_lead").is_err(), "cleanup-exempt prefix");
        assert!(validate_packages_dir(".lead").is_err(), "cleanup-exempt prefix");
        assert!(validate_packages_dir("1pkg").is_err(), "must start with a letter");
        assert!(validate_packages_dir("CON").is_err(), "Windows device name");
        assert!(validate_packages_dir("con").is_err(), "device names reject case-insensitively");
        assert!(validate_packages_dir("Com5").is_err());
        assert!(validate_packages_dir("lpt9").is_err());
        assert!(validate_packages_dir("COM10").is_ok(), "only COM1-COM9 are reserved");
    }
}
