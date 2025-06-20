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
use reqwest::{Client, Method};
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

/// Tracks per-version resolution state
#[derive(Debug)]
struct VersionState {
    resolved: bool,
    dependencies: HashMap<String, String>,
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
    resolved: String,
    integrity: String,
    pub location: String,
    pub dependencies: HashMap<String, String>,
}

type LockfilePackages = HashMap<String, Vec<LockfileEntry>>;

/// HTTP responses
#[derive(Deserialize)]
struct VersionsData { versions: Vec<String> }

#[derive(Deserialize)]
struct PackageInfo { dependencies: Option<HashMap<String, String>> }

const BASE_URL: &str = "http://localhost:3000/";

async fn make_request<T: serde::de::DeserializeOwned>(
    client: &Client,
    method: Method,
    path: &str,
) -> Result<T> {
    let url = format!("{}{}", BASE_URL, path);
    let resp = client
        .request(method, &url)
        .header("Content-Type", "application/json")
        .send().await
        .with_context(|| format!("Failed request to {}", url))?
        .error_for_status()
        .with_context(|| format!("HTTP error from {}", url))?;
    let data = resp.json::<T>().await
        .with_context(|| format!("Invalid JSON from {}", url))?;
    Ok(data)
}

pub async fn get_lockfile_packages(root_deps: HashMap<String, String>) -> Result<LockfilePackages> {
    let client = Client::new();
    let mut resolved: ResolvedVersions = HashMap::new();
    let mut queue: VecDeque<(String, String)> = root_deps.clone().into_iter().collect();

    // 1) Resolve dependency graph into buckets & versions
    while let Some((name, version_range)) = queue.pop_front() {
        let pkg_state = resolved.entry(name.clone())
            .or_insert_with(|| PackageState {
                buckets: HashMap::new(),
                versions: HashMap::new(),
            });

        // fetch available versions
        if pkg_state.versions.is_empty() {
            let path = format!("v1/package/user1/{}", name);
            let data: VersionsData = make_request(&client, Method::GET, &path).await
                .with_context(|| format!("Failed to fetch versions for {}", name))?;
            for v in data.versions {
                pkg_state.versions.insert(
                    v.clone(),
                    VersionState { resolved: false, dependencies: HashMap::new() }
                );
            }
        }

        // filter by range
        let req = VersionReq::parse(&version_range)
            .with_context(|| format!("Invalid range {} for {}", version_range, name))?;
        let all_versions: Vec<String> = pkg_state.versions.keys().cloned().collect();
        let mut matches: Vec<String> = all_versions.iter()
            .filter(|v| Version::parse(v).map(|ver| req.matches(&ver)).unwrap_or(false))
            .cloned()
            .collect();
        if matches.is_empty() {
            anyhow::bail!("No versions found for {} matching {}", name, version_range);
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
        let path = format!("v1/package/user1/{}/{}", name, agreed);
        let info: PackageInfo = make_request(&client, Method::GET, &path).await
            .with_context(|| format!("Failed to fetch {}@{}", name, agreed))?;
        vs.resolved = true;
        let deps = info.dependencies.unwrap_or_default();
        vs.dependencies = deps.clone();
        for (dn, dr) in deps {
            queue.push_front((dn, dr));
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
                resolved: "https://registry.forestpm.dev/".into(),
                integrity: "abc-1234".into(),
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

                for (dn, dv) in deps {
                    let next_loc = format!("{}/{}", loc, name);
                    build_tree(&dn, &dv, &next_loc, lockfile);
                }
            }
        }
    }

    for (name, _) in &root_deps {
        if let Some(state) = resolved.get(name) {
            let mut keys: Vec<&String> = state.buckets.keys().collect();
            keys.sort_by(|a,b| Version::parse(b).unwrap().cmp(&Version::parse(a).unwrap()));
            if let Some(first) = keys.first() {
                build_tree(name, first, "~", &mut lockfile);
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
    get_lockfile_packages(roots).await?;
    Ok(())
}
