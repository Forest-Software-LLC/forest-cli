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
pub mod wally;

/// The Roblox install mount, relative to the manifest directory.
pub const PACKAGES_DIR: &str = "Packages";

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
