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

/// The Roblox install mount, relative to the manifest directory.
pub const PACKAGES_DIR: &str = "Packages";

/// The Packages mount relative to the manifest dir, derived from the
/// manifest's `root`: `<parent-of-root>/Packages` for a nested root (root
/// "src/init.luau" -> "src/Packages"), plain "Packages" when there is no
/// root or it sits at the top level. Deps then live inside the package's own
/// dir, mirroring the installed layout (`Packages/Knit/Packages/...`).
/// Forward slashes always; tolerates backslash roots from manifests
/// published on Windows before publish normalized separators.
pub fn packages_base(manifest: &serde_json::Value) -> String {
    let root = manifest
        .get("root")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let root = root.replace('\\', "/");
    let root = root.strip_prefix("./").unwrap_or(&root);
    match root.rsplit_once('/') {
        Some((parent, _)) if !parent.is_empty() => format!("{}/{}", parent, PACKAGES_DIR),
        _ => PACKAGES_DIR.to_string(),
    }
}

/// Map a plan-format path (`./Packages/...`) to its physical location under
/// `base`. Plan/receipt/reconcile strings stay base-agnostic (`./Packages`)
/// so the planning layer never learns where the mount physically sits.
pub fn physical_path(base: &str, plan_path: &str) -> std::path::PathBuf {
    let virtual_prefix = format!("./{}", PACKAGES_DIR);
    match plan_path.strip_prefix(&virtual_prefix) {
        Some("") => std::path::PathBuf::from(base),
        Some(rest) if rest.starts_with('/') => {
            std::path::PathBuf::from(base).join(rest.trim_start_matches('/'))
        }
        _ => std::path::PathBuf::from(plan_path),
    }
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
        assert_eq!(physical_path("Packages", "./Packages"), PathBuf::from("Packages"));
        assert_eq!(
            physical_path("Packages", "./Packages/Knit/Packages/Comm"),
            PathBuf::from("Packages").join("Knit").join("Packages").join("Comm")
        );
        assert_eq!(physical_path("src/Packages", "./Packages"), PathBuf::from("src/Packages"));
        assert_eq!(
            physical_path("src/Packages", "./Packages/Knit"),
            PathBuf::from("src/Packages").join("Knit")
        );
    }
}
