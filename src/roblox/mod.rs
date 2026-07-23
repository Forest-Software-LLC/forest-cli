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
/// `default.project.json` or a Wally `wally.toml` in the directory or any
/// ancestor, or any `*.project.json` directly in the directory.
pub fn detect_project(start: &std::path::Path) -> bool {
    let mut dir = Some(start);
    while let Some(current) = dir {
        if current.join("default.project.json").is_file() || current.join("wally.toml").is_file() {
            return true;
        }
        dir = current.parent();
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
