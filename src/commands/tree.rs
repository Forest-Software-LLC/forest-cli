use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::Result;
use colored::Colorize;
use serde_json::Value;

use crate::lockfile_gen::{lockfile_satisfies_manifest, LockFile};
use crate::lockfile_solver::{DepSpec, LockfileEntry};
use crate::message::{info, warn};
use crate::utils::{digest_package_name, get_ci, normalize_forest_deps, normalize_forest_excludes, normalize_forest_overrides, resolve_dep_ref, DepRef};

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

    // Active local links change what's really on disk without touching the
    // lockfile this tree renders from; surface them before the tree.
    let link_res = crate::links::resolve_active(&roots);
    for warning in &link_res.warnings {
        warn(warning);
    }
    crate::links::print_banner(&link_res.active, |name| {
        get_ci(&lockfile.packages, name)
            .and_then(|entries| entries.iter().find(|e| e.location == "~"))
            .map(|e| e.version.clone())
    });

    // Same trust check install uses (UEFN widens the roots to the whole
    // workspace). A stale lockfile still prints since it's what is on disk.
    let overrides = normalize_forest_overrides(&manifest);
    let excludes = normalize_forest_excludes(&manifest);
    // A package can be both a direct dep and overridden (the override
    // predating `install <pkg>`); the override only rewrites transitive
    // edges, so call the split out instead of letting the tree imply
    // the root is pinned too.
    for key in overrides.keys() {
        if get_ci(&roots, key).is_some() {
            warn(&format!(
                "Override for {} applies only to transitive occurrences; the direct dependency keeps its declared range.",
                key
            ));
        }
    }
    let platform = crate::platform::Platform::from_manifest(&manifest)?;
    let resolution_roots = platform.resolution_roots(roots)?;
    if !lockfile_satisfies_manifest(&lockfile, &resolution_roots, &overrides, &excludes) {
        warn("forest.json changed since the last install; run `forest install` to refresh the tree.");
    }

    print!("{}", render_tree(&project_label(&manifest), &display_roots, &lockfile, &overrides, &excludes, platform.uses_pointer_files()));
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

fn render_tree(label: &str, roots: &HashMap<String, DepSpec>, lockfile: &LockFile, overrides: &HashMap<String, String>, excludes: &HashMap<String, String>, mark_pointers: bool) -> String {
    let mut out = format!("{}\n", label.bold());
    let mut sorted: Vec<(&String, &DepSpec)> = roots.iter().collect();
    sorted.sort_by_key(|(k, _)| k.to_lowercase());

    let mut stack = Vec::new();
    let mut saw_pointer = false;
    for (i, (name, spec)) in sorted.iter().enumerate() {
        let last = i + 1 == sorted.len();
        let entry = find_root_entry(lockfile, name, &spec.version);
        render_dep(lockfile, overrides, excludes, name, spec, entry, "", "~", last, true, mark_pointers, &mut saw_pointer, &mut stack, &mut out);
    }
    if saw_pointer {
        out.push_str(&format!("{}\n", "↪ pointer to the physical copy elsewhere in the tree".dimmed()));
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
    overrides: &HashMap<String, String>,
    excludes: &HashMap<String, String>,
    name: &str,
    spec: &DepSpec,
    entry: Option<&LockfileEntry>,
    prefix: &str,
    loc: &str,
    last: bool,
    is_root: bool,
    mark_pointers: bool,
    saw_pointer: &mut bool,
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
    // An occurrence away from its entry's recorded location is materialized
    // as a pointer module, not a physical copy (Roblox hoisting).
    if mark_pointers {
        if let Some(e) = entry {
            if !e.location.eq_ignore_ascii_case(loc) {
                label.push_str(&format!(" {}", "↪"));
                *saw_pointer = true;
            }
        }
    }
    // Call out deps installed under a different folder name than their own.
    if spec.alias != digest_package_name(name).name {
        label.push_str(&format!(" {}", format!("(as {})", spec.alias).dimmed()));
    }
    // Overrides rewrite transitive edges only; a root occurrence keeps its
    // declared range, so tagging it would claim a pin that isn't applied.
    if !is_root {
        if let Some(range) = get_ci(overrides, name) {
            label.push_str(&format!(" {}", format!("(overridden: {})", range).yellow()));
        }
    }
    // Exclusions are uniform (they filter roots too), so every occurrence
    // gets the tag.
    if let Some(range) = get_ci(excludes, name) {
        label.push_str(&format!(" {}", format!("(excluding {})", range).yellow()));
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
    // Children's location = this node's location + this node's alias, same
    // construction as the solver's build_tree.
    let child_loc = format!("{}/{}", loc, spec.alias);
    for (i, (dep_name, dep_spec)) in children.iter().enumerate() {
        let dep_last = i + 1 == children.len();
        // Dep versions in the lockfile are exact, so match them verbatim.
        let dep_entry = get_ci(&lockfile.packages, dep_name)
            .and_then(|entries| entries.iter().find(|e| e.version == dep_spec.version));
        render_dep(lockfile, overrides, excludes, dep_name, dep_spec, dep_entry, &child_prefix, &child_loc, dep_last, false, mark_pointers, saw_pointer, stack, out);
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
            packages_dir: "Packages".to_string(),
            dependencies: deps.iter().map(|(n, v)| dep(n, v)).collect(),
        }
    }

    fn lockfile(packages: Vec<(&str, Vec<LockfileEntry>)>) -> LockFile {
        LockFile {
            file_version: 2,
            overrides: HashMap::new(),
            excludes: HashMap::new(),
            packages: packages.into_iter().map(|(n, e)| (n.to_string(), e)).collect(),
        }
    }

    fn roots(pairs: &[(&str, &str)]) -> HashMap<String, DepSpec> {
        pairs.iter().map(|(n, v)| dep(n, v)).collect()
    }

    fn render_plain(label: &str, roots: &HashMap<String, DepSpec>, lf: &LockFile) -> String {
        colored::control::set_override(false);
        render_tree(label, roots, lf, &HashMap::new(), &HashMap::new(), false)
    }

    fn map(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn root_occurrence_of_an_overridden_package_is_not_tagged() {
        // signal is both a direct dep (keeps its own range -> 2.0.3) and a
        // transitive dep of comm (override applies -> 1.5.0). Only the
        // transitive node carries the tag.
        colored::control::set_override(false);
        let lf = lockfile(vec![
            ("sleitnick/signal", vec![
                entry("2.0.3", "~", &[]),
                entry("1.5.0", "Comm", &[]),
            ]),
            ("sleitnick/comm", vec![
                entry("1.0.1", "~", &[("sleitnick/signal", "1.5.0")]),
            ]),
        ]);
        let r = roots(&[("sleitnick/comm", "^1.0.0"), ("sleitnick/signal", "^2.0.0")]);
        let overrides = map(&[("sleitnick/signal", "^1.0.0")]);
        assert_eq!(
            render_tree("proj", &r, &lf, &overrides, &HashMap::new(), false),
            "proj\n\
             ├── sleitnick/comm@1.0.1\n\
             │   └── sleitnick/signal@1.5.0 (overridden: ^1.0.0)\n\
             └── sleitnick/signal@2.0.3\n"
        );
    }

    #[test]
    fn excluded_packages_are_tagged_everywhere_including_roots() {
        // Exclusions filter roots too, so the tag is uniform.
        colored::control::set_override(false);
        let lf = lockfile(vec![
            ("a/x", vec![entry("1.0.0", "~", &[("b/y", "1.5.2")])]),
            ("b/y", vec![
                entry("1.5.2", "x", &[]),
                entry("1.5.2", "~", &[]),
            ]),
        ]);
        let r = roots(&[("a/x", "^1.0.0"), ("b/y", "^1.0.0")]);
        let excludes = map(&[("b/y", "=1.6.0")]);
        assert_eq!(
            render_tree("proj", &r, &lf, &HashMap::new(), &excludes, false),
            "proj\n\
             ├── a/x@1.0.0\n\
             │   └── b/y@1.5.2 (excluding =1.6.0)\n\
             └── b/y@1.5.2 (excluding =1.6.0)\n"
        );
    }

    #[test]
    fn overridden_packages_are_tagged_wherever_they_appear() {
        colored::control::set_override(false);
        let lf = lockfile(vec![
            ("a/x", vec![entry("1.0.0", "~", &[("b/y", "2.1.2")])]),
            ("b/y", vec![entry("2.1.2", "x", &[])]),
        ]);
        let r = roots(&[("a/x", "^1.0.0")]);
        let overrides = map(&[("b/y", "^2.0.0")]);
        assert_eq!(
            render_tree("proj", &r, &lf, &overrides, &HashMap::new(), false),
            "proj\n\
             └── a/x@1.0.0\n    \
                 └── b/y@2.1.2 (overridden: ^2.0.0)\n"
        );
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
    fn pointer_occurrences_are_marked_and_physical_ones_are_not() {
        // signal is physically installed at "~/knit" (hoisted); comm's own
        // occurrence of it is a pointer module. Roots and the physical
        // occurrence stay unmarked; a legend explains the arrow.
        colored::control::set_override(false);
        let lf = lockfile(vec![
            ("sleitnick/knit", vec![
                entry("1.7.0", "~", &[("sleitnick/comm", "1.0.1"), ("sleitnick/signal", "2.0.3")]),
            ]),
            ("sleitnick/comm", vec![
                entry("1.0.1", "~/knit", &[("sleitnick/signal", "2.0.3")]),
            ]),
            ("sleitnick/signal", vec![
                entry("2.0.3", "~/knit", &[]),
            ]),
        ]);
        let r = roots(&[("sleitnick/knit", "^1.7.0")]);
        assert_eq!(
            render_tree("proj", &r, &lf, &HashMap::new(), &HashMap::new(), true),
            "proj\n\
             └── sleitnick/knit@1.7.0\n    \
                 ├── sleitnick/comm@1.0.1\n    \
                 │   └── sleitnick/signal@2.0.3 ↪\n    \
                 └── sleitnick/signal@2.0.3\n\
             ↪ pointer to the physical copy elsewhere in the tree\n"
        );
    }

    #[test]
    fn uefn_trees_never_mark_pointers() {
        // Same lockfile shape, flag off (UEFN installs everything physically
        // once): no arrows, no legend.
        colored::control::set_override(false);
        let lf = lockfile(vec![
            ("a/x", vec![entry("1.0.0", "~", &[("b/y", "1.0.0")])]),
            ("b/y", vec![entry("1.0.0", "~/other", &[])]),
        ]);
        let r = roots(&[("a/x", "^1.0.0")]);
        let out = render_tree("proj", &r, &lf, &HashMap::new(), &HashMap::new(), false);
        assert!(!out.contains('↪'));
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
