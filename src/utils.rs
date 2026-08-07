// use tokio::sync::mpsc::error; // Not needed for logging

use std::collections::HashMap;

use serde_json::Value;

use crate::lockfile_solver::{DepSpec};

pub struct PackageName {
    pub name: String,
    pub scope: String,
    pub full_name: String,
}

pub fn digest_package_name(name : &str) -> PackageName {
    let parts: Vec<&str> = name.split('/').collect();
    if parts.len() == 1 {
        panic!("Invalid package name format");
    }
    PackageName { name: parts[1].to_string(), scope: parts[0].to_string(), full_name: name.to_string() }
}

/// Lowercase hex SHA-256 — the format lockfile integrity hashes use.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Case-insensitive HashMap lookup for package-name keys: Exact match wins; otherwise the first case-insensitive hit.
pub fn get_ci<'a, V>(map: &'a HashMap<String, V>, key: &str) -> Option<&'a V> {
    map.get(key).or_else(|| {
        map.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    })
}

/// How a user-typed package reference resolved against the manifest's
/// declared dependencies.
#[derive(Debug, PartialEq, Eq)]
pub enum DepRef {
    /// The manifest key it refers to.
    Match(String),
    NotFound,
    /// A bare name that several declared packages answer to (their keys,
    /// sorted) - the caller should warn and ask for the full <scope>/<name>.
    Ambiguous(Vec<String>),
}

/// Resolve a package reference against the declared dependencies. Accepts the
/// full `scope/name` key, the install alias, or the bare package name - all
/// case-insensitive. Alias matches win over name-part matches (the alias is
/// the name code requires by); the name part is only consulted when no
/// declared alias claims the reference.
pub fn resolve_dep_ref(roots: &HashMap<String, DepSpec>, reference: &str) -> DepRef {
    if reference.contains('/') {
        return match roots
            .keys()
            .find(|k| k.eq_ignore_ascii_case(reference))
        {
            Some(key) => DepRef::Match(key.clone()),
            None => DepRef::NotFound,
        };
    }

    // Deps without an explicit alias default it to their name part, so this
    // covers the plain-name case too - and two scopes shipping the same
    // name both land here, which is exactly the ambiguity to surface.
    let alias_matches: Vec<&String> = roots
        .iter()
        .filter(|(_, spec)| spec.alias.eq_ignore_ascii_case(reference))
        .map(|(k, _)| k)
        .collect();
    let matches = if alias_matches.is_empty() {
        roots
            .keys()
            .filter(|k| k.rsplit('/').next().map_or(false, |n| n.eq_ignore_ascii_case(reference)))
            .collect()
    } else {
        alias_matches
    };

    match matches.len() {
        0 => DepRef::NotFound,
        1 => DepRef::Match(matches[0].clone()),
        _ => {
            let mut keys: Vec<String> = matches.into_iter().cloned().collect();
            keys.sort();
            DepRef::Ambiguous(keys)
        }
    }
}

pub fn normalize_forest_deps(forest_json : &Value) -> HashMap<String, DepSpec> {
     let roots : HashMap<String, DepSpec> = forest_json
        .get("dependencies")
        .and_then(|deps| deps.as_object())
        .map_or_else(HashMap::new, |deps| {
            deps.iter()
                .filter_map(|(k, v)| {
                    if let Some(s) = v.as_str() {
                        Some((k.clone(), DepSpec{ alias: digest_package_name(k).name, version: s.to_string() }))
                    } else if let Some(obj) = v.as_object() {
                        let version = obj.get("version")
                            .and_then(|x| x.as_str())
                            .unwrap_or("")
                            .to_string();
                        let alias = obj.get("alias")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| digest_package_name(k).name);
                        Some((k.clone(), DepSpec{ alias, version }))
                    } else {
                        None
                    }
                })
                .collect()
        }); 

    roots

}

/// Read the manifest's `overrides` map: `scope/name` -> semver range. An
/// override forces every transitive edge to that package onto the given
/// range, replacing whatever range the parent declared. Keys without a
/// scope and non-string values are skipped; the solver reports overrides
/// that never matched anything.
pub fn normalize_forest_overrides(forest_json: &Value) -> HashMap<String, String> {
    string_map_field(forest_json, "overrides")
}

/// Read the manifest's `excludes` map: `scope/name` -> semver range of
/// versions that must never be installed. Unlike overrides, exclusions
/// apply uniformly to direct and transitive deps: the solver just removes
/// the banned versions from the candidate set, so every declared range is
/// still honored — or resolution fails loudly when a range has no
/// non-excluded version left.
pub fn normalize_forest_excludes(forest_json: &Value) -> HashMap<String, String> {
    string_map_field(forest_json, "excludes")
}

fn string_map_field(forest_json: &Value, field: &str) -> HashMap<String, String> {
    forest_json
        .get(field)
        .and_then(|o| o.as_object())
        .map_or_else(HashMap::new, |entries| {
            entries
                .iter()
                .filter(|(k, _)| k.contains('/'))
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
}

/// Package-name equality: case-insensitive.
pub fn same_package(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(pairs: &[(&str, &str)]) -> HashMap<String, DepSpec> {
        pairs
            .iter()
            .map(|(key, alias)| {
                let alias = if alias.is_empty() {
                    digest_package_name(key).name
                } else {
                    alias.to_string()
                };
                (key.to_string(), DepSpec { alias, version: "^1.0.0".to_string() })
            })
            .collect()
    }

    #[test]
    fn full_key_matches_case_insensitively() {
        let r = roots(&[("Scope/Pkg", "")]);
        assert_eq!(resolve_dep_ref(&r, "scope/pkg"), DepRef::Match("Scope/Pkg".into()));
        assert_eq!(resolve_dep_ref(&r, "other/pkg"), DepRef::NotFound);
    }

    #[test]
    fn bare_name_matches_the_default_alias() {
        let r = roots(&[("stratiz/Signal", ""), ("acme/Promise", "")]);
        assert_eq!(resolve_dep_ref(&r, "signal"), DepRef::Match("stratiz/Signal".into()));
        assert_eq!(resolve_dep_ref(&r, "Missing"), DepRef::NotFound);
    }

    #[test]
    fn explicit_alias_matches_and_owns_the_local_name() {
        // a/foo is locally known as "bar"; typing "foo" no longer means it.
        let r = roots(&[("a/foo", "bar"), ("b/baz", "")]);
        assert_eq!(resolve_dep_ref(&r, "bar"), DepRef::Match("a/foo".into()));
        assert_eq!(resolve_dep_ref(&r, "foo"), DepRef::Match("a/foo".into()));
    }

    #[test]
    fn alias_match_beats_another_packages_bare_name() {
        // b/foo claimed "bar" as its alias; c/bar's name part also says
        // "bar" but its alias points elsewhere. The alias holder wins.
        let r = roots(&[("b/foo", "bar"), ("c/bar", "Something")]);
        assert_eq!(resolve_dep_ref(&r, "bar"), DepRef::Match("b/foo".into()));
    }

    #[test]
    fn same_name_across_scopes_is_ambiguous() {
        let r = roots(&[("a/Signal", "SignalA"), ("b/Signal", "SignalB")]);
        match resolve_dep_ref(&r, "signal") {
            DepRef::Ambiguous(keys) => assert_eq!(keys, vec!["a/Signal".to_string(), "b/Signal".to_string()]),
            other => panic!("expected Ambiguous, got {:?}", other),
        }
    }

    #[test]
    fn overrides_parse_and_skip_malformed_entries() {
        let manifest = serde_json::json!({
            "overrides": {
                "scope/pkg": "^2.0.0",
                "noscope": "^1.0.0",
                "scope/other": { "version": "^1.0.0" }
            }
        });
        let overrides = normalize_forest_overrides(&manifest);
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides["scope/pkg"], "^2.0.0");
    }

    #[test]
    fn missing_overrides_field_is_empty() {
        assert!(normalize_forest_overrides(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn excludes_parse_like_overrides() {
        let manifest = serde_json::json!({
            "excludes": { "scope/pkg": ">=1.6.0, <1.7.0", "noscope": "1.0.0" }
        });
        let excludes = normalize_forest_excludes(&manifest);
        assert_eq!(excludes.len(), 1);
        assert_eq!(excludes["scope/pkg"], ">=1.6.0, <1.7.0");
    }

    #[test]
    fn same_package_ignores_case() {
        assert!(same_package("Scope/Pkg", "scope/pkg"));
        assert!(!same_package("a/pkg", "b/pkg"));
    }

    #[test]
    fn same_default_alias_across_scopes_is_ambiguous() {
        // UEFN shape: no aliases exist, so both default to the name part.
        let r = roots(&[("a/Signal", ""), ("b/Signal", "")]);
        assert!(matches!(resolve_dep_ref(&r, "Signal"), DepRef::Ambiguous(_)));
    }
}