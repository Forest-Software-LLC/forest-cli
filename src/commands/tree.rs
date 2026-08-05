use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::Result;
use colored::Colorize;
use serde_json::Value;

use crate::lockfile_gen::{lockfile_satisfies_manifest, LockFile};
use crate::lockfile_solver::{DepSpec, LockfileEntry};
use crate::message::{info, warn};
use crate::utils::{digest_package_name, get_ci, normalize_forest_deps, resolve_dep_ref, DepRef};

/// Print the dependency tree from forest-lock.json. Fully offline: the
/// lockfile stores each entry's resolved deps with exact versions, so no
/// registry calls are needed. With a package reference, only that root
/// dependency's subtree is shown.
pub fn tree_command(target_package: Option<String>) -> Result<()> {
    // Same manifest discovery as install: some platforms keep forest.json
    // away from the project root (UEFN: inside Content/).
    if !Path::new("forest.json").exists() {
        if let Some(manifest_dir) = crate::platform::discover_manifest_dir(&std::env::current_dir()?) {
            std::env::set_current_dir(&manifest_dir)?;
        }
    }
    if !Path::new("forest.json").exists() {
        info("No forest.json found, nothing to show.");
        return Ok(());
    }

    let manifest: Value = serde_json::from_str(&fs::read_to_string("forest.json")?)?;
    let roots = normalize_forest_deps(&manifest);
    if roots.is_empty() {
        info("No dependencies declared in forest.json.");
        return Ok(());
    }

    // The reference may be the full scope/name, the alias, or the bare name,
    // same as remove. Only the rendered roots shrink; the staleness check
    // below still needs the full manifest set.
    let mut display_roots = roots.clone();
    if let Some(reference) = &target_package {
        match resolve_dep_ref(&roots, reference) {
            DepRef::NotFound => {
                info(&format!("Package {} is not a dependency of this project.", reference));
                return Ok(());
            }
            DepRef::Ambiguous(candidates) => {
                warn(&format!(
                    "\"{}\" matches more than one installed package: {}. Use the full <scope>/<name>.",
                    reference,
                    candidates.join(", ")
                ));
                return Ok(());
            }
            DepRef::Match(key) => display_roots.retain(|k, _| *k == key),
        }
    }

    if !Path::new("forest-lock.json").exists() {
        info("No forest-lock.json found. Run `forest install` first.");
        return Ok(());
    }
    let lock_content: Value = serde_json::from_str(&fs::read_to_string("forest-lock.json")?)?;
    if lock_content.get("file_version").and_then(Value::as_u64) != Some(2) {
        warn("Lockfile format is out of date; run `forest install` to regenerate it.");
        return Ok(());
    }
    let lockfile: LockFile = serde_json::from_value(lock_content)?;

    // Same trust check install uses (UEFN widens the roots to the whole
    // workspace). A stale lockfile still prints since it's what is on disk.
    let resolution_roots = crate::platform::Platform::from_manifest(&manifest)?
        .resolution_roots(roots)?;
    if !lockfile_satisfies_manifest(&lockfile, &resolution_roots) {
        warn("forest.json changed since the last install; run `forest install` to refresh the tree.");
    }

    print!("{}", render_tree(&project_label(&manifest), &display_roots, &lockfile));
    Ok(())
}

/// `author/name@version`, degrading gracefully when fields are missing.
fn project_label(manifest: &Value) -> String {
    let name = manifest.get("name").and_then(Value::as_str).unwrap_or("(unnamed)");
    let mut label = match manifest.get("author").and_then(Value::as_str) {
        Some(author) => format!("{}/{}", author, name),
        None => name.to_string(),
    };
    if let Some(version) = manifest.get("version").and_then(Value::as_str) {
        label.push_str(&format!("@{}", version));
    }
    label
}

fn render_tree(label: &str, roots: &HashMap<String, DepSpec>, lockfile: &LockFile) -> String {
    let mut out = format!("{}\n", label.bold());
    let mut sorted: Vec<(&String, &DepSpec)> = roots.iter().collect();
    sorted.sort_by_key(|(k, _)| k.to_lowercase());

    let mut stack = Vec::new();
    for (i, (name, spec)) in sorted.iter().enumerate() {
        let last = i + 1 == sorted.len();
        let entry = find_root_entry(lockfile, name, &spec.version);
        render_dep(lockfile, name, spec, entry, "", last, &mut stack, &mut out);
    }
    out
}

/// Root deps pin their resolved version at location "~". Fall back to a
/// range match so a hand-edited or stale lockfile still renders something.
fn find_root_entry<'a>(lockfile: &'a LockFile, name: &str, range: &str) -> Option<&'a LockfileEntry> {
    let entries = get_ci(&lockfile.packages, name)?;
    entries.iter().find(|e| e.location == "~").or_else(|| {
        let req = semver::VersionReq::parse(range).ok()?;
        entries
            .iter()
            .find(|e| semver::Version::parse(&e.version).map_or(false, |v| req.matches(&v)))
    })
}

fn render_dep(
    lockfile: &LockFile,
    name: &str,
    spec: &DepSpec,
    entry: Option<&LockfileEntry>,
    prefix: &str,
    last: bool,
    stack: &mut Vec<(String, String)>,
    out: &mut String,
) {
    let connector = if last { "└── " } else { "├── " };

    let mut label = match entry {
        Some(e) => format!("{}{}", name, format!("@{}", e.version).dimmed()),
        None => format!(
            "{}{} {}",
            name,
            format!("@{}", spec.version).dimmed(),
            "(not installed)".red()
        ),
    };
    // Call out deps installed under a different folder name than their own.
    if spec.alias != digest_package_name(name).name {
        label.push_str(&format!(" {}", format!("(as {})", spec.alias).dimmed()));
    }

    let Some(entry) = entry else {
        out.push_str(&format!("{}{}{}\n", prefix, connector, label));
        return;
    };

    let key = (name.to_lowercase(), entry.version.clone());
    if stack.contains(&key) {
        out.push_str(&format!("{}{}{} {}\n", prefix, connector, label, "(circular)".dimmed()));
        return;
    }
    out.push_str(&format!("{}{}{}\n", prefix, connector, label));

    let mut children: Vec<(&String, &DepSpec)> = entry.dependencies.iter().collect();
    children.sort_by_key(|(k, _)| k.to_lowercase());

    stack.push(key);
    let child_prefix = format!("{}{}", prefix, if last { "    " } else { "│   " });
    for (i, (dep_name, dep_spec)) in children.iter().enumerate() {
        let dep_last = i + 1 == children.len();
        // Dep versions in the lockfile are exact, so match them verbatim.
        let dep_entry = get_ci(&lockfile.packages, dep_name)
            .and_then(|entries| entries.iter().find(|e| e.version == dep_spec.version));
        render_dep(lockfile, dep_name, dep_spec, dep_entry, &child_prefix, dep_last, stack, out);
    }
    stack.pop();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(name: &str, version: &str) -> (String, DepSpec) {
        (
            name.to_string(),
            DepSpec { alias: digest_package_name(name).name, version: version.to_string() },
        )
    }

    fn entry(version: &str, location: &str, deps: &[(&str, &str)]) -> LockfileEntry {
        LockfileEntry {
            version: version.to_string(),
            integrity: String::new(),
            public: true,
            root: String::new(),
            location: location.to_string(),
            dependencies: deps.iter().map(|(n, v)| dep(n, v)).collect(),
        }
    }

    fn lockfile(packages: Vec<(&str, Vec<LockfileEntry>)>) -> LockFile {
        LockFile {
            file_version: 2,
            packages: packages.into_iter().map(|(n, e)| (n.to_string(), e)).collect(),
        }
    }

    fn roots(pairs: &[(&str, &str)]) -> HashMap<String, DepSpec> {
        pairs.iter().map(|(n, v)| dep(n, v)).collect()
    }

    fn render_plain(label: &str, roots: &HashMap<String, DepSpec>, lf: &LockFile) -> String {
        colored::control::set_override(false);
        render_tree(label, roots, lf)
    }

    #[test]
    fn nested_tree_renders_with_per_parent_versions() {
        // signal is pinned at 2.1.0 for the root but networking holds 1.8.2.
        let lf = lockfile(vec![
            ("forest/signal", vec![
                entry("2.1.0", "~", &[]),
                entry("1.8.2", "networking", &[]),
            ]),
            ("studio/networking", vec![
                entry("3.0.0", "~", &[("forest/signal", "1.8.2")]),
            ]),
        ]);
        let r = roots(&[("forest/signal", "^2.0.0"), ("studio/networking", "^3.0.0")]);
        assert_eq!(
            render_plain("studio/game@1.0.0", &r, &lf),
            "studio/game@1.0.0\n\
             ├── forest/signal@2.1.0\n\
             └── studio/networking@3.0.0\n    \
                 └── forest/signal@1.8.2\n"
        );
    }

    #[test]
    fn missing_lockfile_entry_is_flagged() {
        let lf = lockfile(vec![]);
        let r = roots(&[("a/b", "^1.0.0")]);
        assert_eq!(
            render_plain("proj", &r, &lf),
            "proj\n└── a/b@^1.0.0 (not installed)\n"
        );
    }

    #[test]
    fn explicit_alias_is_shown() {
        let lf = lockfile(vec![("a/foo", vec![entry("1.0.0", "~", &[])])]);
        let mut r = HashMap::new();
        r.insert(
            "a/foo".to_string(),
            DepSpec { alias: "Bar".to_string(), version: "^1.0.0".to_string() },
        );
        assert_eq!(
            render_plain("proj", &r, &lf),
            "proj\n└── a/foo@1.0.0 (as Bar)\n"
        );
    }

    #[test]
    fn circular_deps_do_not_recurse_forever() {
        let lf = lockfile(vec![
            ("a/x", vec![entry("1.0.0", "~", &[("b/y", "1.0.0")])]),
            ("b/y", vec![entry("1.0.0", "x", &[("a/x", "1.0.0")])]),
        ]);
        let r = roots(&[("a/x", "^1.0.0")]);
        assert_eq!(
            render_plain("proj", &r, &lf),
            "proj\n\
             └── a/x@1.0.0\n    \
                 └── b/y@1.0.0\n        \
                     └── a/x@1.0.0 (circular)\n"
        );
    }

    #[test]
    fn lockfile_key_casing_differences_still_match() {
        let lf = lockfile(vec![("Scope/Pkg", vec![entry("1.2.0", "~", &[])])]);
        let r = roots(&[("scope/pkg", "^1.0.0")]);
        assert_eq!(
            render_plain("proj", &r, &lf),
            "proj\n└── scope/pkg@1.2.0\n"
        );
    }
}
