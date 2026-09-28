//! Dependency mounts: the folders a project installs dependencies into.
//!
//! The default mount is forest.json's top-level `dependencies`, placed by
//! the platform (Roblox: next to the root file, so development layout
//! matches the published one). Extra mounts live under `mounts`, keyed by
//! their path relative to the manifest dir. Each mount is resolved and
//! installed on its own, never shares packages with another, and only the
//! default mount is published. This module is the only writer of the
//! `mounts` field.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};

use crate::lockfile_solver::DepSpec;
use crate::platform::Platform;
use crate::utils::{normalize_forest_deps, resolve_dep_ref, DepRef};

pub const MOUNTS_FIELD: &str = "mounts";

#[derive(Debug, Clone)]
pub struct Mount {
    /// Relative to the manifest dir: forward slashes, no leading "./" or
    /// trailing "/". The lockfile keys extra mounts by this.
    pub path: String,
    /// The `mounts` key exactly as forest.json spells it; None for the
    /// default mount.
    pub key: Option<String>,
    pub deps: HashMap<String, DepSpec>,
}

impl Mount {
    pub fn is_default(&self) -> bool {
        self.key.is_none()
    }

    /// The folder's own name: the planner's container prefix and what code
    /// requires by.
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    /// For lists and headers: the path, marked when it is the default.
    pub fn label(&self) -> String {
        if self.is_default() {
            format!("{} (default)", self.path)
        } else {
            self.path.clone()
        }
    }
}

/// Every mount the manifest declares, default first, validated as a set.
pub fn project_mounts(manifest: &Value, platform: Platform) -> Result<Vec<Mount>> {
    let mut mounts = vec![Mount {
        path: platform.default_mount_path(manifest),
        key: None,
        deps: normalize_forest_deps(manifest),
    }];
    let Some(field) = manifest.get(MOUNTS_FIELD) else {
        return Ok(mounts);
    };
    let entries = field
        .as_object()
        .ok_or_else(|| anyhow!("\"mounts\" in forest.json must be an object keyed by folder path."))?;
    if entries.is_empty() {
        return Ok(mounts);
    }
    if !platform.supports_mounts() {
        bail!(
            "Mounts are not supported on {}. Remove \"mounts\" from forest.json.",
            platform.display_name()
        );
    }
    for (key, entry) in entries {
        let path = normalize_mount_path(key)
            .map_err(|reason| anyhow!("Invalid mount \"{}\" in forest.json: {}", key, reason))?;
        if !entry.is_object() || entry.get("dependencies").map_or(false, |d| !d.is_object()) {
            bail!(
                "Invalid mount \"{}\" in forest.json: expected {{ \"dependencies\": {{ ... }} }}.",
                key
            );
        }
        mounts.push(Mount { path, key: Some(key.clone()), deps: normalize_forest_deps(entry) });
    }
    let root = manifest.get("root").and_then(Value::as_str).unwrap_or("");
    check_layout(&mounts, root)?;
    Ok(mounts)
}

/// Folder-name rule for every dependency folder (a mount's own name, and
/// the `packagesDir` a package publishes): `^[A-Za-z][A-Za-z0-9_-]*$`, max
/// 64 chars, Windows reserved device names rejected case-insensitively. The
/// letter start excludes path-traversal characters and `_`/`.` names. The
/// gateway enforces the same rule on `packagesDir`, and install checks it
/// again because registry values flow into filesystem paths.
pub fn validate_folder_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Dependency folder name cannot be empty.".to_string());
    }
    if name.len() > 64 {
        return Err("Dependency folder name cannot be longer than 64 characters.".to_string());
    }
    let mut chars = name.chars();
    if !chars.next().map_or(false, |c| c.is_ascii_alphabetic()) {
        return Err("Dependency folder name must start with a letter.".to_string());
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(
            "Dependency folder name may only contain letters, numbers, underscores, and hyphens."
                .to_string(),
        );
    }
    const WINDOWS_RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if WINDOWS_RESERVED.contains(&name.to_ascii_uppercase().as_str()) {
        return Err(format!(
            "Dependency folder name '{}' is a reserved Windows device name.",
            name
        ));
    }
    Ok(())
}

/// The stored form of a mount path. It must stay inside the project, and
/// its last segment is the folder name, so it follows the folder-name rule.
pub fn normalize_mount_path(raw: &str) -> Result<String, String> {
    let unified = raw.trim().replace('\\', "/");
    let trimmed = unified.trim_end_matches('/');
    let trimmed = trimmed.strip_prefix("./").unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Err("the path is empty; a mount is a folder inside the project.".to_string());
    }
    if trimmed.starts_with('/') || trimmed.contains(':') {
        return Err("the path must be relative to the folder holding forest.json.".to_string());
    }
    let segments: Vec<&str> = trimmed.split('/').collect();
    for segment in &segments {
        if segment.is_empty() {
            return Err("the path has an empty segment.".to_string());
        }
        if *segment == "." || *segment == ".." {
            return Err("the path cannot contain . or .. segments.".to_string());
        }
        if segment.chars().any(|c| c.is_control() || "<>\"|?*".contains(c))
            || segment.ends_with('.')
            || segment.ends_with(' ')
        {
            return Err(format!("\"{}\" is not a folder name every OS accepts.", segment));
        }
    }
    let name = segments.last().expect("split yields at least one segment");
    validate_folder_name(name)?;
    Ok(segments.join("/"))
}

/// Set-level rules: no two mounts share a folder or nest inside each other
/// (an install prunes everything it didn't plan at its mount's top level, so
/// a nested mount would be deleted), and no extra mount holds the package's
/// root file (it would be pruned along with the rest of the source).
fn check_layout(mounts: &[Mount], root: &str) -> Result<()> {
    // First: such a mount also contains the default mount, and the nesting
    // error below would hide the reason that matters.
    if let Some(root_parent) = crate::utils::manifest_root_parent(root) {
        let parent_lc = root_parent.to_ascii_lowercase();
        for mount in mounts.iter().filter(|m| !m.is_default()) {
            let mount_lc = mount.path.to_ascii_lowercase();
            if parent_lc == mount_lc || parent_lc.starts_with(&format!("{}/", mount_lc)) {
                bail!(
                    "Mount {} holds this package's root file ({}). Installs replace everything in a mount, so give it a folder of its own.",
                    mount.path,
                    root
                );
            }
        }
    }
    for (i, a) in mounts.iter().enumerate() {
        for b in &mounts[i + 1..] {
            let (a_lc, b_lc) = (a.path.to_ascii_lowercase(), b.path.to_ascii_lowercase());
            if a_lc == b_lc {
                bail!("Mount {} is declared twice in forest.json (as {} and {}).", b.path, a.label(), b.label());
            }
            if b_lc.starts_with(&format!("{}/", a_lc)) || a_lc.starts_with(&format!("{}/", b_lc)) {
                let (outer, inner) = if b_lc.len() > a_lc.len() { (a, b) } else { (b, a) };
                bail!(
                    "Mount {} is inside mount {}. Mounts can't nest: installing {} would delete it.",
                    inner.label(),
                    outer.label(),
                    outer.path
                );
            }
        }
    }
    Ok(())
}

/// How a `--mount` reference resolved against the project's mounts.
#[derive(Debug)]
pub enum MountRef<'a> {
    Match(&'a Mount),
    NotFound,
    /// Paths of every mount the reference could mean, sorted.
    Ambiguous(Vec<String>),
}

/// Resolve a `--mount` reference: an exact path wins; otherwise the
/// reference must match the trailing segments of exactly one mount's path
/// (`server/Packages` finds `src/server/Packages`). Case-insensitive, and
/// ambiguity is reported, never guessed.
pub fn resolve_mount_ref<'a>(mounts: &'a [Mount], reference: &str) -> MountRef<'a> {
    let unified = reference.trim().replace('\\', "/");
    let wanted = unified.trim_end_matches('/');
    let wanted = wanted.strip_prefix("./").unwrap_or(wanted).to_ascii_lowercase();
    if wanted.is_empty() {
        return MountRef::NotFound;
    }
    if let Some(exact) = mounts.iter().find(|m| m.path.to_ascii_lowercase() == wanted) {
        return MountRef::Match(exact);
    }
    let suffix = format!("/{}", wanted);
    let matches: Vec<&Mount> = mounts
        .iter()
        .filter(|m| m.path.to_ascii_lowercase().ends_with(&suffix))
        .collect();
    match matches.as_slice() {
        [] => MountRef::NotFound,
        [only] => MountRef::Match(only),
        many => {
            let mut paths: Vec<String> = many.iter().map(|m| m.path.clone()).collect();
            paths.sort();
            MountRef::Ambiguous(paths)
        }
    }
}

/// `resolve_mount_ref` with the user-facing errors every `--mount` flag
/// shares.
pub fn find_mount<'a>(mounts: &'a [Mount], reference: &str) -> Result<&'a Mount> {
    match resolve_mount_ref(mounts, reference) {
        MountRef::Match(mount) => Ok(mount),
        MountRef::Ambiguous(paths) => Err(anyhow!(
            "--mount {} matches more than one mount: {}. Give more of the path.",
            reference,
            paths.join(", ")
        )),
        MountRef::NotFound => Err(anyhow!(
            "No mount matches {}. Mounts: {}. Create one with `forest mount create <path>`.",
            reference,
            mounts.iter().map(|m| m.path.as_str()).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// Where a package reference (scope/name, alias, or bare name) is a direct
/// dependency.
pub enum DepLocation<'a> {
    Found(&'a Mount, String),
    NotFound,
    /// Ambiguous inside one mount: the candidate keys.
    Ambiguous(Vec<String>),
    /// Declared in several mounts, with each one's manifest key.
    InSeveral(Vec<(&'a Mount, String)>),
}

impl DepLocation<'_> {
    /// The paths of the mounts an InSeveral result names.
    pub fn several_paths(found: &[(&Mount, String)]) -> String {
        found.iter().map(|(m, _)| m.path.as_str()).collect::<Vec<_>>().join(", ")
    }
}

/// Find the mount declaring `reference`, searching `scope` (one mount) or
/// every mount.
pub fn locate_dep<'a>(mounts: &'a [Mount], scope: Option<&'a Mount>, reference: &str) -> DepLocation<'a> {
    let candidates: Vec<&Mount> = match scope {
        Some(mount) => vec![mount],
        None => mounts.iter().collect(),
    };
    let mut found: Vec<(&Mount, String)> = Vec::new();
    for mount in candidates {
        match resolve_dep_ref(&mount.deps, reference) {
            DepRef::Match(key) => found.push((mount, key)),
            DepRef::Ambiguous(keys) => return DepLocation::Ambiguous(keys),
            DepRef::NotFound => {}
        }
    }
    match found.len() {
        0 => DepLocation::NotFound,
        1 => {
            let (mount, key) = found.pop().expect("one match");
            DepLocation::Found(mount, key)
        }
        _ => DepLocation::InSeveral(found),
    }
}

/// A mount's dependency map in the manifest, created when absent.
pub fn deps_map_mut<'a>(manifest: &'a mut Value, mount: &Mount) -> Result<&'a mut Map<String, Value>> {
    let holder = match &mount.key {
        None => manifest,
        Some(key) => manifest
            .get_mut(MOUNTS_FIELD)
            .and_then(Value::as_object_mut)
            .and_then(|entries| entries.get_mut(key))
            .ok_or_else(|| anyhow!("Mount {} is not in forest.json.", mount.path))?,
    };
    if !holder.get("dependencies").map_or(false, Value::is_object) {
        holder["dependencies"] = Value::Object(Map::new());
    }
    Ok(holder["dependencies"].as_object_mut().expect("ensured above"))
}

/// Declare a new mount at `path` (already normalized and checked).
pub fn insert_mount(manifest: &mut Value, path: &str, dependencies: Map<String, Value>) {
    if !manifest.get(MOUNTS_FIELD).map_or(false, Value::is_object) {
        manifest[MOUNTS_FIELD] = Value::Object(Map::new());
    }
    manifest[MOUNTS_FIELD]
        .as_object_mut()
        .expect("ensured above")
        .insert(path.to_string(), serde_json::json!({ "dependencies": dependencies }));
}

/// Drop an extra mount's entry, and the field once it empties.
pub fn remove_mount(manifest: &mut Value, mount: &Mount) {
    let Some(key) = &mount.key else { return };
    let Some(entries) = manifest.get_mut(MOUNTS_FIELD).and_then(Value::as_object_mut) else {
        return;
    };
    entries.remove(key);
    if entries.is_empty() {
        manifest.as_object_mut().expect("manifest is an object").remove(MOUNTS_FIELD);
    }
}

/// Re-key an extra mount's entry, keeping its dependencies.
pub fn rename_mount(manifest: &mut Value, mount: &Mount, new_path: &str) {
    let Some(key) = &mount.key else { return };
    let Some(entries) = manifest.get_mut(MOUNTS_FIELD).and_then(Value::as_object_mut) else {
        return;
    };
    if let Some(entry) = entries.remove(key) {
        entries.insert(new_path.to_string(), entry);
    }
}

/// Validate a new extra mount against the project's current ones, the way
/// a manifest declaring it would be validated.
pub fn check_new_mount(manifest: &Value, platform: Platform, path: &str) -> Result<()> {
    let mut candidate = manifest.clone();
    insert_mount(&mut candidate, path, Map::new());
    project_mounts(&candidate, platform).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mounts(manifest: Value) -> Result<Vec<Mount>> {
        project_mounts(&manifest, Platform::Roblox)
    }

    fn paths(list: &[Mount]) -> Vec<&str> {
        list.iter().map(|m| m.path.as_str()).collect()
    }

    #[test]
    fn folder_name_rule_accepts_sane_names_only() {
        assert!(validate_folder_name("Packages").is_ok());
        assert!(validate_folder_name("roblox_packages").is_ok());
        assert!(validate_folder_name("my-packages").is_ok());
        assert!(validate_folder_name(&"a".repeat(64)).is_ok(), "64 chars is the ceiling");

        assert!(validate_folder_name("").is_err());
        assert!(validate_folder_name(&"a".repeat(65)).is_err());
        assert!(validate_folder_name("..").is_err());
        assert!(validate_folder_name("a/b").is_err());
        assert!(validate_folder_name("a\\b").is_err());
        assert!(validate_folder_name("_lead").is_err());
        assert!(validate_folder_name(".lead").is_err());
        assert!(validate_folder_name("1pkg").is_err(), "must start with a letter");
        assert!(validate_folder_name("CON").is_err(), "Windows device name");
        assert!(validate_folder_name("con").is_err(), "device names reject case-insensitively");
        assert!(validate_folder_name("Com5").is_err());
        assert!(validate_folder_name("lpt9").is_err());
        assert!(validate_folder_name("COM10").is_ok(), "only COM1-COM9 are reserved");
    }

    #[test]
    fn mount_paths_normalize_to_the_stored_form() {
        assert_eq!(normalize_mount_path("DevPackages").unwrap(), "DevPackages");
        assert_eq!(normalize_mount_path("./src/server/Packages/").unwrap(), "src/server/Packages");
        assert_eq!(normalize_mount_path("src\\server\\Packages").unwrap(), "src/server/Packages");
        assert_eq!(normalize_mount_path(" Server Scripts/Packages ").unwrap(), "Server Scripts/Packages");
    }

    #[test]
    fn mount_paths_that_leave_the_project_or_break_folder_rules_are_rejected() {
        for bad in [
            "", ".", "./", "/abs/Packages", "C:/Packages", "../Packages", "src/../Packages",
            "src//Packages", "src/./Packages", "src/_Index", "src/1Packages", "src/CON",
            "a?b/Packages", "trailing./Packages",
        ] {
            assert!(normalize_mount_path(bad).is_err(), "{:?} must be rejected", bad);
        }
    }

    #[test]
    fn default_mount_comes_first_and_extra_mounts_follow() {
        let list = mounts(json!({
            "platform": "roblox",
            "root": "src/init.luau",
            "dependencies": { "a/knit": "^1.0.0" },
            "mounts": {
                "src/server/Packages": { "dependencies": { "b/profile": "^1.0.0" } },
                "DevPackages": {}
            }
        }))
        .unwrap();
        assert_eq!(paths(&list), vec!["src/Packages", "DevPackages", "src/server/Packages"]);
        assert!(list[0].is_default());
        assert!(list[0].deps.contains_key("a/knit"));
        assert!(list[1].deps.is_empty(), "an entry without dependencies is an empty mount");
        assert!(list[2].deps.contains_key("b/profile"));
        assert_eq!(list[2].name(), "Packages");
        assert_eq!(list[2].key.as_deref(), Some("src/server/Packages"));
    }

    #[test]
    fn same_named_mounts_in_different_folders_are_fine() {
        let list = mounts(json!({
            "platform": "roblox",
            "mounts": {
                "src/client/Packages": {},
                "src/server/Packages": {}
            }
        }))
        .unwrap();
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn duplicate_nested_and_root_holding_mounts_fail_loudly() {
        let twice = mounts(json!({ "platform": "roblox", "mounts": { "Packages": {} } })).unwrap_err();
        assert!(twice.to_string().contains("declared twice"), "{}", twice);

        let case_twice = mounts(json!({
            "platform": "roblox",
            "mounts": { "DevPackages": {}, "./devpackages": {} }
        }))
        .unwrap_err();
        assert!(case_twice.to_string().contains("declared twice"), "{}", case_twice);

        let nested = mounts(json!({
            "platform": "roblox",
            "mounts": { "Packages/Server": {} }
        }))
        .unwrap_err();
        assert!(nested.to_string().contains("inside mount Packages (default)"), "{}", nested);

        let outer = mounts(json!({
            "platform": "roblox",
            "root": "lib/src/init.luau",
            "mounts": { "lib": {} }
        }))
        .unwrap_err();
        assert!(outer.to_string().contains("holds this package's root file"), "{}", outer);
    }

    #[test]
    fn malformed_mount_entries_fail_loudly() {
        assert!(mounts(json!({ "platform": "roblox", "mounts": [] })).is_err());
        assert!(mounts(json!({ "platform": "roblox", "mounts": { "Dev": "x" } })).is_err());
        assert!(mounts(json!({ "platform": "roblox", "mounts": { "Dev": { "dependencies": [] } } })).is_err());
        assert!(mounts(json!({ "platform": "roblox", "mounts": { "../Dev": {} } })).is_err());
    }

    #[test]
    fn uefn_manifests_cannot_declare_mounts() {
        let err = project_mounts(
            &json!({ "platform": "uefn", "mounts": { "Dev": {} } }),
            Platform::Uefn,
        )
        .unwrap_err();
        assert!(err.to_string().contains("not supported on UEFN"), "{}", err);
        // An empty field is harmless.
        assert_eq!(project_mounts(&json!({ "platform": "uefn", "mounts": {} }), Platform::Uefn).unwrap().len(), 1);
    }

    fn fixture_mounts() -> Vec<Mount> {
        mounts(json!({
            "platform": "roblox",
            "root": "src/init.luau",
            "mounts": {
                "src/server/Packages": { "dependencies": { "acme/signal": "^1.0.0" } },
                "DevPackages": { "dependencies": { "roblox/testez": "^0.4.0", "acme/signal": "^2.0.0" } }
            },
            "dependencies": { "acme/knit": "^1.0.0" }
        }))
        .unwrap()
    }

    #[test]
    fn mount_refs_match_exact_paths_or_unique_suffixes() {
        let list = fixture_mounts();
        let hit = |r: &str| match resolve_mount_ref(&list, r) {
            MountRef::Match(m) => m.path.clone(),
            other => panic!("{:?} -> {:?}", r, other),
        };
        assert_eq!(hit("DevPackages"), "DevPackages");
        assert_eq!(hit("devpackages/"), "DevPackages");
        assert_eq!(hit("server/Packages"), "src/server/Packages");
        assert_eq!(hit("src\\server\\packages"), "src/server/Packages");
        assert_eq!(hit("src/Packages"), "src/Packages");

        match resolve_mount_ref(&list, "Packages") {
            MountRef::Ambiguous(paths) => assert_eq!(paths, vec!["src/Packages", "src/server/Packages"]),
            other => panic!("expected ambiguity, got {:?}", other),
        }
        assert!(matches!(resolve_mount_ref(&list, "Server"), MountRef::NotFound), "suffixes are whole segments");
        assert!(matches!(resolve_mount_ref(&list, ""), MountRef::NotFound));
    }

    #[test]
    fn an_exact_path_beats_a_suffix_match() {
        let list = mounts(json!({
            "platform": "roblox",
            "mounts": { "src/server/Packages": {} }
        }))
        .unwrap();
        match resolve_mount_ref(&list, "Packages") {
            MountRef::Match(m) => assert!(m.is_default(), "top-level Packages is an exact hit"),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn deps_are_located_in_one_mount_or_reported() {
        let list = fixture_mounts();
        match locate_dep(&list, None, "testez") {
            DepLocation::Found(m, key) => {
                assert_eq!(m.path, "DevPackages");
                assert_eq!(key, "roblox/testez");
            }
            _ => panic!("testez lives in DevPackages only"),
        }
        match locate_dep(&list, None, "acme/signal") {
            DepLocation::InSeveral(found) => {
                assert_eq!(DepLocation::several_paths(&found), "DevPackages, src/server/Packages");
            }
            _ => panic!("signal is declared in two mounts"),
        }
        match locate_dep(&list, Some(&list[2]), "signal") {
            DepLocation::Found(m, _) => assert_eq!(m.path, "src/server/Packages"),
            _ => panic!("scoping to one mount disambiguates"),
        }
        assert!(matches!(locate_dep(&list, None, "missing"), DepLocation::NotFound));
    }

    #[test]
    fn manifest_writers_touch_only_the_mounts_field() {
        let mut manifest = json!({ "platform": "roblox", "dependencies": { "a/b": "^1.0.0" } });
        insert_mount(&mut manifest, "DevPackages", Map::new());
        assert_eq!(manifest["mounts"]["DevPackages"], json!({ "dependencies": {} }));

        let list = project_mounts(&manifest, Platform::Roblox).unwrap();
        deps_map_mut(&mut manifest, &list[1]).unwrap().insert("x/y".into(), json!("^2.0.0"));
        deps_map_mut(&mut manifest, &list[0]).unwrap().insert("c/d".into(), json!("^3.0.0"));
        assert_eq!(manifest["mounts"]["DevPackages"]["dependencies"]["x/y"], "^2.0.0");
        assert_eq!(manifest["dependencies"]["c/d"], "^3.0.0");

        let list = project_mounts(&manifest, Platform::Roblox).unwrap();
        rename_mount(&mut manifest, &list[1], "tools/Dev");
        assert_eq!(manifest["mounts"]["tools/Dev"]["dependencies"]["x/y"], "^2.0.0");
        assert!(manifest["mounts"].get("DevPackages").is_none());

        let list = project_mounts(&manifest, Platform::Roblox).unwrap();
        remove_mount(&mut manifest, &list[1]);
        assert!(manifest.get("mounts").is_none(), "an emptied field is dropped");
        assert_eq!(manifest["dependencies"]["a/b"], "^1.0.0");
    }

    #[test]
    fn new_mounts_are_checked_against_the_existing_set() {
        let manifest = json!({ "platform": "roblox", "root": "src/init.luau" });
        assert!(check_new_mount(&manifest, Platform::Roblox, "DevPackages").is_ok());
        assert!(check_new_mount(&manifest, Platform::Roblox, "src/Packages").is_err(), "the default's own folder");
        assert!(check_new_mount(&manifest, Platform::Roblox, "src").is_err(), "holds the root file");
        assert!(check_new_mount(&manifest, Platform::Roblox, "src/Packages/Dev").is_err(), "nested");
    }
}
