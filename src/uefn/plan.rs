//! Pure flat install planner for UEFN.
//!
//! Verse's module system removes everything the Roblox planner exists for:
//! no hoisting, no nesting, no pointer shims - every package lands at
//! `./<Mount>/<MappedScope>/<Name>` and portability comes from that shared
//! mount convention (docs/uefn-adapter.md §4/§5). What the planner adds
//! instead is MARKERS: implicit Verse folder modules are `<internal>` by
//! default, so the CLI generates `<public>` declarations (scope level +
//! direct deps) and `<scoped {…/Mount}>` declarations (transitive-only deps
//! - compiler-level phantom-dependency prevention).
//!
//! Layout planning and marker generation are kept as distinct steps: the
//! layout half is platform-agnostic flat-tree logic a future platform could
//! lift; the markers are Verse-specific (rule of three - no trait yet).
//!
//! The lockfile's `location` strings are Roblox hoisting metadata and are
//! deliberately IGNORED here.

use crate::lockfile_gen::LockFile;
use crate::lockfile_solver::DepSpec;
use anyhow::{anyhow, Result};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug)]
pub struct UefnPlannedPackage {
    /// Plan-format path, forward slashes: "./ForestPackages/<MappedScope>/<Name>"
    pub path: String,
    /// Canonical lockfile key, "scope/name" (lowercased by the solver).
    pub name: String,
    pub version: String,
    pub integrity: String,
    pub public: bool,
    /// Present in the manifest's own dependencies (=> `<public>` marker).
    /// The marker list already encodes the outcome; kept for tests and
    /// future summary output.
    #[allow(dead_code)]
    pub direct: bool,
}

#[derive(Debug)]
pub struct PlannedMarker {
    /// Plan-format path: "./ForestPackages/X.verse" | "./ForestPackages/X/Y.verse"
    pub file: String,
    pub content: String,
}

#[derive(Debug)]
pub struct UefnInstallPlan {
    pub packages: Vec<UefnPlannedPackage>,
    pub markers: Vec<PlannedMarker>,
    /// Non-fatal notes surfaced by the executor (e.g. an authored folder
    /// whose name can't be a Verse identifier gets no marker).
    pub warnings: Vec<String>,
}

/// Recover the display casing of a package's name segment. Lockfile keys are
/// lowercased by the solver, but the folder name becomes the Verse module
/// identifier consumers import, so casing matters. The manifest's dep keys
/// and every entry's dependency map carry registry-canonical casing.
fn canonical_name_segment(
    lock_key: &str,
    root_deps: &HashMap<String, DepSpec>,
    lockfile: &LockFile,
) -> String {
    fn name_from<'a>(keys: impl Iterator<Item = &'a String>, lock_key: &str) -> Option<String> {
        for key in keys {
            if key.eq_ignore_ascii_case(lock_key) {
                return key.split('/').nth(1).map(str::to_string);
            }
        }
        None
    }
    name_from(root_deps.keys(), lock_key)
        .or_else(|| {
            lockfile
                .packages
                .values()
                .flatten()
                .find_map(|entry| name_from(entry.dependencies.keys(), lock_key))
        })
        .unwrap_or_else(|| lock_key.split('/').nth(1).unwrap_or(lock_key).to_string())
}

pub fn plan_install_uefn(
    lockfile: &LockFile,
    root_deps: &HashMap<String, DepSpec>,
    verse_path: &str,
    authored: &[(String, String)], // (mapped_scope, dir_name)
) -> Result<UefnInstallPlan> {
    let mount = &crate::contracts::verse_rules().packages_mount;
    let mut warnings = Vec::new();

    // Aliases need per-symbol re-export shims Verse doesn't have - rejected
    // for UEFN (docs/uefn-adapter.md §1/§10). Disambiguate with qualified
    // access instead: (ForestPackages.scope.name:)Fn(...).
    for (pkg_name, spec) in root_deps {
        let default_alias = pkg_name.split('/').nth(1).unwrap_or(pkg_name);
        if spec.alias != default_alias {
            return Err(anyhow!(
                "Dependency aliases aren't supported on UEFN ({} is aliased to '{}'). Verse imports \
                 are already scope-namespaced - remove the alias and disambiguate call sites with \
                 qualified access: ({}.<scope>.<name>:)Fn(...)",
                pkg_name, spec.alias, mount
            ));
        }
    }

    let mut packages = Vec::new();
    // BTreeMap for deterministic marker ordering (stable output across runs).
    let mut name_markers: BTreeMap<String, String> = BTreeMap::new();
    let mut scope_markers: BTreeMap<String, ()> = BTreeMap::new();
    let scoped_subtree = format!("{}/{}", verse_path, mount);

    let mut lock_keys: Vec<&String> = lockfile.packages.keys().collect();
    lock_keys.sort();
    for lock_key in lock_keys {
        let entries = &lockfile.packages[lock_key];
        if entries.is_empty() {
            continue;
        }
        // Verse module paths carry no version: exactly one version of a
        // package can exist per project. The Roblox planner resolves range
        // conflicts by nesting; UEFN cannot - fail with the requesters named.
        if entries.len() > 1 {
            let versions: Vec<&str> = entries.iter().map(|e| e.version.as_str()).collect();
            let mut requesters = Vec::new();
            for (dep_key, spec) in root_deps {
                if dep_key.eq_ignore_ascii_case(lock_key) {
                    requesters.push(format!("forest.json ({})", spec.version));
                }
            }
            for (parent_key, parent_entries) in &lockfile.packages {
                for parent in parent_entries {
                    for (dep_key, spec) in &parent.dependencies {
                        if dep_key.eq_ignore_ascii_case(lock_key) {
                            requesters.push(format!(
                                "{}@{} ({})",
                                parent_key, parent.version, spec.version
                            ));
                        }
                    }
                }
            }
            return Err(anyhow!(
                "{} resolves to multiple versions ({}) - UEFN installs one version per package \
                 (Verse module paths carry no version). Requested by: {}. Align the version \
                 ranges or remove one of the requesters.",
                lock_key,
                versions.join(", "),
                requesters.join("; ")
            ));
        }

        let entry = &entries[0];
        let scope = lock_key.split('/').next().unwrap_or(lock_key);
        let mapped_scope = super::map_scope_to_verse_identifier(scope);
        let name = canonical_name_segment(lock_key, root_deps, lockfile);
        let direct = root_deps.keys().any(|k| k.eq_ignore_ascii_case(lock_key));

        scope_markers.insert(mapped_scope.clone(), ());
        let access = if direct {
            super::MarkerAccess::Public
        } else {
            super::MarkerAccess::Scoped { subtree: scoped_subtree.clone() }
        };
        name_markers.insert(
            format!("./{}/{}/{}.verse", mount, mapped_scope, name),
            super::marker_content(&name, &access),
        );

        packages.push(UefnPlannedPackage {
            path: format!("./{}/{}/{}", mount, mapped_scope, name),
            name: lock_key.clone(),
            version: entry.version.clone(),
            integrity: entry.integrity.clone(),
            public: entry.public,
            direct,
        });
    }

    // Authored packages (forest.json, no receipt) get `<public>` markers too
    // - the folder is the author's, the markers are Forest's job either way.
    for (mapped_scope, dir_name) in authored {
        if super::validate_uefn_package_name(dir_name).is_err() {
            warnings.push(format!(
                "Authored package folder \"{}\" is not a valid Verse identifier - no module \
                 marker generated for it.",
                dir_name
            ));
            continue;
        }
        scope_markers.insert(mapped_scope.clone(), ());
        name_markers
            .entry(format!("./{}/{}/{}.verse", mount, mapped_scope, dir_name))
            .or_insert_with(|| super::marker_content(dir_name, &super::MarkerAccess::Public));
    }

    let mut markers: Vec<PlannedMarker> = scope_markers
        .keys()
        .map(|mapped_scope| PlannedMarker {
            file: format!("./{}/{}.verse", mount, mapped_scope),
            content: super::marker_content(mapped_scope, &super::MarkerAccess::Public),
        })
        .collect();
    markers.extend(
        name_markers.into_iter().map(|(file, content)| PlannedMarker { file, content }),
    );

    Ok(UefnInstallPlan { packages, markers, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lockfile_solver::LockfileEntry;

    fn dep(alias: &str, version: &str) -> DepSpec {
        DepSpec { alias: alias.to_string(), version: version.to_string() }
    }

    fn entry(version: &str, integrity: &str, deps: &[(&str, &str, &str)]) -> LockfileEntry {
        LockfileEntry {
            version: version.to_string(),
            integrity: integrity.to_string(),
            public: true,
            root: String::new(),
            location: "~".to_string(), // hoisting metadata - must be ignored
            dependencies: deps
                .iter()
                .map(|(k, a, v)| (k.to_string(), dep(a, v)))
                .collect(),
        }
    }

    fn lockfile(packages: Vec<(&str, Vec<LockfileEntry>)>) -> LockFile {
        LockFile {
            file_version: 2,
            packages: packages.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        }
    }

    fn root_deps(deps: &[(&str, &str, &str)]) -> std::collections::HashMap<String, DepSpec> {
        deps.iter().map(|(k, a, v)| (k.to_string(), dep(a, v))).collect()
    }

    #[test]
    fn flat_layout_with_direct_and_transitive_markers() {
        // Direct dep cool-studio/MathUtil pulls transitive other/Calc.
        let lf = lockfile(vec![
            ("cool-studio/mathutil", vec![entry("1.0.0", "aaa", &[("other/Calc", "Calc", "^2.0.0")])]),
            ("other/calc", vec![entry("2.1.0", "bbb", &[])]),
        ]);
        let roots = root_deps(&[("cool-studio/MathUtil", "MathUtil", "^1.0.0")]);
        let plan = plan_install_uefn(&lf, &roots, "/invaliddomain/Proj", &[]).unwrap();

        let math = plan.packages.iter().find(|p| p.name == "cool-studio/mathutil").unwrap();
        assert_eq!(math.path, "./ForestPackages/cool_studio/MathUtil");
        assert!(math.direct);
        let calc = plan.packages.iter().find(|p| p.name == "other/calc").unwrap();
        assert_eq!(calc.path, "./ForestPackages/other/Calc");
        assert!(!calc.direct);

        let marker_for = |file: &str| {
            plan.markers.iter().find(|m| m.file == file).map(|m| m.content.clone())
        };
        assert!(marker_for("./ForestPackages/cool_studio.verse")
            .unwrap()
            .contains("cool_studio<public>"));
        assert!(marker_for("./ForestPackages/cool_studio/MathUtil.verse")
            .unwrap()
            .contains("MathUtil<public>"));
        // Transitive-only dep is scoped to the mount subtree.
        assert!(marker_for("./ForestPackages/other/Calc.verse")
            .unwrap()
            .contains("Calc<scoped {/invaliddomain/Proj/ForestPackages}>"));
        assert!(plan.warnings.is_empty());
    }

    #[test]
    fn version_conflict_errors_naming_requesters() {
        let lf = lockfile(vec![
            ("acme/a", vec![entry("1.0.0", "a1", &[("acme/C", "C", "^1.0.0")])]),
            ("acme/b", vec![entry("3.1.0", "b1", &[("acme/C", "C", "^2.0.0")])]),
            ("acme/c", vec![entry("1.5.0", "c1", &[]), entry("2.0.0", "c2", &[])]),
        ]);
        let roots = root_deps(&[("acme/a", "a", "^1.0.0"), ("acme/b", "b", "^3.0.0")]);
        let err = plan_install_uefn(&lf, &roots, "/x/Y", &[]).unwrap_err().to_string();
        assert!(err.contains("acme/c"), "{}", err);
        assert!(err.contains("acme/a@1.0.0"), "{}", err);
        assert!(err.contains("acme/b@3.1.0"), "{}", err);
        assert!(err.contains("one version per package"), "{}", err);
    }

    #[test]
    fn alias_is_rejected_with_qualified_access_hint() {
        let lf = lockfile(vec![("acme/a", vec![entry("1.0.0", "a1", &[])])]);
        let roots = root_deps(&[("acme/a", "CoolAlias", "^1.0.0")]);
        let err = plan_install_uefn(&lf, &roots, "/x/Y", &[]).unwrap_err().to_string();
        assert!(err.contains("aliases aren't supported on UEFN"), "{}", err);
        assert!(err.contains("qualified access"), "{}", err);
    }

    #[test]
    fn scope_marker_dedupes_and_authored_packages_contribute() {
        let lf = lockfile(vec![
            ("cool-studio/a", vec![entry("1.0.0", "a1", &[])]),
            ("cool-studio/b", vec![entry("1.0.0", "b1", &[])]),
        ]);
        let roots = root_deps(&[("cool-studio/a", "a", "^1"), ("cool-studio/b", "b", "^1")]);
        let authored = vec![("mine".to_string(), "MyPkg".to_string())];
        let plan = plan_install_uefn(&lf, &roots, "/x/Y", &authored).unwrap();

        let scope_markers: Vec<&str> = plan
            .markers
            .iter()
            .filter(|m| m.file.matches('/').count() == 2) // "./Mount/X.verse"
            .map(|m| m.file.as_str())
            .collect();
        assert_eq!(scope_markers, vec!["./ForestPackages/cool_studio.verse", "./ForestPackages/mine.verse"]);
        assert!(plan
            .markers
            .iter()
            .any(|m| m.file == "./ForestPackages/mine/MyPkg.verse" && m.content.contains("MyPkg<public>")));
    }

    #[test]
    fn invalid_authored_folder_name_warns_instead_of_generating() {
        let lf = lockfile(vec![]);
        let roots = root_deps(&[]);
        let authored = vec![("mine".to_string(), "bad-name".to_string())];
        let plan = plan_install_uefn(&lf, &roots, "/x/Y", &authored).unwrap();
        assert!(plan.markers.iter().all(|m| !m.file.contains("bad-name")));
        assert_eq!(plan.warnings.len(), 1);
        assert!(plan.warnings[0].contains("bad-name"));
    }

    #[test]
    fn reserved_scope_maps_with_prefix_in_paths() {
        let lf = lockfile(vec![("module/pkg", vec![entry("1.0.0", "m1", &[])])]);
        let roots = root_deps(&[("module/Pkg", "Pkg", "^1")]);
        let plan = plan_install_uefn(&lf, &roots, "/x/Y", &[]).unwrap();
        assert_eq!(plan.packages[0].path, "./ForestPackages/_module/Pkg");
        assert!(plan.markers.iter().any(|m| m.file == "./ForestPackages/_module.verse"
            && m.content.contains("_module<public>")));
    }
}
