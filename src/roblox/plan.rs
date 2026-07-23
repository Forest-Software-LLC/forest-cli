use std::collections::HashMap;

use anyhow::{anyhow, Result};
use urlencoding::encode;

use crate::lockfile_gen::LockFile;
use crate::roblox::PACKAGES_DIR;
use crate::lockfile_solver::DepSpec;
use crate::utils::get_ci;

/// One package version and the physical directory it installs into.
#[derive(Debug, Clone)]
pub struct PlannedPackage {
    /// Physical install dir, forward slashes, e.g. `./Packages/Knit/Packages/Comm`.
    pub path: String,
    /// Canonical lockfile key, `scope/name`.
    pub name: String,
    pub version: String,
    /// Lockfile sha256 — both the identity key and the download address.
    pub integrity: String,
    /// The registry's archiveRoot (layout-affecting on extraction).
    pub root: String,
    pub public: bool,
}

/// One generated pointer module bridging a nested Packages/ entry to where
/// the dependency physically lives after dedupe/hoisting.
#[derive(Debug, Clone)]
pub struct PlannedPointer {
    /// Pointer module dir, e.g. `./Packages/Knit/Packages/Promise`.
    pub dir: String,
    /// Contents of the `init.lua` inside that dir.
    pub init_lua: String,
}

/// The pure output of install planning: every physical package path and every
/// pointer file the lockfile implies. No IO, no network — `make_directories`
/// executes this against the filesystem, and reconciliation diffs it against
/// what a previous install recorded.
#[derive(Debug, Default)]
pub struct InstallPlan {
    pub packages: Vec<PlannedPackage>,
    pub pointers: Vec<PlannedPointer>,
}

/// Compute the full install layout for a lockfile. Logic moved verbatim from
/// `make_directories` — the behavior is pinned by the tests below.
pub fn plan_install(lockfile: &LockFile, root_deps: &HashMap<String, DepSpec>) -> Result<InstallPlan> {
    let mut plan = InstallPlan::default();

    // path_cache[pkg][version] = physical dir, needed by pointer planning.
    let mut path_cache: HashMap<String, HashMap<String, String>> = HashMap::new();

    for (pkg_name, versions) in &lockfile.packages {
        for version_data in versions {
            let mut path_parts: Vec<&str> = version_data.location.split('/').collect();
            path_parts.remove(0);

            // A hand-edited manifest key may differ in casing only.
            let mut dir_pkg_name = get_ci(root_deps, pkg_name)
                .map(|d| d.alias.clone())
                .unwrap_or_else(String::new);

            if !path_parts.is_empty() {
                let target_root_dep_alias = path_parts[0];
                let mut root_dep_name = String::new();

                for (dep_name, dep_info) in root_deps {
                    // Aliased folder names case-fold on Windows/macOS so match them case-insensitively
                    if dep_info.alias.eq_ignore_ascii_case(target_root_dep_alias) {
                        root_dep_name = dep_name.clone();
                        break;
                    }
                }

                let root_dep_ver = &get_ci(&lockfile.packages, &root_dep_name)
                    .ok_or_else(|| anyhow!("Root dependency {} not found", root_dep_name))?
                    .iter()
                    .find(|v| v.location == "~")
                    .ok_or_else(|| anyhow!("Root Version not found for {}", root_dep_name))?;

                dir_pkg_name = backtrack_name(
                    lockfile,
                    &root_dep_name,
                    &root_dep_ver.version,
                    pkg_name,
                    &path_parts.clone()[1..],
                )?;
            } else if dir_pkg_name.is_empty() {
                return Err(anyhow!("Failed to determine directory package name for {}", pkg_name));
            }

            let nested_sep = format!("/{}/", PACKAGES_DIR);
            let mut path: String = format!("./{}/{}/{}", PACKAGES_DIR, path_parts.join(&nested_sep), PACKAGES_DIR);
            if path_parts.is_empty() {
                path = format!("./{}", PACKAGES_DIR);
            }

            let full_path = format!("{}/{}", path, dir_pkg_name);
            plan.packages.push(PlannedPackage {
                path: full_path.clone(),
                name: pkg_name.clone(),
                version: version_data.version.clone(),
                integrity: version_data.integrity.clone(),
                root: version_data.root.clone(),
                public: version_data.public,
            });

            path_cache.entry(pkg_name.clone())
                .or_default()
                .insert(version_data.version.clone(), full_path);
        }
    }

    // Plan pointer files
    for (pkg_name, versions) in &lockfile.packages {
        for version_data in versions {
            let cache_result = path_cache
                .get(pkg_name)
                .and_then(|v| v.get(&version_data.version))
                .ok_or_else(|| anyhow!("Path for {} @ {} not found in cache.", pkg_name, version_data.version))?;

            let mut true_path = cache_result.clone();
            true_path.push_str(&format!("/{}", PACKAGES_DIR));

            for (dep_name, dep_version) in &version_data.dependencies {
                let dep_true_path = path_cache
                    .get(dep_name)
                    .and_then(|v| v.get(&dep_version.version))
                    .ok_or_else(|| anyhow!("Path for dependency {} @ {} not found in cache.", dep_name, &dep_version.version))?;

                if let Some((pointer_dir, init_lua)) = plan_pointer(&true_path, &dep_version.alias, dep_true_path) {
                    plan.pointers.push(PlannedPointer {
                        dir: format!("{}/{}", true_path, pointer_dir),
                        init_lua,
                    });
                }
            }
        }
    }

    Ok(plan)
}

/// Resolve alias path for a package by backtracking through the dependency
/// chain its location string describes.
///
/// ex: pkg loc /a/n/c/d — look for the root dep aliased "a", then follow
/// aliases "n", "c" through each dependency's recorded deps until the end
/// goal package is found; its recorded alias is the directory name.
fn backtrack_name(
    lockfile: &LockFile,
    dep_name: &str,
    dep_version: &str,
    end_goal: &str,
    remaining_segments: &[&str],
) -> Result<String> {
    // First call passes a manifest key; lockfile keys are canonical
    let deps = get_ci(&lockfile.packages, dep_name)
        .ok_or_else(|| anyhow!("Dependency {} not found", dep_name))?;

    // Find the exact version entry
    let ver = deps
        .iter()
        .find(|v| v.version == dep_version)
        .ok_or_else(|| anyhow!("Version {} for {} not found", dep_version, dep_name))?;

    // Recurse through dependencies to match alias path
    let is_empty = remaining_segments.is_empty();
    for (sub_dep_name, sub_dep_spec) in &ver.dependencies {
        if is_empty {
            if sub_dep_name == end_goal {
                return Ok(sub_dep_spec.alias.clone());
            }
        } else if sub_dep_spec.alias.eq_ignore_ascii_case(remaining_segments[0]) {
            return backtrack_name(
                lockfile,
                sub_dep_name,
                &sub_dep_spec.version,
                end_goal,
                &remaining_segments[1..],
            );
        }
    }

    Err(anyhow!("Failed to backtrack package name"))
}

/// First line of every generated pointer init.lua. Doubles as the on-disk
/// signature that lets reconciliation recognize (and safely delete) stale
/// pointer dirs without any central bookkeeping.
pub(crate) const POINTER_HEADER: &str = "--Pointer file generated by Forest Package Manager.";

/// Plan the pointer module bridging a package's nested Packages/ entry to
/// wherever the dependency physically lives after dedupe/hoisting. Returns
/// the directory name to create under `true_path` and the init.lua source,
/// or None when the dependency is already physically at that level.
///
/// The directory is named by the dependency's ALIAS: package code requires
/// `script…Packages[alias]`, and physically-placed siblings are already
/// alias-named — pointers must match.
pub(crate) fn plan_pointer(true_path: &str, dep_alias: &str, dep_true_path: &str) -> Option<(String, String)> {
    let dep_parts: Vec<&str> = dep_true_path.split('/').collect();
    let true_parts: Vec<&str> = true_path.split('/').collect();

    // No pointer only when the dep physically sits INSIDE this container
    // (alias-named dir). Same depth is not enough: a dep living at equal
    // depth in a DIFFERENT branch (two roots sharing a nested transitive
    // dep) still needs a pointer, or the consumer's require breaks.
    if dep_parts.len() == true_parts.len() + 1 && dep_parts[..true_parts.len()] == true_parts[..] {
        return None;
    }

    // Count shared ancestors on dep_true_path and true_path
    let mut shared_ancestors: u16 = 0;
    for (d, t) in dep_parts.iter().zip(true_parts.iter()) {
        if d == t {
            shared_ancestors += 1;
        } else {
            break;
        }
    }

    // From the pointer module (script = its own folder), climb one Parent per
    // unshared segment of true_path to reach the deepest shared container,
    // then bracket-index down the dep's unshared segments.
    let parent_count = true_parts.len() - shared_ancestors as usize;
    let lua_path = format!("script.Parent{}", (".Parent").repeat(parent_count));
    let relative_path = dep_parts
        .iter()
        .skip(shared_ancestors as usize)
        .map(|s| encode(s).into_owned())
        .collect::<Vec<String>>()
        .join("']['");

    let mut init_lua = String::new();
    init_lua.push_str(POINTER_HEADER);
    init_lua.push('\n');
    init_lua.push_str(&format!("return require({}['{}'])", lua_path, relative_path));

    Some((dep_alias.to_string(), init_lua))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockfile_solver::LockfileEntry;

    #[test]
    fn same_level_dependency_needs_no_pointer() {
        // Dep physically inside this package's own Packages dir.
        let plan = plan_pointer(
            "./Packages/Knit/Packages",
            "Comm",
            "./Packages/Knit/Packages/Comm",
        );
        assert!(plan.is_none());
    }

    #[test]
    fn hoisted_dependency_gets_an_alias_named_pointer() {
        // Dep hoisted to the root container: the pointer dir must be the
        // alias, never the canonical "Scope/Name" key — a slash would nest
        // a bogus Scope/ directory the require chain doesn't expect.
        let (dir, init_lua) = plan_pointer(
            "./Packages/Knit/Packages",
            "Promise",
            "./Packages/Promise",
        )
        .expect("hoisted dep needs a pointer");
        assert_eq!(dir, "Promise");
        assert!(!dir.contains('/'));
        // From ./Packages/Knit/Packages/Promise/init.lua: climb to the root
        // container (3 Parents), then index the physical dir.
        assert_eq!(
            init_lua,
            "--Pointer file generated by Forest Package Manager.\nreturn require(script.Parent.Parent.Parent['Promise'])"
        );
    }

    #[test]
    fn two_level_hoist_builds_the_full_bracket_chain() {
        let (dir, init_lua) = plan_pointer(
            "./Packages/A/Packages/B/Packages",
            "Deep",
            "./Packages/Deep",
        )
        .expect("pointer expected");
        assert_eq!(dir, "Deep");
        assert!(init_lua.ends_with("return require(script.Parent.Parent.Parent.Parent.Parent['Deep'])"));
    }

    #[test]
    fn same_depth_in_another_branch_still_gets_a_pointer() {
        // B physically lives under root A; root C also depends on it. The
        // paths have equal depth but different containers — C must get a
        // pointer that crosses to A's branch (this returned None before).
        let (dir, init_lua) = plan_pointer(
            "./Packages/C/Packages",
            "B",
            "./Packages/A/Packages/B",
        )
        .expect("cross-branch dep needs a pointer");
        assert_eq!(dir, "B");
        // From ./Packages/C/Packages/B/init.lua: climb to the root container
        // (3 Parents), then walk down A's branch.
        assert_eq!(
            init_lua,
            "--Pointer file generated by Forest Package Manager.\nreturn require(script.Parent.Parent.Parent['A']['Packages']['B'])"
        );
    }

    // ---- plan_install: pins the layout the old make_directories produced ----

    fn dep(name: &str, alias: &str, version: &str) -> (String, DepSpec) {
        (name.to_string(), DepSpec { alias: alias.to_string(), version: version.to_string() })
    }

    fn entry(
        version: &str,
        location: &str,
        integrity: &str,
        deps: Vec<(String, DepSpec)>,
    ) -> LockfileEntry {
        LockfileEntry {
            version: version.to_string(),
            integrity: integrity.to_string(),
            public: true,
            root: "src/init.luau".to_string(),
            location: location.to_string(),
            dependencies: deps.into_iter().collect(),
        }
    }

    /// Root Knit (nested dep Comm, hoisted dep Promise), root Promise.
    fn synthetic_lockfile() -> (LockFile, HashMap<String, DepSpec>) {
        let mut packages = HashMap::new();
        packages.insert(
            "acme/knit".to_string(),
            vec![entry(
                "1.0.0",
                "~",
                "aa11",
                vec![dep("acme/comm", "Comm", "1.0.0"), dep("acme/promise", "Promise", "2.0.0")],
            )],
        );
        packages.insert(
            "acme/comm".to_string(),
            vec![entry(
                "1.0.0",
                "~/Knit",
                "bb22",
                vec![dep("acme/promise", "Promise", "2.0.0")],
            )],
        );
        packages.insert(
            "acme/promise".to_string(),
            vec![entry("2.0.0", "~", "cc33", vec![])],
        );

        let root_deps: HashMap<String, DepSpec> = [
            dep("acme/knit", "Knit", "^1.0.0"),
            dep("acme/promise", "Promise", "^2.0.0"),
        ]
        .into_iter()
        .collect();

        (LockFile { file_version: 2, packages }, root_deps)
    }

    #[test]
    fn plans_root_nested_and_hoisted_paths() {
        let (lockfile, root_deps) = synthetic_lockfile();
        let plan = plan_install(&lockfile, &root_deps).unwrap();

        let mut paths: Vec<(String, String)> = plan
            .packages
            .iter()
            .map(|p| (p.name.clone(), p.path.clone()))
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            vec![
                ("acme/comm".to_string(), "./Packages/Knit/Packages/Comm".to_string()),
                ("acme/knit".to_string(), "./Packages/Knit".to_string()),
                ("acme/promise".to_string(), "./Packages/Promise".to_string()),
            ]
        );

        // Integrity/root/public flow through untouched.
        let knit = plan.packages.iter().find(|p| p.name == "acme/knit").unwrap();
        assert_eq!((knit.version.as_str(), knit.integrity.as_str(), knit.root.as_str(), knit.public),
                   ("1.0.0", "aa11", "src/init.luau", true));
    }

    #[test]
    fn plans_pointers_only_for_hoisted_deps() {
        let (lockfile, root_deps) = synthetic_lockfile();
        let plan = plan_install(&lockfile, &root_deps).unwrap();

        let mut dirs: Vec<String> = plan.pointers.iter().map(|p| p.dir.clone()).collect();
        dirs.sort();
        // Comm is physically inside Knit (no pointer); Promise is hoisted to
        // the root, so both Knit and Comm need pointer modules to reach it.
        assert_eq!(
            dirs,
            vec![
                "./Packages/Knit/Packages/Comm/Packages/Promise".to_string(),
                "./Packages/Knit/Packages/Promise".to_string(),
            ]
        );

        let knit_ptr = plan.pointers.iter().find(|p| p.dir == "./Packages/Knit/Packages/Promise").unwrap();
        assert_eq!(
            knit_ptr.init_lua,
            "--Pointer file generated by Forest Package Manager.\nreturn require(script.Parent.Parent.Parent['Promise'])"
        );
    }

    #[test]
    fn missing_root_alias_is_an_error() {
        let (lockfile, _) = synthetic_lockfile();
        // Root deps that know nothing about the lockfile's tree.
        let bogus: HashMap<String, DepSpec> = HashMap::new();
        assert!(plan_install(&lockfile, &bogus).is_err());
    }

    #[test]
    fn shared_nested_dep_plans_a_cross_branch_pointer() {
        // Roots A and C both depend on B; dedupe placed B physically under A
        // (location "~/A"). C's tree must bridge to it with a pointer — the
        // old depth-only check planned NO pointer here, silently breaking
        // C's require at runtime.
        let mut packages = HashMap::new();
        packages.insert(
            "acme/a".to_string(),
            vec![entry("1.0.0", "~", "aa", vec![dep("acme/b", "B", "1.0.0")])],
        );
        packages.insert(
            "acme/c".to_string(),
            vec![entry("1.0.0", "~", "cc", vec![dep("acme/b", "B", "1.0.0")])],
        );
        packages.insert(
            "acme/b".to_string(),
            vec![entry("1.0.0", "~/A", "bb", vec![])],
        );
        let root_deps: HashMap<String, DepSpec> =
            [dep("acme/a", "A", "^1.0.0"), dep("acme/c", "C", "^1.0.0")].into_iter().collect();
        let lockfile = LockFile { file_version: 2, packages };

        let plan = plan_install(&lockfile, &root_deps).unwrap();

        let b = plan.packages.iter().find(|p| p.name == "acme/b").unwrap();
        assert_eq!(b.path, "./Packages/A/Packages/B", "B physically lives under A");

        // Exactly one pointer: A holds B physically (no pointer), C bridges.
        let dirs: Vec<&str> = plan.pointers.iter().map(|p| p.dir.as_str()).collect();
        assert_eq!(dirs, vec!["./Packages/C/Packages/B"]);
        assert!(
            plan.pointers[0].init_lua.ends_with("return require(script.Parent.Parent.Parent['A']['Packages']['B'])"),
            "unexpected chain: {}",
            plan.pointers[0].init_lua
        );
    }
}
