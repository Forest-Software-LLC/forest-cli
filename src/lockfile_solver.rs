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
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use anyhow::{Result, Context};
use reqwest::{Method};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use crate::http::{api_request, packages_api_request};
use crate::license_helper::LicenseInfo;
use crate::message::{Message, MessageType};
use crate::meta_cache::{trim_install_meta, MetaCache};
use crate::utils::{digest_package_name, PackageName };

/// Concurrent version-list prefetches in flight at once.
const PREFETCH_CONCURRENCY: usize = 8;

type VersionListHandle = tokio::task::JoinHandle<Result<(serde_json::Value, reqwest::StatusCode)>>;

/// Version-list URL. Scope and name are lowercased so every casing shares
/// one edge cache entry (the backend resolves them case-insensitively and
/// purges only the lowercase URL); the response carries canonical casing
/// back. `?detail=install` asks for the fat response with per-version
/// install metadata inlined.
fn version_list_path(scope: &str, pkg_name: &str, platform: &str) -> String {
    format!(
        "v1/package/{}/{}/{}?detail=install",
        scope.to_lowercase(),
        platform,
        pkg_name.to_lowercase()
    )
}

/// Fire the version-list request for a package the moment its name is known,
/// instead of when the BFS gets around to it. The BFS awaits the memoized
/// handle at exactly the point it used to issue the request, so processing
/// order (and therefore bucket merging and the resulting lockfile) is
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
        let path = version_list_path(&scope, &pkg_name, &platform);
        api_request(&path, Method::GET, None, None).await
            .with_context(|| format!("Failed to fetch package info for {}", full_name))
    })
}

/// Tracks per-version resolution state
#[derive(Debug)]
struct VersionState {
    resolved: bool,
    /// Install metadata inlined by the fat version-list response, taken at
    /// the resolve point so no per-version request is needed. Always
    /// network-fresh, so it skips the disk-cache confirmation pass.
    prefetched: Option<serde_json::Value>,
    dependencies: HashMap<String, DepSpec>,
    integrity: String,
    public: bool,
    archive_root: String,
    /// The version's own nested dep container name (`packagesDir` in the
    /// registry metadata); "Packages" when unset, covering pre-field and
    /// wally-mirrored versions.
    packages_dir: String,
}

/// Holds buckets (grouped ranges) and per-version state
#[derive(Debug)]
struct PackageState {
    canonical: String,
    buckets: HashMap<String, Vec<String>>,
    versions: HashMap<String, VersionState>,
}

/// A disk-cached metadata entry the resolve used, queued for the post-BFS
/// confirmation pass.
struct CacheSourced {
    scope: String,
    pkg_name: String,
    full_name: String,
    version: String,
    cached: serde_json::Value,
}

/// Disk-cached metadata failed its registry confirmation (republished
/// version or local tampering). The public wrapper catches this and
/// re-resolves with the cache disabled.
#[derive(Debug)]
struct MetaCacheMismatch {
    packages: Vec<String>,
}

impl std::fmt::Display for MetaCacheMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Locally cached metadata for {} no longer matches the registry.",
            self.packages.join(", ")
        )
    }
}

impl std::error::Error for MetaCacheMismatch {}

/// Merge a __fresh version-list response into an existing package state:
/// unseen versions are added, existing entries stay untouched.
fn merge_fresh_version_list(state: &mut PackageState, version_data: &serde_json::Value) {
    let Some(versions) = version_data.get("versions").and_then(|v| v.as_array()) else {
        return;
    };
    let pkg_public = version_data.get("public").and_then(|v| v.as_bool());
    for ver_info in versions {
        let Some(ver) = ver_info.get("version").and_then(|v| v.as_str()) else {
            continue;
        };
        if state.versions.contains_key(ver) {
            continue;
        }
        let prefetched = ver_info.get("install")
            .filter(|block| block.is_object())
            .map(|block| trim_install_meta(block, pkg_public));
        state.versions.insert(
            ver.to_string(),
            VersionState {
                resolved: false,
                prefetched,
                dependencies: HashMap::new(),
                integrity: String::new(),
                public: false,
                archive_root: String::new(),
                packages_dir: default_packages_dir(),
            },
        );
    }
}

/// Keyed by the LOWERCASED full name
type ResolvedVersions = HashMap<String, PackageState>;

/// Lockfile entry for a package version.
///
/// Deliberately stores no download URL: tarballs are content-addressed
/// (`{integrity}.tgz` on the CDN), so the URL is derived from `integrity` at
/// install time. A URL field would be an attacker-editable pointer in PRs
/// (lockfile injection); the hash both names and verifies the content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockfileEntry {
    pub version: String,
    pub integrity: String,
    pub public: bool,
    pub root : String,
    pub location: String,
    /// The version's own nested dep container name (registry `packagesDir`).
    /// Defaulted so v2 lockfiles written before the field stay readable, and
    /// skipped on write when default so unaffected lockfiles don't churn.
    #[serde(
        rename = "packagesDir",
        default = "default_packages_dir",
        skip_serializing_if = "is_default_packages_dir"
    )]
    pub packages_dir: String,
    pub dependencies: HashMap<String, DepSpec>,
}

/// Serde default for `LockfileEntry::packages_dir` / `VersionState`: the
/// historic hardcoded container name.
pub fn default_packages_dir() -> String {
    "Packages".to_string()
}

fn is_default_packages_dir(value: &String) -> bool {
    value == "Packages"
}

type LockfilePackages = HashMap<String, Vec<LockfileEntry>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepSpec {
    pub alias: String,
    pub version: String,
}

/// What the manifest's overrides and excludes actually did during
/// resolution. All lists hold manifest keys, sorted for deterministic
/// output.
#[derive(Debug, Default)]
pub struct SolveReport {
    /// Total dependency edges the overrides rewrote during resolution.
    pub override_edges: usize,
    /// Overrides that matched no edge in the resolved graph.
    pub override_unused: Vec<String>,
    /// Overrides every rewritten edge would satisfy naturally; removing
    /// them would not change the resolution.
    pub override_unnecessary: Vec<String>,
    /// Excludes whose package never appeared in the graph.
    pub exclude_unused: Vec<String>,
    /// Excludes that removed nothing any range would have picked; natural
    /// resolution already lands outside the banned set.
    pub exclude_inert: Vec<String>,
    /// Resolved packages (direct or transitive) the registry marked archived.
    /// Warn-only: archived packages always keep installing.
    pub archived: Vec<ArchivedPackage>,
}

/// A resolved package marked archived by the registry, with the owner's
/// optional reason/successor note for the warning line.
#[derive(Debug)]
pub struct ArchivedPackage {
    pub name: String,
    pub reason: Option<String>,
}

/// Resolves the dependency graph. Also returns license-safety issues for any
/// resolved version the registry rated caution/unsafe; each version is fetched
/// exactly once, so issues are naturally deduplicated. The final map records
/// root manifest keys whose registry identity is a different package name
/// entirely (claimed/renamed scopes, e.g. a wally scope claimed under a new
/// username); casing-only differences are not renames.
///
/// `use_meta_cache: false` (install --force) resolves everything from the
/// network. When the cache IS used and its confirmation pass finds a stale
/// or tampered entry, resolution transparently re-runs without the cache.
pub async fn get_lockfile_packages(root_deps: HashMap<String, DepSpec>, overrides: &HashMap<String, String>, excludes: &HashMap<String, String>, platform : String, msg: &mut Message, use_meta_cache: bool) -> Result<(LockfilePackages, Vec<LicenseInfo>, HashMap<String, String>, SolveReport)> {
    match resolve_lockfile_packages(root_deps.clone(), overrides, excludes, platform.clone(), msg, use_meta_cache).await {
        Err(err) if err.downcast_ref::<MetaCacheMismatch>().is_some() => {
            msg.emit(
                MessageType::Warn,
                &format!("{} Re-resolving from the registry.", err),
            );
            resolve_lockfile_packages(root_deps, overrides, excludes, platform, msg, false).await
        }
        result => result,
    }
}

async fn resolve_lockfile_packages(root_deps: HashMap<String, DepSpec>, overrides: &HashMap<String, String>, excludes: &HashMap<String, String>, platform : String, msg: &mut Message, use_meta_cache: bool) -> Result<(LockfilePackages, Vec<LicenseInfo>, HashMap<String, String>, SolveReport)> {
    let mut resolved: ResolvedVersions = HashMap::new();
    let mut license_warnings: Vec<LicenseInfo> = Vec::new();
    // Archived packages in the resolved tree; warn-only, collected once per
    // package at its first-encounter fetch.
    let mut archived_packages: Vec<ArchivedPackage> = Vec::new();

    // Disk metadata cache. Entries only ever save round trips (threat model
    // in meta_cache.rs): everything consumed this run is re-fetched in the
    // confirmation pass after the BFS, and a mismatch aborts with
    // MetaCacheMismatch so the wrapper retries without the cache.
    let meta_cache = if use_meta_cache { MetaCache::open_default() } else { None };
    let mut confirm_queue: Vec<CacheSourced> = Vec::new();
    // One __fresh version-list retry per package, for ranges a stale edge
    // cache can't satisfy (publish then install).
    let mut fresh_retried: HashSet<String> = HashSet::new();

    // Overrides force every transitive edge to a package onto one range,
    // replacing the parent's declared range. Root deps are never rewritten:
    // their ranges are the user's own manifest entries. Keyed lowercased so
    // any publisher casing of the dep name matches.
    let overrides_lc: HashMap<String, String> = overrides.iter()
        .map(|(k, v)| (k.to_lowercase(), v.clone()))
        .collect();
    // Declared ranges each override replaced, for the unused/unnecessary report.
    let mut override_hits: HashMap<String, Vec<String>> = HashMap::new();

    // Excludes ban versions outright: they are filtered from the candidate
    // set wherever versions are matched, for roots and transitive deps
    // alike, so declared ranges are still honored; or resolution fails
    // naming the exclusion. An unparseable exclude range is a hard error,
    // same as an unparseable dependency range.
    let mut excludes_lc: HashMap<String, VersionReq> = HashMap::new();
    for (pkg, range) in excludes {
        let req = VersionReq::parse(range)
            .with_context(|| format!("Invalid exclude range {} for {} in forest.json", range, pkg))?;
        excludes_lc.insert(pkg.to_lowercase(), req);
    }
    // Every range requested for an excluded package, for the inert report.
    let mut exclude_ranges_seen: HashMap<String, Vec<String>> = HashMap::new();
    // Live spinner counter: versions whose metadata has been fetched. The BFS
    // discovers the tree as it goes, so there is no fixed total to show.
    let mut resolved_count: usize = 0;

    // Make queue with digest_package_name using normalized specs
    let mut queue: VecDeque<(PackageName, String, u8)> = root_deps.clone().into_iter()
        .map(|(name, spec)| (digest_package_name(&name), spec.version, 1))
        .collect();

    // Version lists are fetched eagerly as names are discovered (roots now,
    // deps as their parents resolve) and awaited at the same point the BFS
    // always fetched them; see spawn_version_list_fetch.
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
                    let path = version_list_path(&name.scope, &name.name, &platform);
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

            // The response carries the canonical (stored) casing, which is
            // what the lockfile keys are written as. Fall back to the casing we
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

            // Archived packages still install; the registry marks them so the
            // resolve can warn (an old backend simply sends no flag).
            if version_data.get("archived").and_then(|v| v.as_bool()) == Some(true) {
                archived_packages.push(ArchivedPackage {
                    name: canonical.clone(),
                    reason: version_data.get("archivedReason")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                });
            }

            // Fat responses carry `public` at package level; inject it into
            // each version's trimmed block so every metadata source shares
            // one shape. Slim responses (old backend, stale cache) have no
            // install blocks and those versions fall back per-version.
            let pkg_public = version_data.get("public").and_then(|v| v.as_bool());

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

                let prefetched = ver_info.get("install")
                    .filter(|block| block.is_object())
                    .map(|block| crate::meta_cache::trim_install_meta(block, pkg_public));

                //println!("Found version {} for package {}", ver, name.full_name);
                pkg_state.versions.insert(
                    ver,
                    VersionState { resolved: false, prefetched, dependencies: HashMap::new(), integrity: String::new(), public: false, archive_root: String::new(), packages_dir: default_packages_dir() }
                );
            }
        }

        let pkg_state = resolved.get_mut(&key)
            .expect("package state exists after first-encounter fetch");

        // filter by range, then drop excluded versions from the candidates
        let req = VersionReq::parse(&version_range)
            .with_context(|| format!("Invalid range {} for {}", version_range, name.full_name))?;
        let exclude_req = excludes_lc.get(&key);
        if exclude_req.is_some() {
            exclude_ranges_seen.entry(key.clone()).or_default().push(version_range.clone());
        }
        let all_versions: Vec<String> = pkg_state.versions.keys().cloned().collect();
        let raw_matches: Vec<String> = all_versions.iter()
            .filter(|v| Version::parse(v).map(|ver| req.matches(&ver)).unwrap_or(false))
            .cloned()
            .collect();
        let mut matches: Vec<String> = raw_matches.iter()
            .filter(|v| !version_excluded(exclude_req, v))
            .cloned()
            .collect();
        if matches.is_empty() {
            // The edge cache may be serving a pre-publish version list.
            // Refetch once with a __fresh cache-buster (the shim skips its
            // cache for it) and retry this queue item, so installing a
            // just-published version works even before the purge lands.
            // insert() makes this once per package.
            if raw_matches.is_empty() && fresh_retried.insert(key.clone()) {
                let buster = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                let path = format!(
                    "{}&__fresh={}",
                    version_list_path(&name.scope, &name.name, &platform),
                    buster
                );
                if let Ok((fresh_data, fresh_status)) = api_request(&path, Method::GET, None, None).await {
                    if fresh_status.is_success() {
                        if let Some(state) = resolved.get_mut(&key) {
                            merge_fresh_version_list(state, &fresh_data);
                        }
                    }
                }
                queue.push_front((name, version_range, depth));
                continue;
            }
            if !raw_matches.is_empty() {
                anyhow::bail!(
                    "Every version of {} matching {} is excluded by forest.json; remove or narrow the exclusion with `forest exclude {} --remove`",
                    name.full_name, version_range, name.full_name
                );
            }
            anyhow::bail!("No versions found for {} matching {}", name.full_name, version_range);
        }

        // determine bucket
        matches.sort_by(|a,b| Version::parse(b).unwrap().cmp(&Version::parse(a).unwrap()));
        let mut agreed = matches[0].clone();
        for (bucket_ver, ranges) in pkg_state.buckets.clone() {
            let mut in_bucket: Vec<String> = all_versions.iter()
                .filter(|v| req.matches(&Version::parse(v).unwrap()))
                .filter(|v| !version_excluded(exclude_req, v))
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

        // Metadata source: fat list block (network-fresh), else disk cache
        // (queued for confirmation), else per-version request (network-fresh
        // and stored into the disk cache). All three share the trimmed
        // shape, so the code below is source-agnostic.
        let package_info = if let Some(block) = vs.prefetched.take() {
            block
        } else if let Some(cached) = meta_cache.as_ref()
            .and_then(|cache| cache.lookup(&platform, &name.full_name, &agreed))
        {
            confirm_queue.push(CacheSourced {
                scope: name.scope.clone(),
                pkg_name: name.name.clone(),
                full_name: name.full_name.clone(),
                version: agreed.clone(),
                cached: cached.clone(),
            });
            cached
        } else {
            let path = format!(
                "v1/package/{}/{}/{}/{}",
                name.scope.to_lowercase(), platform, name.name.to_lowercase(), agreed
            );
            let (response, status) = packages_api_request(&path, Method::GET, None, None).await
                .with_context(|| format!("Failed to fetch package info for {}@{}", name.full_name, agreed))?;

            if !status.is_success() {
                return Err(anyhow::anyhow!(
                    "Failed to fetch package info for {}@{}: HTTP {}",
                    name.full_name, agreed, status
                ));
            }
            let trimmed = trim_install_meta(&response, None);
            if let Some(cache) = meta_cache.as_ref() {
                cache.store(&platform, &name.full_name, &agreed, &trimmed);
            }
            trimmed
        };

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
                    // older publishes (`alias: "<scope>/<name>"`), never a
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

        // Rewrite overridden edges before they are stored or queued, so
        // bucket grouping and the phase-2 dep lookup both see the forced
        // range. The alias (install folder name) stays the parent's.
        let deps_hm: HashMap<String, DepSpec> = deps_hm.into_iter()
            .map(|(dep_name, mut spec)| {
                if let Some(forced) = overrides_lc.get(&dep_name.to_lowercase()) {
                    override_hits.entry(dep_name.to_lowercase()).or_default().push(spec.version.clone());
                    spec.version = forced.clone();
                }
                (dep_name, spec)
            })
            .collect();

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

        // Absent/empty = the historic "Packages" (all pre-field versions and
        // every wally-mirrored version). Validation happens where the value
        // flows into filesystem paths (the install planner).
        vs.packages_dir = package_info.get("packagesDir")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .unwrap_or_else(default_packages_dir);


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

    // 1.5) Confirm disk-cached entries against the registry before anything
    // derived from them reaches a lockfile. One concurrent wave, so the
    // serial latency this file exists to avoid does not come back. After
    // this, every hash in the lockfile came from the registry this session.
    if !confirm_queue.is_empty() {
        msg.update(&format!(
            "Confirming {} cached package version{}...",
            confirm_queue.len(),
            if confirm_queue.len() == 1 { "" } else { "s" }
        ));
        let limiter = Arc::new(tokio::sync::Semaphore::new(PREFETCH_CONCURRENCY));
        let mut handles = Vec::new();
        for CacheSourced { scope, pkg_name, full_name, version, cached } in confirm_queue.drain(..) {
            let limiter = Arc::clone(&limiter);
            let platform = platform.clone();
            handles.push(tokio::spawn(async move {
                let _permit = limiter.acquire_owned().await.expect("semaphore closed");
                let path = format!(
                    "v1/package/{}/{}/{}/{}",
                    scope.to_lowercase(), platform, pkg_name.to_lowercase(), version
                );
                let (response, status) = packages_api_request(&path, Method::GET, None, None).await
                    .with_context(|| format!("Failed to confirm cached metadata for {}@{}", full_name, version))?;
                if !status.is_success() {
                    return Err(anyhow::anyhow!(
                        "Failed to confirm cached metadata for {}@{}: HTTP {}",
                        full_name, version, status
                    ));
                }
                let fresh = trim_install_meta(&response, None);
                Ok::<_, anyhow::Error>((full_name, version, cached, fresh))
            }));
        }
        let mut mismatched: Vec<String> = Vec::new();
        for handle in handles {
            let (full_name, version, cached, fresh) = handle.await
                .map_err(|e| anyhow::anyhow!("Metadata confirmation task panicked: {e}"))??;
            if cached != fresh {
                if let Some(cache) = meta_cache.as_ref() {
                    cache.evict(&platform, &full_name, &version);
                }
                mismatched.push(format!("{}@{}", full_name, version));
            }
        }
        if !mismatched.is_empty() {
            mismatched.sort();
            return Err(anyhow::Error::new(MetaCacheMismatch { packages: mismatched }));
        }
    }

    // 2) Build lockfile entries; keyed by the canonical casing, while dep
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
                packages_dir: vs.packages_dir.clone(),
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
        // pre-canonicalization installs); the lowercased key still hits.
        if let Some(state) = resolved.get(&name.to_lowercase()) {
            if !state.canonical.eq_ignore_ascii_case(name) {
                root_renames.insert(name.clone(), state.canonical.clone());
            }
            let req = VersionReq::parse(&dep_spec.version)
                .with_context(|| format!("Invalid range {} for {}", dep_spec.version, name))?;

            // Use the version the lockfile actually holds for this root, not
            // the max satisfying registry version. A dep's tighter constraint
            // can merge the root into a lower bucket, and the registry max
            // would then name a version with no lockfile entry, so build_tree
            // silently no-ops and the root never gets planned.
            let root_version = select_root_bucket(state.buckets.keys(), &req)
                .ok_or_else(|| anyhow::anyhow!("No versions found for {} matching {}", name, dep_spec.version))?;

            // Lockfile keys use canonical casing, not the manifest's
            build_tree(&state.canonical, &dep_spec.alias, &root_version, "~", &mut lockfile);
        }
    }

    // 4) Solve report: which overrides/excludes did nothing (unused) and
    // which no longer change the outcome (unnecessary/inert).
    let mut report = SolveReport::default();
    report.override_edges = override_hits.values().map(|hits| hits.len()).sum();
    for (key, range) in overrides {
        let lc = key.to_lowercase();
        let Some(hits) = override_hits.get(&lc) else {
            report.override_unused.push(key.clone());
            continue;
        };
        let (Some(state), Ok(override_req)) = (resolved.get(&lc), VersionReq::parse(range)) else {
            continue;
        };
        // "Natural resolution" respects exclusions, so judge the override
        // against the non-excluded pool.
        let exclude_req = excludes_lc.get(&lc);
        let available: Vec<Version> = state.versions.keys()
            .filter(|v| !version_excluded(exclude_req, v))
            .filter_map(|v| Version::parse(v).ok())
            .collect();
        let mut declared: Vec<String> = hits.clone();
        declared.sort();
        declared.dedup();
        if override_is_unnecessary(&available, &declared, &override_req) {
            report.override_unnecessary.push(key.clone());
        }
    }
    for (key, _) in excludes {
        let lc = key.to_lowercase();
        let Some(state) = resolved.get(&lc) else {
            report.exclude_unused.push(key.clone());
            continue;
        };
        let (Some(ranges), Some(exclude_req)) = (exclude_ranges_seen.get(&lc), excludes_lc.get(&lc)) else {
            continue;
        };
        let available: Vec<Version> = state.versions.keys()
            .filter_map(|v| Version::parse(v).ok())
            .collect();
        let mut ranges = ranges.clone();
        ranges.sort();
        ranges.dedup();
        if exclusion_is_inert(&available, &ranges, exclude_req) {
            report.exclude_inert.push(key.clone());
        }
    }
    report.override_unused.sort();
    report.override_unnecessary.sort();
    report.exclude_unused.sort();
    report.exclude_inert.sort();
    archived_packages.sort_by(|a, b| a.name.cmp(&b.name));
    report.archived = archived_packages;

    Ok((lockfile, license_warnings, root_renames, report))
}

/// True when the version string parses and the exclusion range bans it.
fn version_excluded(exclude_req: Option<&VersionReq>, v: &str) -> bool {
    match (exclude_req, Version::parse(v)) {
        (Some(req), Ok(ver)) => req.matches(&ver),
        _ => false,
    }
}

/// True when no requested range would naturally pick a banned version:
/// the exclusion currently changes nothing (a newer allowed version now
/// outranks the banned ones everywhere).
fn exclusion_is_inert(available: &[Version], ranges: &[String], exclude_req: &VersionReq) -> bool {
    !ranges.is_empty()
        && ranges.iter().all(|range| {
            let Ok(req) = VersionReq::parse(range) else {
                return false;
            };
            available.iter()
                .filter(|v| req.matches(v))
                .max()
                .map_or(true, |natural| !exclude_req.matches(natural))
        })
}

/// True when every declared range the override replaced would naturally
/// resolve to a version the override also accepts; removing the override
/// would not change any edge it rewrote. A declared range matching nothing
/// keeps the override load-bearing.
fn override_is_unnecessary(available: &[Version], declared_ranges: &[String], override_req: &VersionReq) -> bool {
    !declared_ranges.is_empty()
        && declared_ranges.iter().all(|range| {
            let Ok(req) = VersionReq::parse(range) else {
                return false;
            };
            available.iter()
                .filter(|v| req.matches(v))
                .max()
                .map_or(false, |natural| override_req.matches(natural))
        })
}


/// Highest bucket version satisfying `req`. Roots processed by the BFS
/// always have a matching bucket, so None is defensive.
fn select_root_bucket<'a>(buckets: impl Iterator<Item = &'a String>, req: &VersionReq) -> Option<String> {
    buckets
        .filter_map(|v| Version::parse(v).ok().map(|parsed| (v, parsed)))
        .filter(|(_, parsed)| req.matches(parsed))
        .max_by(|(_, a), (_, b)| a.cmp(b))
        .map(|(v, _)| v.clone())
}

pub async fn _test() -> Result<()> {

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(range: &str) -> VersionReq {
        VersionReq::parse(range).unwrap()
    }

    fn buckets(keys: &[&str]) -> Vec<String> {
        keys.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn root_annotates_from_its_bucket_not_the_registry_max() {
        // A dep's ~1.2.0 merged the root into the 1.2.3 bucket while the
        // registry also has 1.5.0. Picking the registry max meant a version
        // with no lockfile entry, so the root was never annotated.
        let b = buckets(&["1.2.3"]);
        assert_eq!(
            select_root_bucket(b.iter(), &req("^1.0.0")),
            Some("1.2.3".to_string())
        );
    }

    #[test]
    fn highest_matching_bucket_wins() {
        let b = buckets(&["1.2.3", "2.0.0", "1.5.0"]);
        assert_eq!(
            select_root_bucket(b.iter(), &req("^1.0.0")),
            Some("1.5.0".to_string())
        );
    }

    #[test]
    fn no_matching_bucket_is_none() {
        let b = buckets(&["2.0.0"]);
        assert_eq!(select_root_bucket(b.iter(), &req("^1.0.0")), None);
    }

    fn versions(list: &[&str]) -> Vec<Version> {
        list.iter().map(|v| Version::parse(v).unwrap()).collect()
    }

    #[test]
    fn override_still_needed_when_parents_resolve_outside_it() {
        // Parent declares ^1.0.0, override forces ^2.0.0: removing the
        // override would land on 1.4.0.
        let avail = versions(&["1.4.0", "2.1.2"]);
        assert!(!override_is_unnecessary(
            &avail,
            &["^1.0.0".to_string()],
            &req("^2.0.0")
        ));
    }

    #[test]
    fn override_unnecessary_when_natural_resolution_already_satisfies_it() {
        // Parent bumped to ^2.0.0 upstream; the override changes nothing.
        let avail = versions(&["1.4.0", "2.1.2"]);
        assert!(override_is_unnecessary(
            &avail,
            &["^2.0.0".to_string()],
            &req("^2.0.0")
        ));
    }

    #[test]
    fn override_kept_when_any_parent_still_needs_it() {
        let avail = versions(&["1.4.0", "2.1.2"]);
        assert!(!override_is_unnecessary(
            &avail,
            &["^1.0.0".to_string(), "^2.0.0".to_string()],
            &req("^2.0.0")
        ));
    }

    #[test]
    fn override_kept_when_declared_range_matches_nothing() {
        // Without the override the install would fail outright.
        let avail = versions(&["2.1.2"]);
        assert!(!override_is_unnecessary(
            &avail,
            &["^1.0.0".to_string()],
            &req("^2.0.0")
        ));
    }

    #[test]
    fn exclusion_active_while_a_banned_version_is_the_natural_pick() {
        // ^1.5.0 would naturally take 1.6.0; the exclusion is doing work.
        let avail = versions(&["1.5.2", "1.6.0"]);
        assert!(!exclusion_is_inert(
            &avail,
            &["^1.5.0".to_string()],
            &req("=1.6.0")
        ));
    }

    #[test]
    fn exclusion_inert_once_a_newer_allowed_version_wins() {
        // 1.6.1 shipped; every range now lands past the banned 1.6.0.
        let avail = versions(&["1.5.2", "1.6.0", "1.6.1"]);
        assert!(exclusion_is_inert(
            &avail,
            &["^1.5.0".to_string()],
            &req("=1.6.0")
        ));
    }

    #[test]
    fn exclusion_stays_active_if_any_range_still_hits_it() {
        let avail = versions(&["1.5.2", "1.6.0", "1.6.1"]);
        assert!(!exclusion_is_inert(
            &avail,
            &["^1.5.0".to_string(), "=1.6.0".to_string()],
            &req("=1.6.0")
        ));
    }

    #[test]
    fn version_excluded_requires_a_parseable_version() {
        assert!(version_excluded(Some(&req("=1.6.0")), "1.6.0"));
        assert!(!version_excluded(Some(&req("=1.6.0")), "1.6.1"));
        assert!(!version_excluded(None, "1.6.0"));
        assert!(!version_excluded(Some(&req("=1.6.0")), "not-a-version"));
    }

    fn state_with(versions: &[(&str, bool)]) -> PackageState {
        let mut vs = HashMap::new();
        for (ver, resolved) in versions {
            vs.insert(ver.to_string(), VersionState {
                resolved: *resolved,
                prefetched: None,
                dependencies: HashMap::new(),
                integrity: if *resolved { "kept".into() } else { String::new() },
                public: false,
                archive_root: String::new(),
                packages_dir: default_packages_dir(),
            });
        }
        PackageState { canonical: "Scope/Pkg".into(), buckets: HashMap::new(), versions: vs }
    }

    #[test]
    fn fresh_merge_adds_unseen_versions_with_their_install_blocks() {
        let mut state = state_with(&[("1.0.0", true)]);
        merge_fresh_version_list(&mut state, &serde_json::json!({
            "public": true,
            "versions": [
                { "version": "1.0.0", "install": { "dependencies": {}, "integrity": "would-clobber" } },
                { "version": "1.0.1", "install": { "dependencies": {}, "integrity": "abc" } },
                { "version": "1.0.2" }
            ]
        }));

        // The already-resolved entry is untouched; a fresh list must never
        // rewrite state the BFS already consumed.
        assert_eq!(state.versions["1.0.0"].integrity, "kept");
        assert!(state.versions["1.0.0"].prefetched.is_none());

        // New version with an install block: prefetched, package-level
        // public injected. Without a block: listed, resolves per-version.
        let added = &state.versions["1.0.1"];
        let block = added.prefetched.as_ref().expect("install block prefetched");
        assert_eq!(block["integrity"], "abc");
        assert_eq!(block["public"], true);
        assert!(state.versions["1.0.2"].prefetched.is_none());
    }

    #[test]
    fn fresh_merge_tolerates_slim_or_malformed_responses() {
        let mut state = state_with(&[("1.0.0", false)]);
        merge_fresh_version_list(&mut state, &serde_json::json!({ "error": "whoops" }));
        merge_fresh_version_list(&mut state, &serde_json::json!({
            "versions": [{ "notVersion": true }, { "version": "2.0.0" }]
        }));
        assert_eq!(state.versions.len(), 2);
        assert!(state.versions.contains_key("2.0.0"));
    }

    #[test]
    fn version_list_urls_are_lowercased_and_ask_for_install_detail() {
        assert_eq!(
            version_list_path("ChiefWildin", "AnimNation", "roblox"),
            "v1/package/chiefwildin/roblox/animnation?detail=install"
        );
    }
}
