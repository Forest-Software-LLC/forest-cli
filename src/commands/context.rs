//! Shared command preamble: find the project manifest and load it.

use std::fs;
use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::platform::Platform;

pub struct Project {
    pub manifest: Value,
    pub platform: Platform,
}

/// Enter the manifest directory and load forest.json. A local forest.json
/// wins; otherwise the platform seam may find one nearby (UEFN keeps it
/// inside Content/). Ok(None) when no manifest exists anywhere; the caller
/// decides whether that is fatal. Prints where it relocated to, so call it
/// before starting a spinner (or with the spinner paused).
pub fn load_project() -> Result<Option<Project>> {
    if !Path::new("forest.json").exists() {
        if let Some(manifest_dir) = crate::platform::discover_manifest_dir(&std::env::current_dir()?) {
            std::env::set_current_dir(&manifest_dir)?;
            crate::message::info(&format!(
                "Using manifest at {}",
                manifest_dir.join("forest.json").display()
            ));
        }
    }
    if !Path::new("forest.json").exists() {
        return Ok(None);
    }
    let manifest: Value = serde_json::from_str(&fs::read_to_string("forest.json")?)?;
    let platform = Platform::from_manifest(&manifest)?;
    Ok(Some(Project { manifest, platform }))
}
