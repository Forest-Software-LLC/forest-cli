//! UEFN publish preflight: Verse naming rules, the compatVersion metadata,
//! and the pre-pack lint for files the gateway will reject. Reached only
//! through the `Platform` seam.

use anyhow::Result;
use ignore::gitignore::Gitignore;
use serde_json::Value;
use std::path::Path;
use walkdir::WalkDir;

use crate::message::warn;
use crate::platform::Preflight;

/// No entry point on UEFN: the folder IS the package (archiveRoot '').
/// A pre-existing manifest name must satisfy the Verse rules the name
/// prompt enforces for new names; the authored-against UEFN version travels
/// in the upload metadata (display/warn only, registry-side).
pub fn publish_preflight(cwd: &Path, forest_json: &mut Value, metadata: &mut Value) -> Result<Preflight> {
    if let Some(name) = forest_json["name"].as_str() {
        if let Err(reason) = super::validate_uefn_package_name(name) {
            return Ok(Preflight::Abort(reason));
        }
    }
    match super::find_project(cwd).and_then(|p| super::read_compat_version(&p)) {
        Some(compat) => metadata["compatVersion"] = Value::String(compat),
        None => warn("Could not read the project's compatibilityVersion (not inside a UEFN project?) - publishing without it."),
    }
    Ok(Preflight::Continue)
}

/// Pre-pack lint: files the gateway will reject (Epic digest files, binary
/// UE assets) surfaced as warnings before any bytes are uploaded. Only files
/// the ignore matcher would actually pack are reported.
pub fn prepack_warnings(dir: &Path, matcher: &Gitignore) -> Vec<String> {
    let mut warnings = Vec::new();
    for entry in WalkDir::new(dir).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(rel) = entry.path().strip_prefix(dir).ok() else { continue };
        if matcher.matched(rel, false).is_ignore() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name.ends_with(".digest.verse") {
            warnings.push(format!(
                "{} is an Epic-generated digest file - the registry will reject it. Add it to .forestignore.",
                rel.display()
            ));
        } else if name.ends_with(".uasset") || name.ends_with(".umap") {
            warnings.push(format!(
                "{} is a binary UE asset - UEFN packages are Verse-code-only and the registry will reject it.",
                rel.display()
            ));
        }
    }
    warnings
}
