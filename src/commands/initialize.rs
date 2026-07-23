use anyhow::Result;
use std::env;

use crate::message::warn;
use crate::platform::Platform;

/// Initialize a new Forest project or package.
///
/// `platform` lets callers skip the interactive picker (e.g. `forest init
/// --platform roblox`). When `None`, we prompt as before. This keeps `init`
/// scriptable/agent-friendly without an interactive terminal. What actually
/// gets scaffolded is wholly platform-owned (roblox/init.rs, uefn/init.rs).
pub async fn init_command(platform: Option<String>) -> Result<()> {
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
        None => Platform::detect_or_prompt(&env::current_dir()?)?,
    };

    platform.init(&env::current_dir()?)
}
