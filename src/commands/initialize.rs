use anyhow::Result;
use std::env;

use crate::message::warn;
use crate::platform::{InitMode, Platform};

/// Start development on a new Forest package (or, with `--project`, scaffold
/// a bare consuming manifest).
///
/// `platform` lets callers skip the interactive picker (e.g. `forest init
/// --platform roblox`), keeping `init` scriptable. `project` selects the
/// bare project scaffold — the same one install's create-on-install path
/// uses. `packages_dir` is the `--packages-dir` flag (Roblox only). What
/// actually gets scaffolded is wholly platform-owned (roblox/init.rs,
/// uefn/init.rs).
pub async fn init_command(platform: Option<String>, project: bool, packages_dir: Option<String>) -> Result<()> {
    let cwd = env::current_dir()?;
    let platform = match platform {
        Some(p) => match Platform::parse(&p) {
            Ok(platform) => platform,
            Err(_) => {
                warn(&format!(
                    "Invalid platform '{}'. Supported platforms: roblox, uefn.",
                    p
                ));
                return Ok(());
            }
        },
        None => Platform::detect_or_prompt(&cwd)?,
    };

    let mode = if project { InitMode::Project { from_install: false } } else { InitMode::Package };
    platform.init(&cwd, mode, packages_dir.as_deref()).await
}
