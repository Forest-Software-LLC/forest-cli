//! `forest update`: move every dependency to the newest version its
//! declared range allows. forest.json is never touched. Jumping past a
//! range is `forest audit --update`; the CLI's own self-update is
//! `forest upgrade`.
//!
//! Implemented as a fresh resolve. The old lockfile is only read for the
//! before/after report, so direct and transitive deps all land where a
//! first-ever install would put them.

use std::collections::BTreeMap;
use std::fs;

use anyhow::Result;
use serde_json::Value;

use crate::lockfile_gen::lockfile_gen;
use crate::message::{self, Message};

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

pub async fn update_command() -> Result<()> {
    let Some(project) = super::context::load_project()? else {
        crate::message::fail("No forest.json found. Run `forest init` to create a new package.");
        return Ok(());
    };
    let mut msg = Message::new("Updating dependencies...");
    let info = project.manifest;

    // Read only for the before/after report.
    let old_versions = fs::read_to_string("forest-lock.json")
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .map(|lock| locked_version_map(&lock))
        .unwrap_or_default();

    let lockfile = lockfile_gen(&info, &mut msg, false).await?;
    fs::write("forest-lock.json", lockfile.to_json_pretty()?)?;

    let new_versions = locked_version_map(&serde_json::to_value(&lockfile)?);
    let changes = diff_locked(&old_versions, &new_versions);

    msg.destroy();
    if changes.is_empty() {
        message::success("All dependencies are already at their newest allowed versions!");
        message::info("Newer majors may exist outside your declared ranges - see `forest audit`.");
    } else {
        for line in &changes {
            message::info(&format!("  {}", line));
        }
        message::success(&format!(
            "Updated {} package{} within the declared ranges!",
            changes.len(),
            if changes.len() == 1 { "" } else { "s" }
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
