//! `forest update`: move every dependency to the newest version its
//! declared range allows. forest.json is never touched. Jumping past a
//! range is `forest audit --update`; the CLI's own self-update is
//! `forest upgrade`.
//!
//! Implemented as a fresh resolve. The old lockfile is only read for the
//! before/after report, so direct and transitive deps all land where a
//! first-ever install would put them.

use std::collections::BTreeMap;

use anyhow::Result;
use serde_json::Value;

use crate::lockfile::{LockFile, LockSection};
use crate::lockfile_gen::{sync, Refresh, SyncOptions};
use crate::message::{self, Message};
use crate::mounts::Mount;

/// Package name -> sorted resolved versions (usually one; conflict buckets
/// can hold several), taken from a lockfile's `packages` map.
fn locked_version_map(lock: &Value) -> BTreeMap<String, Vec<String>> {
    let mut map = BTreeMap::new();
    let Some(packages) = lock.get("packages").and_then(Value::as_object) else {
        return map;
    };
    for (name, entries) in packages {
        let mut versions: Vec<String> = entries
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|e| e.get("version").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        versions.sort();
        map.insert(name.clone(), versions);
    }
    map
}

/// Human lines for what a re-resolve changed. Keys are canonical on both
/// sides, so plain equality is the right comparison.
fn diff_locked(
    old: &BTreeMap<String, Vec<String>>,
    new: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    let mut lines = Vec::new();
    for (name, new_versions) in new {
        match old.get(name) {
            None => lines.push(format!("{} added ({})", name, new_versions.join(", "))),
            Some(old_versions) if old_versions != new_versions => lines.push(format!(
                "{} {} -> {}",
                name,
                old_versions.join(", "),
                new_versions.join(", ")
            )),
            Some(_) => {}
        }
    }
    for (name, old_versions) in old {
        if !new.contains_key(name) {
            lines.push(format!("{} removed (was {})", name, old_versions.join(", ")));
        }
    }
    lines
}

/// A mount's section as the version map the diff compares.
fn section_versions(section: Option<&LockSection>) -> Result<BTreeMap<String, Vec<String>>> {
    Ok(match section {
        Some(section) => locked_version_map(&serde_json::to_value(section)?),
        None => BTreeMap::new(),
    })
}

pub async fn update_command(mount: Option<String>) -> Result<()> {
    let Some(project) = super::context::load_project()? else {
        crate::message::fail("No forest.json found. Run `forest init` to create a new package.");
        return Ok(());
    };
    let info = project.manifest;
    let mounts = crate::mounts::project_mounts(&info, project.platform)?;
    let scope = mount.as_deref().map(|r| crate::mounts::find_mount(&mounts, r)).transpose()?;
    let in_run: Vec<&Mount> = mounts.iter().filter(|m| scope.map_or(true, |s| s.path == m.path)).collect();
    let mut msg = Message::new("Updating dependencies...");

    // Read only for the before/after report.
    let old = LockFile::load();
    let mut old_versions = Vec::new();
    for mount in &in_run {
        old_versions.push(section_versions(old.as_ref().and_then(|lf| lf.section(mount)))?);
    }

    let opts = SyncOptions::new(scope.map(|m| m.path.clone()), Refresh::All);
    let outcome = sync(&info, &mut msg, &opts).await?;

    msg.destroy();
    let mut total = 0;
    for (mount, old) in in_run.iter().zip(&old_versions) {
        let changes = diff_locked(old, &section_versions(outcome.lockfile.section(mount))?);
        if changes.is_empty() {
            continue;
        }
        let indent = if mounts.len() > 1 {
            message::info(&format!("{}:", mount.label()));
            "    "
        } else {
            "  "
        };
        for line in &changes {
            message::info(&format!("{}{}", indent, line));
        }
        total += changes.len();
    }
    if total == 0 {
        message::success("All dependencies are already at their newest allowed versions!");
        message::info("Newer majors may exist outside your declared ranges - see `forest audit`.");
    } else {
        message::success(&format!(
            "Updated {} package{} within the declared ranges!",
            total,
            if total == 1 { "" } else { "s" }
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, vs)| (k.to_string(), vs.iter().map(|v| v.to_string()).collect()))
            .collect()
    }

    #[test]
    fn version_map_flattens_and_sorts_buckets() {
        let lock = json!({
            "packages": {
                "a/b": [ { "version": "2.0.0" }, { "version": "1.4.0" } ],
                "c/d": [ { "version": "0.3.1" } ]
            }
        });
        assert_eq!(
            locked_version_map(&lock),
            map(&[("a/b", &["1.4.0", "2.0.0"]), ("c/d", &["0.3.1"])])
        );
    }

    #[test]
    fn diff_reports_moves_additions_and_removals_only() {
        let old = map(&[
            ("a/b", &["1.4.0"]),
            ("kept/same", &["3.0.0"]),
            ("gone/pkg", &["0.1.0"]),
        ]);
        let new = map(&[
            ("a/b", &["1.5.0"]),
            ("kept/same", &["3.0.0"]),
            ("new/pkg", &["2.2.0"]),
        ]);
        assert_eq!(
            diff_locked(&old, &new),
            vec![
                "a/b 1.4.0 -> 1.5.0",
                "new/pkg added (2.2.0)",
                "gone/pkg removed (was 0.1.0)",
            ]
        );
    }

    #[test]
    fn identical_lockfiles_diff_to_nothing() {
        let m = map(&[("a/b", &["1.0.0"])]);
        assert!(diff_locked(&m, &m).is_empty());
    }
}
