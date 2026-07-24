// Cargo.toml
// ---------------------
// [package]
// name = "forest_lockfile_resolver"
// version = "0.1.0"
// edition = "2021"
//
// [dependencies]
// tokio = { version = "1", features = ["full"] }
// reqwest = { version = "0.11", features = ["json"] }
// semver = "1.0"
// serde = { version = "1.0", features = ["derive"] }
// anyhow = "1.0"

// src/main.rs
// ---------------------
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use anyhow::{Result, Context};
use reqwest::{Method};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use crate::http::{api_request, packages_api_request};
use crate::license_helper::LicenseInfo;
use crate::message::Message;
use crate::utils::{digest_package_name, PackageName };

/// Concurrent version-list prefetches in flight at once.
const PREFETCH_CONCURRENCY: usize = 8;

type VersionListHandle = tokio::task::JoinHandle<Result<(serde_json::Value, reqwest::StatusCode)>>;

/// Fire the version-list request for a package the moment its name is known,
/// instead of when the BFS gets around to it. The BFS awaits the memoized
/// handle at exactly the point it used to issue the request, so processing
/// order — and therefore bucket merging and the resulting lockfile — is
/// unchanged; only the network wait overlaps.
fn spawn_version_list_fetch(
    scope: String,
    pkg_name: String,
    full_name: String,
    platform: String,
    limiter: &Arc<tokio::sync::Semaphore>,
) -> VersionListHandle {
    let limiter = Arc::clone(limiter);
    tokio::spawn(async move {
        let _permit = limiter.acquire_owned().await.expect("semaphore closed");
        let path = format!("v1/package/{}/{}/{}", scope, platform, pkg_name);
        api_request(&path, Method::GET, None, None).await
            .with_context(|| format!("Failed to fetch package info for {}", full_name))
    })
}

/// Tracks per-version resolution state
#[derive(Debug)]
struct VersionState {
    resolved: bool,
    dependencies: HashMap<String, DepSpec>,
    integrity: String,
    public: bool,
    archive_root: String
}

/// Holds buckets (grouped ranges) and per-version state
#[derive(Debug)]
struct PackageState {
    canonical: String,
    buckets: HashMap<String, Vec<String>>,
    versions: HashMap<String, VersionState>,
}

/// Keyed by the LOWERCASED full name
type ResolvedVersions = HashMap<String, PackageState>;

/// Lockfile entry for a package version.
///
/// Deliberately stores no download URL: tarballs are content-addressed
/// (`{integrity}.tgz` on the CDN), so the URL is derived from `integrity` at
/// install time. A URL field would be an attacker-editable pointer in PRs
/// (lockfile injection); the hash both names and verifies the content.
#[derive(Debug, Serialize, Deserialize)]
pub struct LockfileEntry {
    pub version: String,
    pub integrity: String,
    pub public: bool,
    pub root : String,
    pub location: String,
    pub dependencies: HashMap<String, DepSpec>,
}

type LockfilePackages = HashMap<String, Vec<LockfileEntry>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepSpec {
    pub alias: String,
    pub version: String,
}

/// Resolves the dependency graph. Also returns license-safety issues for any
/// resolved version the registry rated caution/unsafe — each version is fetched
/// exactly once, so issues are naturally deduplicated. The final map records
/// root manifest keys whose registry identity is a different package name
/// entirely (claimed/renamed scopes, e.g. a wally scope claimed under a new
/// username) — casing-only differences are not renames.
pub async fn get_lockfile_packages(root_deps: HashMap<String, DepSpec>, platform : String, msg: &mut Message) -> Result<(LockfilePackages, Vec<LicenseInfo>, HashMap<String, String>)> {
    let mut resolved: ResolvedVersions = HashMap::new();
    let mut license_warnings: Vec<LicenseInfo> = Vec::new();
    // Live spinner counter: versions whose metadata has been fetched. The BFS
    // discovers the tree as it goes, so there is no fixed total to show.
    let mut resolved_count: usize = 0;

    // Make queue with digest_package_name using normalized specs
    let mut queue: VecDeque<(PackageName, String, u8)> = root_deps.clone().into_iter()
        .map(|(name, spec)| (digest_package_name(&name), spec.version, 1))
        .collect();

    // Version lists are fetched eagerly as names are discovered (roots now,
    // deps as their parents resolve) and awaited at the same point the BFS
    // always fetched them — see spawn_version_list_fetch.
    let limiter = Arc::new(tokio::sync::Semaphore::new(PREFETCH_CONCURRENCY));
    let mut list_prefetch: HashMap<String, VersionListHandle> = HashMap::new();
    for (name, _, _) in &queue {
        let key = name.full_name.to_lowercase();
        if !list_prefetch.contains_key(&key) {
            let handle = spawn_version_list_fetch(
                name.scope.clone(),
                name.name.clone(),
                name.full_name.clone(),
                platform.clone(),
                &limiter,
            );
            list_prefetch.insert(key, handle);
        }
    }

    // 1) Resolve dependency graph into buckets & versions

    while let Some((name, version_range, depth)) = queue.pop_front() {
        // Case variants of one package all land on the same lowercased key
        let key = name.full_name.to_lowercase();

        msg.update(&format!(
            "Resolving {} ({} resolved, {} queued)",
            name.full_name, resolved_count, queue.len()
        ));

        // fetch available versions (first encounter under any casing)
        if !resolved.contains_key(&key) {
            let (version_data, versions_status) = match list_prefetch.remove(&key) {
                Some(handle) => handle.await
                    .map_err(|e| anyhow::anyhow!("Version-list task panicked: {e}"))??,
                None => {
                    let path = format!("v1/package/{}/{}/{}", name.scope, platform, name.name);
                    api_request(&path, Method::GET, None, None).await
                        .with_context(|| format!("Failed to fetch package info for {}", name.full_name))?
                }
            };

            if !versions_status.is_success() {
                Err(anyhow::anyhow!(
                    "Failed to fetch package info for {}: HTTP {}",
                    name.full_name, versions_status
                ))?;
            }

            // The response carries the canonical (stored) casing — what the
            // lockfile keys are written as. Fall back to the casing we
            // queried with if a response ever lacks the fields.
            let canonical = match (
                version_data.get("scope").and_then(|v| v.as_str()),
                version_data.get("name").and_then(|v| v.as_str()),
            ) {
                (Some(scope), Some(pkg_name)) => format!("{}/{}", scope, pkg_name),
                _ => name.full_name.clone(),
            };

            let versions = version_data.get("versions")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow::anyhow!("Invalid versions data for {}", name.full_name))?;

            let pkg_state = resolved.entry(key.clone())
                .or_insert_with(|| PackageState {
                    canonical,
                    buckets: HashMap::new(),
                    versions: HashMap::new(),
                });

            for ver_info in versions {
                let ver = ver_info.get("version")
                    .ok_or_else(|| anyhow::anyhow!("Missing version field for {}", name.full_name))?
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Invalid version field for {}", name.full_name))?.to_string();


                //println!("Found version {} for package {}", ver, name.full_name);
                pkg_state.versions.insert(
                    ver,
                    VersionState { resolved: false, dependencies: HashMap::new(), integrity: String::new(), public: false, archive_root: String::new() }
                );
            }
        }

        let pkg_state = resolved.get_mut(&key)
            .expect("package state exists after first-encounter fetch");

        // filter by range
        let req = VersionReq::parse(&version_range)
            .with_context(|| format!("Invalid range {} for {}", version_range, name.full_name))?;
        let all_versions: Vec<String> = pkg_state.versions.keys().cloned().collect();
        let mut matches: Vec<String> = all_versions.iter()
            .filter(|v| Version::parse(v).map(|ver| req.matches(&ver)).unwrap_or(false))
            .cloned()
            .collect();
        if matches.is_empty() {
            anyhow::bail!("No versions found for {} matching {}", name.full_name, version_range);
        }

        // determine bucket
        matches.sort_by(|a,b| Version::parse(b).unwrap().cmp(&Version::parse(a).unwrap()));
        let mut agreed = matches[0].clone();
        for (bucket_ver, ranges) in pkg_state.buckets.clone() {
            let mut in_bucket: Vec<String> = all_versions.iter()
                .filter(|v| req.matches(&Version::parse(v).unwrap()))
                .cloned().collect();
            for br in &ranges {
                let br_req = VersionReq::parse(br).unwrap();
                in_bucket.retain(|v| br_req.matches(&Version::parse(v).unwrap()));
            }
            in_bucket.sort_by(|a,b| Version::parse(b).unwrap().cmp(&Version::parse(a).unwrap()));
            if let Some(nv) = in_bucket.into_iter().next() {
                if nv != bucket_ver {
                    let old = pkg_state.buckets.remove(&bucket_ver).unwrap();
                    pkg_state.buckets.insert(nv.clone(), old.into_iter().filter(|r| r != &version_range).collect());
                }
                agreed = nv;
                break;
            }
        }
        pkg_state.buckets.entry(agreed.clone()).or_default().push(version_range.clone());

        // fetch dependencies if not resolved
        let vs = pkg_state.versions.get_mut(&agreed).unwrap();
        if vs.resolved { continue; }
        let path = format!("v1/package/{}/{}/{}/{}", name.scope, platform, name.name, agreed);

        let (package_info, status) = packages_api_request(&path, Method::GET, None, None).await
            .with_context(|| format!("Failed to fetch package info for {}@{}", name.full_name, agreed))?;

        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "Failed to fetch package info for {}@{}: HTTP {}",
                name.full_name, agreed, status
            ));
        }

        vs.resolved = true;
        resolved_count += 1;

        let license_info = crate::license_helper::extract_license_info(
            &package_info,
            &format!("{}@{}", name.full_name, agreed),
        );
        if license_info.is_flagged() {
            license_warnings.push(license_info);
        }

        let deps = package_info.get("dependencies")
            .and_then(|v| v.as_object())
            .ok_or_else(|| anyhow::anyhow!("Invalid dependencies data for {}@{}", name.full_name, agreed))?;

        // Support both legacy string dependencies and new object form { alias, version }
        let deps_hm: HashMap<String, DepSpec> = deps.clone().into_iter()
            .map(|(k, v)| -> anyhow::Result<(String, DepSpec)> {
                if let Some(s) = v.as_str() {
                    // Legacy: value is a version string; alias derives from the key
                    let spec = DepSpec { alias: digest_package_name(&k).name, version: s.to_string() };
                    //TODO : Drop this because backend will never return just the string.
                    Ok((k, spec))
                } else if let Some(obj) = v.as_object() {
                    let version = obj.get("version")
                        .and_then(|x| x.as_str())
                        .ok_or_else(|| anyhow::anyhow!(
                            "Dependency version for {}@{} must be a string",
                            name.full_name, agreed
                        ))?
                        .to_string();
                    // An alias is present only when the publisher declared one.
                    // Aliases containing '/' are full-key fabrications from
                    // older publishes (`alias: "<scope>/<name>"`) — never a
                    // real folder name; treat them as unset.
                    let alias = obj.get("alias")
                        .and_then(|x| x.as_str())
                        .filter(|s| !s.contains('/'))
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| digest_package_name(&k).name);
                    Ok((k, DepSpec { alias, version }))
                } else {
                    // Unexpected shape
                    Err(anyhow::anyhow!(
                        "Invalid dependency spec for {}@{}: expected string or object",
                        name.full_name, agreed
                    ))
                }
            })
            .collect::<anyhow::Result<_>>()?;

        vs.dependencies = deps_hm.clone();
        vs.integrity = package_info.get("integrity")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();

        if vs.integrity.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "Registry returned no integrity hash for {}@{}; cannot lock this version",
                name.full_name, agreed
            ));
        }

        // Private tarballs need a fresh signed URL at install time; default to
        // private when the field is missing so we fall back to asking the API.
        vs.public = package_info.get("public")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        vs.archive_root = package_info.get("archiveRoot")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or("".to_string());


        for (dep_name, dep_spec) in deps_hm {
            let dep_pkg = digest_package_name(&dep_name);
            let dep_key = dep_pkg.full_name.to_lowercase();
            // First sighting of this package name → start its version-list
            // request now so it's (likely) done by the time BFS reaches it.
            if !resolved.contains_key(&dep_key) && !list_prefetch.contains_key(&dep_key) {
                let handle = spawn_version_list_fetch(
                    dep_pkg.scope.clone(),
                    dep_pkg.name.clone(),
                    dep_pkg.full_name.clone(),
                    platform.clone(),
                    &limiter,
                );
                list_prefetch.insert(dep_key, handle);
            }
            queue.push_front((dep_pkg, dep_spec.version, depth + 1));
        }
    }

    // 2) Build lockfile entries — keyed by the canonical casing, while dep
    // keys (verbatim from publishers' recorded manifests) are looked up
    // through their lowercased form. Every dep was queued and fetched, so
    // every lookup hits.
    let mut lockfile: LockfilePackages = HashMap::new();
    for state in resolved.values() {
        let mut entries = Vec::new();
        for bucket_ver in state.buckets.keys() {
            let vs = &state.versions[bucket_ver];
            let mut deps = HashMap::new();
            for (dn, dr) in &vs.dependencies {
                let dep_state = &resolved[&dn.to_lowercase()];
                let v = dep_state.buckets.keys()
                    .find(|v| VersionReq::parse(&dr.version).unwrap().matches(&Version::parse(v).unwrap()))
                    .cloned().unwrap();
                deps.insert(dep_state.canonical.clone(), DepSpec{
                    version : v,
                    alias : dr.alias.clone()
                });
            }
            entries.push(LockfileEntry {
                version: bucket_ver.clone(),
                integrity: vs.integrity.clone(),
                public: vs.public,
                root: vs.archive_root.clone(),
                location: String::new(),
                dependencies: deps,
            });
        }
        lockfile.insert(state.canonical.clone(), entries);
    }

    // 3) Annotate locations with tree positions
    

    fn build_tree(
        name: &str,
        alias : &str,
        version: &str,
        loc: &str,
        lockfile: &mut LockfilePackages,
    ) {
        if let Some(entries) = lockfile.get_mut(name) {
            if let Some(entry) = entries.iter_mut().find(|e| e.version == version) {
                if !entry.location.is_empty() && entry.location.len() < loc.len() + 1 {
                    return;
                }

                entry.location = loc.to_string();

                // Collect dependencies to avoid holding a mutable borrow during recursion
                let deps: Vec<(String, DepSpec)> = entry.dependencies.iter()
                    .map(|(dn, dv)| (dn.clone(), dv.clone()))
                    .collect();

                // Also collect dependency keys for each dependency

                for (dn, dv) in deps.clone().into_iter() {
                    //let dep_names: Vec<String> = deps.iter().map(|(dn, _)| dn.clone()).collect();
                    let next_loc = format!("{}/{}", loc, alias);

                    
                    build_tree(&dn, &dv.alias, &dv.version, &next_loc, lockfile);
                }
            }
        }
    }

    // Root keys resolving to a different package name (scope claimed and
    // renamed): the lockfile is keyed by the canonical name, so the caller
    // must re-key its root deps or install planning can't map them back.
    let mut root_renames: HashMap<String, String> = HashMap::new();

    for (name, dep_spec) in &root_deps {
        // Manifest keys may carry non-canonical casing (hand-edited or
        // pre-canonicalization installs) — the lowercased key still hits.
        if let Some(state) = resolved.get(&name.to_lowercase()) {
            if !state.canonical.eq_ignore_ascii_case(name) {
                root_renames.insert(name.clone(), state.canonical.clone());
            }
            // Only get keys that satisfy the version range
            let req = VersionReq::parse(&dep_spec.version)
                .with_context(|| format!("Invalid range {} for {}", dep_spec.version, name))?;

            let mut versions: Vec<String> = state.versions.keys()
                .filter(|v| req.matches(&Version::parse(v).unwrap()))
                .cloned()
                .collect();

            if versions.is_empty() {
                return Err(anyhow::anyhow!("No versions found for {} matching {}", name, dep_spec.version));
            }

            versions.sort_by(|a,b| Version::parse(b).unwrap().cmp(&Version::parse(a).unwrap()));

            if let Some(first) = versions.first() {
                // Collect dependencies to avoid holding a mutable borrow during recursion
                /*let deps: Vec<(String, String)> = normalized_root_deps.clone().iter()
                    .map(|(dn, dv)| (dn.clone(), dv.version.clone()))
                    .collect();

                */
                //let dep_names: Vec<String> = deps.iter().map(|(dn, _)| dn.clone()).collect();
                //let _name_for_next =  get_dupe_name(name.clone(), dep_names);
                // Lockfile keys are canonical — never the manifest's casing
                build_tree(&state.canonical, &dep_spec.alias, first, "~", &mut lockfile);
            }
        }
    }

    Ok((lockfile, license_warnings, root_renames))
}


pub async fn _test() -> Result<()> {
   
    Ok(())
}
