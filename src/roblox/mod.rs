//! Roblox platform module: everything specific to the hoisted `Packages/`
//! tree with pointer `init.lua` shims and folder-module (init-rename)
//! extraction. Reached only through the `Platform` seam (src/platform.rs);
//! core modules never import from here.

pub mod extract;
pub mod init;
pub mod install;
pub mod link_overlay;
pub mod mount_dirs;
pub mod plan;
pub mod publish;
pub mod receipts;
pub mod scratch;
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
    match crate::utils::manifest_root_parent(root) {
        Some(parent) => format!("{}/{}", parent, container),
        None => container,
    }
}

/// `packages_base`, checked the way extra mount paths are: `packagesDir`
/// must be a plain folder name and the whole path must stay inside the
/// project. An install empties the mount's top level, so a `root` or
/// `packagesDir` that climbs out of the project would empty an outside
/// folder.
pub fn checked_packages_base(manifest: &serde_json::Value) -> anyhow::Result<String> {
    crate::mounts::validate_folder_name(&packages_container(manifest))
        .map_err(|reason| anyhow::anyhow!("Invalid packagesDir in forest.json: {}", reason))?;
    let base = packages_base(manifest);
    crate::mounts::normalize_mount_path(&base).map_err(|reason| {
        anyhow::anyhow!(
            "The dependency folder {} (from root and packagesDir in forest.json) is not valid: {}",
            base,
            reason
        )
    })
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

/// The default mount's folder name for `forest mount rename`: `packagesDir`,
/// written only when it differs from the default so manifests stay minimal.
pub fn set_packages_container(manifest: &mut serde_json::Value, name: &str) {
    if name == PACKAGES_DIR {
        if let Some(obj) = manifest.as_object_mut() {
            obj.remove("packagesDir");
        }
    } else {
        manifest["packagesDir"] = serde_json::Value::String(name.to_string());
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
    fn checked_packages_base_stays_inside_the_project() {
        assert_eq!(checked_packages_base(&json!({})).unwrap(), "Packages");
        assert_eq!(checked_packages_base(&json!({ "root": "./src/init.luau" })).unwrap(), "src/Packages");
        // An install empties the folder, so none of these may reach one.
        for manifest in [
            json!({ "packagesDir": "../victim" }),
            json!({ "packagesDir": "a/b" }),
            json!({ "packagesDir": "C:/Users" }),
            json!({ "root": "../other/init.luau" }),
            json!({ "root": "/abs/init.luau" }),
        ] {
            assert!(checked_packages_base(&manifest).is_err(), "{} must be rejected", manifest);
        }
    }

    #[test]
    fn default_container_is_written_as_absence() {
        let mut manifest = json!({ "packagesDir": "Deps" });
        set_packages_container(&mut manifest, "Packages");
        assert!(manifest.get("packagesDir").is_none());
        set_packages_container(&mut manifest, "Deps");
        assert_eq!(manifest["packagesDir"], "Deps");
    }
}
