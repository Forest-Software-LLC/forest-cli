//! The platform seam.
//!
//! Every platform-divergent behavior in the CLI is reached through the
//! `Platform` enum's methods; each arm is a one-line delegation into that
//! platform's module (`src/roblox/`, `src/uefn/`). Commands and core modules
//! never branch on platform strings themselves.
//!
//! DEPENDENCY RULE: core modules (solver, lockfile, http, cache, receipts,
//! fetch) must never import from `roblox/` or `uefn/`. Platform modules
//! import core, not each other.
//!
//! RIP-OUT PROCEDURE for a platform: delete its `src/<platform>/` directory
//! and its variant below. The compiler's exhaustiveness errors then list
//! every remaining touchpoint; when they're fixed and the other platforms'
//! tests pass, removal is complete. See ARCHITECTURE.md.

use anyhow::{anyhow, Result};
use ignore::gitignore::Gitignore;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::lockfile_gen::{InstallSummary, LockFile};
use crate::lockfile_solver::DepSpec;

/// Outcome of a platform's publish preflight.
pub enum Preflight {
    Continue,
    /// Abort with a user-facing reason (printed via `fail`).
    Abort(String),
}

/// Manifest discovery for commands run outside the manifest directory: asks
/// each platform in turn whether it knows where the manifest lives relative
/// to `start`. Only UEFN relocates manifests today (into the project's
/// Content folder); Roblox manifests are always at the cwd.
pub fn discover_manifest_dir(start: &Path) -> Option<PathBuf> {
    crate::uefn::discover_manifest_dir(start)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Roblox,
    Uefn,
}

impl Platform {
    /// Every supported platform, in picker order.
    pub const ALL: [Platform; 2] = [Platform::Roblox, Platform::Uefn];

    /// Display casing for pickers and messages.
    pub fn display_name(&self) -> &'static str {
        match self {
            Platform::Roblox => "Roblox",
            Platform::Uefn => "UEFN",
        }
    }

    /// Interactive platform picker (shared by `forest init` and the
    /// create-on-install offer). Errors in non-interactive terminals.
    pub fn prompt() -> Result<Platform> {
        let names: Vec<&str> = Platform::ALL.iter().map(|p| p.display_name()).collect();
        let selection = dialoguer::Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
            .with_prompt("Platform (Use arrow keys to navigate)")
            .items(&names)
            .default(0)
            .interact()?;
        Ok(Platform::ALL[selection])
    }

    /// Does `dir` look like one of this platform's projects?
    fn detects(&self, dir: &Path) -> bool {
        match self {
            Platform::Roblox => crate::roblox::detect_project(dir),
            Platform::Uefn => crate::uefn::detect_project(dir),
        }
    }

    /// Autodetect the platform from the surrounding project. Conclusive only
    /// when EXACTLY ONE platform recognizes the directory; zero or multiple
    /// matches return None (caller falls back to the picker).
    pub fn detect(dir: &Path) -> Option<Platform> {
        let mut matches = Platform::ALL.iter().filter(|p| p.detects(dir));
        match (matches.next(), matches.next()) {
            (Some(&platform), None) => Some(platform),
            _ => None,
        }
    }

    /// Autodetect with an info line, prompting only when inconclusive.
    pub fn detect_or_prompt(dir: &Path) -> Result<Platform> {
        match Platform::detect(dir) {
            Some(platform) => {
                crate::message::info(&format!("Detected a {} project.", platform.display_name()));
                Ok(platform)
            }
            None => Platform::prompt(),
        }
    }

    pub fn parse(value: &str) -> Result<Platform> {
        match value.trim().to_lowercase().as_str() {
            "roblox" => Ok(Platform::Roblox),
            "uefn" => Ok(Platform::Uefn),
            other => Err(anyhow!(
                "Unknown platform '{}' in forest.json. Supported platforms: roblox, uefn.",
                other
            )),
        }
    }

    pub fn from_manifest(manifest: &Value) -> Result<Platform> {
        let raw = manifest
            .get("platform")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Missing platform in forest.json"))?;
        Platform::parse(raw)
    }

    /// The registry path segment / manifest value for this platform.
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::Roblox => "roblox",
            Platform::Uefn => "uefn",
        }
    }

    /// The roots dependency resolution runs against. Roblox resolves the
    /// invoking manifest's deps; UEFN resolves the WORKSPACE (project
    /// manifest + every authored package's manifest) so a package's declared
    /// deps land at the shared mount no matter where install is run from.
    pub fn resolution_roots(
        &self,
        manifest_roots: HashMap<String, DepSpec>,
    ) -> Result<HashMap<String, DepSpec>> {
        match self {
            Platform::Roblox => Ok(manifest_roots),
            Platform::Uefn => {
                match std::env::current_dir().ok().and_then(|cwd| crate::uefn::find_project(&cwd)) {
                    Some(project) => crate::uefn::gather_workspace_roots(&project),
                    // Outside a project: the executor gives the real error;
                    // fall back so resolution still reports something useful.
                    None => Ok(manifest_roots),
                }
            }
        }
    }

    /// Execute an install plan: layout, extraction, bookkeeping, and
    /// post-install UX are wholly owned by the platform module.
    pub async fn install(
        &self,
        lockfile: &LockFile,
        root_deps: HashMap<String, DepSpec>,
        force: bool,
    ) -> Result<InstallSummary> {
        match self {
            Platform::Roblox => crate::roblox::install::make_directories_roblox(lockfile, root_deps, force).await,
            Platform::Uefn => crate::uefn::install::make_directories_uefn(lockfile, root_deps, force).await,
        }
    }

    /// Publish preparation before anything is packed: entry-point/root
    /// resolution (Roblox), name-rule enforcement and compat metadata (UEFN).
    pub fn publish_preflight(
        &self,
        cwd: &Path,
        forest_json: &mut Value,
        metadata: &mut Value,
    ) -> Result<Preflight> {
        match self {
            Platform::Roblox => crate::roblox::publish::publish_preflight(cwd, forest_json),
            Platform::Uefn => crate::uefn::publish::publish_preflight(cwd, forest_json, metadata),
        }
    }

    /// Package-name rule for NEW names (existing registry names are
    /// grandfathered server-side).
    pub fn validate_package_name(&self, name: &str) -> std::result::Result<(), String> {
        match self {
            Platform::Roblox => crate::roblox::publish::validate_package_name(name),
            Platform::Uefn => crate::uefn::validate_uefn_package_name(name),
        }
    }

    /// Consistency check between the chosen publish author and where the
    /// package physically sits (UEFN: the parent scope folder must be the
    /// author's mapped scope). Runs after the author is known.
    pub fn validate_publish_author(&self, cwd: &Path, author: &str) -> std::result::Result<(), String> {
        match self {
            Platform::Roblox => Ok(()),
            Platform::Uefn => crate::uefn::publish::validate_author(cwd, author),
        }
    }

    /// Non-fatal naming advice (e.g. Roblox's hyphen/dot-indexing note).
    pub fn name_advisory(&self, name: &str) -> Option<String> {
        match self {
            Platform::Roblox => crate::roblox::publish::name_advisory(name),
            Platform::Uefn => None, // hyphens are hard-rejected, nothing to advise
        }
    }

    /// Pre-pack warnings for files the registry will reject.
    pub fn prepack_warnings(&self, cwd: &Path, matcher: &Gitignore) -> Vec<String> {
        match self {
            Platform::Roblox => Vec::new(),
            Platform::Uefn => crate::uefn::publish::prepack_warnings(cwd, matcher),
        }
    }

    /// The `forest init` scaffold for this platform. Async because some
    /// scaffolds consult the registry (UEFN's scope picker offers the same
    /// author list as publish).
    pub async fn init(&self, cwd: &Path) -> Result<()> {
        match self {
            Platform::Roblox => crate::roblox::init::init(cwd),
            Platform::Uefn => crate::uefn::init::init(cwd).await,
        }
    }

    /// Why `-a` is rejected on this platform, if it is.
    pub fn alias_error(&self) -> Option<String> {
        match self {
            Platform::Roblox => None,
            Platform::Uefn => Some(crate::uefn::alias_error()),
        }
    }

    /// The note printed when a typed package id resolves to different
    /// canonical casing.
    pub fn resolved_note(&self, typed: &str, canonical: &str, resolved_name: &str) -> String {
        match self {
            Platform::Roblox => format!(
                "{} resolved to {} (require(\"{}\"))",
                typed, canonical, resolved_name
            ),
            Platform::Uefn => format!("{} resolved to {}", typed, canonical),
        }
    }

    /// Post-add usage snippet, when the platform has one.
    pub fn added_note(&self, scope: &str, name: &str) -> Option<String> {
        match self {
            Platform::Roblox => None,
            Platform::Uefn => Some(crate::uefn::install_snippet(scope, name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("forest-detect-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn detects_roblox_from_rojo_or_wally_files() {
        let base = fixture("roblox");
        fs::write(base.join("default.project.json"), "{}").unwrap();
        // Roblox signals are cwd-ONLY: a subdirectory does not detect (an
        // ancestor walk would let stray wally.toml/default.project.json
        // files high up the tree poison every detection on the machine).
        let sub = base.join("src");
        fs::create_dir_all(&sub).unwrap();
        assert_eq!(Platform::detect(&base), Some(Platform::Roblox));
        assert_eq!(Platform::detect(&sub), None);

        let wally = fixture("wally");
        fs::write(wally.join("wally.toml"), "[package]").unwrap();
        assert_eq!(Platform::detect(&wally), Some(Platform::Roblox));

        let named = fixture("named-project");
        fs::write(named.join("game.project.json"), "{}").unwrap();
        assert_eq!(Platform::detect(&named), Some(Platform::Roblox));

        for dir in [base, wally, named] {
            let _ = fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn stray_roblox_files_in_ancestors_cannot_shadow_a_uefn_project() {
        // The ForestTest regression: a wally.toml somewhere up the tree must
        // not make detection inconclusive inside a real UEFN project.
        let base = fixture("uefn-with-stray");
        fs::write(base.join("wally.toml"), "[package]").unwrap();
        let project = base.join("MyIsland");
        let pkg = project.join("Content").join("ForestPackages").join("myscope").join("MyPkg");
        fs::create_dir_all(&pkg).unwrap();
        fs::write(project.join("MyIsland.uefnproject"), "{}").unwrap();

        assert_eq!(Platform::detect(&pkg), Some(Platform::Uefn));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn detects_uefn_from_uefnproject() {
        let base = fixture("uefn");
        fs::create_dir_all(base.join("Content")).unwrap();
        fs::write(base.join("Island.uefnproject"), "{}").unwrap();
        assert_eq!(Platform::detect(&base), Some(Platform::Uefn));
        assert_eq!(Platform::detect(&base.join("Content")), Some(Platform::Uefn));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn ambiguous_or_empty_directories_are_inconclusive() {
        let empty = fixture("empty");
        assert_eq!(Platform::detect(&empty), None, "no signals: prompt");

        let both = fixture("both");
        fs::write(both.join("default.project.json"), "{}").unwrap();
        fs::write(both.join("Island.uefnproject"), "{}").unwrap();
        assert_eq!(Platform::detect(&both), None, "conflicting signals: prompt");

        for dir in [empty, both] {
            let _ = fs::remove_dir_all(dir);
        }
    }
}
