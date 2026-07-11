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
use anyhow::{Result, Context};
use reqwest::{Method};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use crate::http::api_request;
use crate::utils::{digest_package_name, PackageName };

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
    buckets: HashMap<String, Vec<String>>,
    versions: HashMap<String, VersionState>,
}

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

/// Resolves the dependency graph. Also returns license-safety warnings for any
/// resolved version the registry rated caution/unsafe — each version is fetched
/// exactly once, so warnings are naturally deduplicated.
pub async fn get_lockfile_packages(root_deps: HashMap<String, DepSpec>, platform : String) -> Result<(LockfilePackages, Vec<String>)> {
    let mut resolved: ResolvedVersions = HashMap::new();
    let mut license_warnings: Vec<String> = Vec::new();

    // Make queue with digest_package_name using normalized specs
    let mut queue: VecDeque<(PackageName, String, u8)> = root_deps.clone().into_iter()
        .map(|(name, spec)| (digest_package_name(&name), spec.version, 1))
        .collect();

    // 1) Resolve dependency graph into buckets & versions

    while let Some((name, version_range, depth)) = queue.pop_front() {
        let pkg_state = resolved.entry(name.full_name.clone())
            .or_insert_with(|| PackageState {
                buckets: HashMap::new(),
                versions: HashMap::new(),
            });

        // fetch available versions
        if pkg_state.versions.is_empty() {
            let path = format!("v1/package/{}/{}/{}", name.scope, platform, name.name);
            let (version_data, versions_status) = api_request(&path, Method::GET, None, None).await
                .with_context(|| format!("Failed to fetch package info for {}", name.full_name))?;

            if !versions_status.is_success() {
                Err(anyhow::anyhow!(
                    "Failed to fetch package info for {}: HTTP {}",
                    name.full_name, versions_status
                ))?;
            }
            let versions = version_data.get("versions")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow::anyhow!("Invalid versions data for {}", name.full_name))?;

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

        let (package_info, status) = api_request(&path, Method::GET, None, None).await
            .with_context(|| format!("Failed to fetch package info for {}@{}", name.full_name, agreed))?;

        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "Failed to fetch package info for {}@{}: HTTP {}",
                name.full_name, agreed, status
            ));
        }

        vs.resolved = true;

        if let Some(warning) = crate::licensce_helper::license_warning_for(&package_info, &format!("{}@{}", name.full_name, agreed)) {
            license_warnings.push(warning);
        }

        let deps = package_info.get("dependencies")
            .and_then(|v| v.as_object())
            .ok_or_else(|| anyhow::anyhow!("Invalid dependencies data for {}@{}", name.full_name, agreed))?;

        // Support both legacy string dependencies and new object form { alias, version }
        let deps_hm: HashMap<String, DepSpec> = deps.clone().into_iter()
            .map(|(k, v)| -> anyhow::Result<(String, DepSpec)> {
                if let Some(s) = v.as_str() {
                    // Legacy: value is a version string; alias defaults to the dep key
                    let spec = DepSpec { alias: k.clone(), version: s.to_string() };
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
                    let alias = obj.get("alias")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| k.clone());
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
                "Registry returned no integrity hash for {}@{} — cannot lock this version",
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
            queue.push_front((digest_package_name(&dep_name), dep_spec.version, depth + 1));
        }
    }

    // 2) Build lockfile entries
    let mut lockfile: LockfilePackages = HashMap::new();
    for (pkg, state) in &resolved {
        let mut entries = Vec::new();
        for bucket_ver in state.buckets.keys() {
            let vs = &state.versions[bucket_ver];
            let mut deps = HashMap::new();
            for (dn, dr) in &vs.dependencies {
                let dep_state = &resolved[dn];
                let v = dep_state.buckets.keys()
                    .find(|v| VersionReq::parse(&dr.version).unwrap().matches(&Version::parse(v).unwrap()))
                    .cloned().unwrap();
                deps.insert(dn.clone(), DepSpec{
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
        lockfile.insert(pkg.clone(), entries);
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

    for (name, dep_spec) in &root_deps {
        if let Some(state) = resolved.get(name) {
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
                build_tree(name, &dep_spec.alias, first, "~", &mut lockfile);
            }
        }
    }

    Ok((lockfile, license_warnings))
}


pub async fn _test() -> Result<()> {
   
    Ok(())
}
