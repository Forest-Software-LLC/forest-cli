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
    dependencies: HashMap<String, String>,
    integrity: String,
    access_url : String
}

/// Holds buckets (grouped ranges) and per-version state
#[derive(Debug)]
struct PackageState {
    buckets: HashMap<String, Vec<String>>,
    versions: HashMap<String, VersionState>,
}

type ResolvedVersions = HashMap<String, PackageState>;

/// Lockfile entry for a package version
#[derive(Debug, Serialize, Deserialize)]
pub struct LockfileEntry {
    pub version: String,
    pub resolved: String,
    integrity: String,
    pub location: String,
    pub dependencies: HashMap<String, String>,
}

type LockfilePackages = HashMap<String, Vec<LockfileEntry>>;

pub fn get_dupe_name(target : String, deps : Vec<String>) -> String {
    let dep_name_info = digest_package_name(&target);

    let mut exists = false;
    for target_name in &deps {
        if target_name == &target {
            continue; // Skip exact matches
        }

        let target_name_info = digest_package_name(target_name);
        if target_name_info.name == dep_name_info.name {
            exists = true;
            break;
        }
    }


    return if exists {
        format!("{}-{}", dep_name_info.scope, dep_name_info.name)
    } else {
        dep_name_info.name
    };
}

pub async fn get_lockfile_packages(root_deps: HashMap<String, String>, platform : String) -> Result<LockfilePackages> {
    let mut resolved: ResolvedVersions = HashMap::new();

    // Make queue with digest_package_name

    let mut queue: VecDeque<(PackageName, String, u8)> = root_deps.clone().into_iter()
        .map(|(name, range)| (digest_package_name(&name), range, 1))
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

            for val in versions {
                let ver = String::from(val.as_str()
                    .ok_or_else(|| anyhow::anyhow!("Version is not a string for {}", name.full_name))?);

                //println!("Found version {} for package {}", ver, name.full_name);
                pkg_state.versions.insert(
                    ver,
                    VersionState { resolved: false, dependencies: HashMap::new(), integrity: String::new(), access_url: String::new() }
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

        let deps = package_info.get("dependencies")
            .and_then(|v| v.as_object())
            .ok_or_else(|| anyhow::anyhow!("Invalid dependencies data for {}@{}", name.full_name, agreed))?;

        let deps_hm: HashMap<String, String> = deps.clone().into_iter()
            .map(|(k, v)| {
                let s = v.as_str()
                    .ok_or_else(|| anyhow::anyhow!("Dependency version for {}@{} is not a string", name.full_name, agreed))
                    .unwrap()
                    .to_string();
                (k, s)
            })
            .collect();

        vs.dependencies = deps_hm.clone();
        vs.integrity = package_info.get("integrity")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();

        vs.access_url = package_info.get("accessUrl")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_default();

        for (dep_name, dep_range) in deps_hm {
            queue.push_front((digest_package_name(&dep_name), dep_range, depth + 1));
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
                    .find(|v| VersionReq::parse(dr).unwrap().matches(&Version::parse(v).unwrap()))
                    .cloned().unwrap();
                deps.insert(dn.clone(), v);
            }
            entries.push(LockfileEntry {
                version: bucket_ver.clone(),
                resolved: vs.access_url.clone(),//"https://registry.forestpm.dev/".into(),
                integrity: vs.integrity.clone(),
                location: String::new(),
                dependencies: deps,
            });
        }
        lockfile.insert(pkg.clone(), entries);
    }

    // 3) Annotate locations with tree positions
    

    fn build_tree(
        name: &str,
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
                let deps: Vec<(String, String)> = entry.dependencies.iter()
                    .map(|(dn, dv)| (dn.clone(), dv.clone()))
                    .collect();

                // Also collect dependency keys for each dependency

                for (dn, dv) in deps.clone().into_iter() {
                    let dep_names: Vec<String> = deps.iter().map(|(dn, _)| dn.clone()).collect();
                    let name_for_next = get_dupe_name(dn.clone(), dep_names);
                    let next_loc = format!("{}/{}", loc, name);
                    build_tree(&name_for_next, &dv, &next_loc, lockfile);
                }
            }
        }
    }

    for (name, ver_range) in &root_deps {
        if let Some(state) = resolved.get(name) {
            // Only get keys that satisfy the version range
            let req = VersionReq::parse(ver_range)
                .with_context(|| format!("Invalid range {} for {}", ver_range, name))?;

            let mut versions: Vec<String> = state.versions.keys()
                .filter(|v| req.matches(&Version::parse(v).unwrap()))
                .cloned()
                .collect();

            if versions.is_empty() {
                return Err(anyhow::anyhow!("No versions found for {} matching {}", name, ver_range));
            }

            versions.sort_by(|a,b| Version::parse(b).unwrap().cmp(&Version::parse(a).unwrap()));

            if let Some(first) = versions.first() {
                // Collect dependencies to avoid holding a mutable borrow during recursion
                let deps: Vec<(String, String)> = root_deps.clone().iter()
                    .map(|(dn, dv)| (dn.clone(), dv.clone()))
                    .collect();

                let dep_names: Vec<String> = deps.iter().map(|(dn, _)| dn.clone()).collect();
                let name_for_next =  get_dupe_name(name.clone(), dep_names);
                build_tree(&name_for_next, first, "~", &mut lockfile);
            }
        }
    }

    Ok(lockfile)
}


pub async fn test() -> Result<()> {
    let mut roots = HashMap::new();
    roots.insert("test-2a".into(), "^0.1.0".into());
    roots.insert("test-3a".into(), "^0.1.0".into());
    roots.insert("test-b".into(), "^0.1.0".into());
    get_lockfile_packages(roots, "roblox".to_string()).await?;
    Ok(())
}
