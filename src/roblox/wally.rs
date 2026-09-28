//! Wally-to-Forest conversion: parse a `wally.toml` and translate its
//! dependencies into forest.json form. Every wally package is mirrored on
//! the Forest registry under the same `scope/name`, so converted
//! dependencies resolve as-is. Each wally dependency group has its own
//! folder, and each becomes a forest mount with the same folder name.
//!
//! Wally dependency entries look like `Alias = "scope/name@req"`, where the
//! requirement uses CARGO semantics: a bare `1.2.3` means caret. Forest
//! requirements are explicit, so bare versions gain a `^` on conversion
//! (same rule as the registry's wally mirror pipeline).

use anyhow::{Context, Result};

/// Wally's folder for [server-dependencies].
pub const SERVER_PACKAGES: &str = "ServerPackages";
/// Wally's folder for [dev-dependencies].
pub const DEV_PACKAGES: &str = "DevPackages";

pub struct WallyDep {
    /// "scope/name"
    pub full_name: String,
    /// Forest-form requirement (bare versions caret-prefixed).
    pub version: String,
    /// Kept only when it differs from the package's name segment (wally
    /// aliases are how code requires the package, so casing matters).
    pub alias: Option<String>,
}

pub struct WallyImport {
    /// [dependencies]: the default mount.
    pub dependencies: Vec<WallyDep>,
    /// [server-dependencies]: the SERVER_PACKAGES mount.
    pub server_dependencies: Vec<WallyDep>,
    /// [dev-dependencies]: the DEV_PACKAGES mount.
    pub dev_dependencies: Vec<WallyDep>,
    /// SPDX id from [package].license, when present.
    pub license: Option<String>,
    /// Entries that couldn't be understood, described for warning output.
    pub skipped_malformed: Vec<String>,
}

/// Cargo semantics: a bare requirement starting with a digit means caret.
fn translate_req(req: &str) -> String {
    let trimmed = req.trim();
    if trimmed.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
        format!("^{}", trimmed)
    } else {
        trimmed.to_string()
    }
}

fn parse_dep_group(
    table: &toml::Value,
    group: &str,
    out: &mut Vec<WallyDep>,
    malformed: &mut Vec<String>,
) {
    let Some(deps) = table.get(group).and_then(toml::Value::as_table) else { return };
    for (alias, spec) in deps {
        let Some(spec) = spec.as_str() else {
            malformed.push(format!("{}: non-string entry", alias));
            continue;
        };
        // "scope/name@req"; split at the LAST '@' (scopes/names can't
        // contain '@', but be defensive).
        let Some((full_name, req)) = spec.rsplit_once('@') else {
            malformed.push(format!("{} = \"{}\": missing @version", alias, spec));
            continue;
        };
        let mut parts = full_name.split('/');
        let (Some(scope), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
            malformed.push(format!("{} = \"{}\": expected scope/name", alias, spec));
            continue;
        };
        if scope.is_empty() || name.is_empty() {
            malformed.push(format!("{} = \"{}\": expected scope/name", alias, spec));
            continue;
        }

        // Forest aliases become folder names, and install never recognizes
        // `_`/`.`-prefixed ones. Drop such aliases rather than fail.
        let keep_alias = alias != name && !alias.starts_with('_') && !alias.starts_with('.');
        out.push(WallyDep {
            full_name: full_name.to_string(),
            version: translate_req(req),
            alias: keep_alias.then(|| alias.to_string()),
        });
    }
}

pub fn parse_wally_manifest(text: &str) -> Result<WallyImport> {
    let parsed: toml::Value = text.parse().context("Failed to parse wally.toml")?;
    let mut import = WallyImport {
        dependencies: Vec::new(),
        server_dependencies: Vec::new(),
        dev_dependencies: Vec::new(),
        license: parsed
            .get("package")
            .and_then(|p| p.get("license"))
            .and_then(toml::Value::as_str)
            .map(str::to_string),
        skipped_malformed: Vec::new(),
    };

    parse_dep_group(&parsed, "dependencies", &mut import.dependencies, &mut import.skipped_malformed);
    parse_dep_group(&parsed, "server-dependencies", &mut import.server_dependencies, &mut import.skipped_malformed);
    parse_dep_group(&parsed, "dev-dependencies", &mut import.dev_dependencies, &mut import.skipped_malformed);

    Ok(import)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"
[package]
name = "cool-studio/my-game"
version = "0.1.0"
registry = "https://github.com/UpliftGames/wally-index"
realm = "shared"
license = "MIT"

[dependencies]
Promise = "evaera/promise@4.0.0"
Signal = "sleitnick/signal@^1.5.0"
matter = "matter-ecs/matter@0.8.5"

[server-dependencies]
ProfileService = "loleris/profileservice@1.0.0"

[dev-dependencies]
TestEZ = "roblox/testez@0.4.1"
"#;

    #[test]
    fn parses_and_translates_all_groups() {
        let import = parse_wally_manifest(FIXTURE).unwrap();
        assert_eq!(import.dependencies.len(), 3);
        assert!(import.skipped_malformed.is_empty());
        assert_eq!(import.license.as_deref(), Some("MIT"));

        let promise = import.dependencies.iter().find(|d| d.full_name == "evaera/promise").unwrap();
        assert_eq!(promise.version, "^4.0.0", "bare wally reqs are Cargo-caret semantics");
        assert_eq!(promise.alias.as_deref(), Some("Promise"), "casing differs from name segment");

        let signal = import.dependencies.iter().find(|d| d.full_name == "sleitnick/signal").unwrap();
        assert_eq!(signal.version, "^1.5.0", "explicit reqs pass through");

        let matter = import.dependencies.iter().find(|d| d.full_name == "matter-ecs/matter").unwrap();
        assert!(matter.alias.is_none(), "alias equal to the name segment is dropped");
    }

    #[test]
    fn server_and_dev_groups_stay_apart() {
        // Each group keeps its own folder, so each becomes its own mount.
        let import = parse_wally_manifest(FIXTURE).unwrap();
        assert_eq!(import.server_dependencies.len(), 1);
        assert_eq!(import.server_dependencies[0].full_name, "loleris/profileservice");
        assert_eq!(import.dev_dependencies.len(), 1);
        assert_eq!(import.dev_dependencies[0].full_name, "roblox/testez");
        assert_eq!(import.dev_dependencies[0].alias.as_deref(), Some("TestEZ"));
        assert!(!import.dependencies.iter().any(|d| d.full_name == "loleris/profileservice"));
    }

    #[test]
    fn malformed_entries_are_skipped_with_reasons() {
        let import = parse_wally_manifest(
            "[dependencies]\nBad = \"no-version-marker\"\nWorse = \"toomany/parts/here@1.0.0\"\nGood = \"a/b@1.0.0\"\n",
        )
        .unwrap();
        assert_eq!(import.dependencies.len(), 1);
        assert_eq!(import.skipped_malformed.len(), 2);
    }

    #[test]
    fn underscore_aliases_are_dropped_not_fatal() {
        let import = parse_wally_manifest("[dependencies]\n_Index = \"a/b@1.0.0\"\n").unwrap();
        assert_eq!(import.dependencies.len(), 1);
        assert!(import.dependencies[0].alias.is_none());
    }

    #[test]
    fn empty_or_missing_groups_are_fine() {
        let import = parse_wally_manifest("[package]\nname = \"a/b\"\n").unwrap();
        assert!(import.dependencies.is_empty());
        assert!(import.server_dependencies.is_empty());
        assert!(import.dev_dependencies.is_empty());
        assert!(import.license.is_none());
    }
}
