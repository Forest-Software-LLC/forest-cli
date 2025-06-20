use std::{collections::HashMap, fs, path::Path};
use anyhow::{anyhow, Context, Ok, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use urlencoding::encode;
use semver::{Version, VersionReq};
use reqwest::blocking::Client;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::http::api_request;
use crate::lockfile_solver::{get_lockfile_packages, LockfileEntry};
use crate::message::Message;
use crate::fetch_and_extract::fetch_and_extract;


/// The overall lockfile structure.
#[derive(Debug, Serialize, Deserialize)]
pub struct LockFile {
    #[serde(rename = "fileVersion")]
    pub file_version: u32,
    pub packages: HashMap<String, Vec<LockfileEntry>>,
}

/// Create directories for all packages in the lockfile, fetching & extracting them.
/*pub fn make_directories(lockfile: &LockFile) -> Result<()> {
    if !Path::new("packages").exists() {
        fs::create_dir_all("packages")?;
    } else {
        // Clear existing packages directory
        //TODO: Preserve existing packages if needed
        for entry in fs::read_dir("packages")? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            }
        }
    }

    let mp = MultiProgress::new();
    let style = ProgressStyle::with_template("{bar:40.cyan/blue} {msg}")?
        .progress_chars("=> ");

    let mut workers = Vec::new();
    let mut non_primary = HashMap::new();

    for (pkg_name, versions) in &lockfile.packages {
        for version in versions {
            let dir_path = Path::new(&version.location).join(pkg_name);
            if !dir_path.exists() {
                fs::create_dir_all(&dir_path)?;
            }
            let bar = mp.add(ProgressBar::new(100));
            bar.set_style(style.clone());
            bar.set_message(format!("{} @ {}", pkg_name, version.version));

            let url = "https://registry.forestpm.dev/public/eden.tgz".to_string();
            let dir_clone = dir_path.clone();
            let bar_clone = bar.clone();

            let handle = std::thread::spawn(move || -> Result<()> {
                fetch_and_extract(&url, &dir_clone, bar_clone)?;
                Ok(())
            });
            workers.push((bar, handle));

            let mut has_primary = false;
            let mut dep_count : u16 = 0;
            for (dep_name, dep_info) in &version.dependencies {
                dep_count += 1;
                if dep_info.primary {
                    has_primary = true;
                } else {
                    let loc = dir_path.join("packages").join(dep_name);
                    non_primary.insert(
                        loc.to_string_lossy().to_string(),
                        (dep_name.clone(), dep_info.version.clone()),
                    );
                }
            }
            if !has_primary && dep_count > 0 {
                let nested = dir_path.join("packages");
                if !nested.exists() {
                    fs::create_dir_all(nested)?;
                }
            }
        }
    }

    for (loc, (name, version)) in non_primary {
        let path = Path::new(&loc);
        if !path.exists() {
            fs::create_dir_all(path)?;
        }
        let parts: Vec<&str> = loc.split('/').collect();
        let prefix = format!("script{}", ".Parent".repeat(parts.len().saturating_sub(1)));

        let target_location = lockfile
            .packages
            .get(&name)
            .and_then(|lst| lst.iter().find(|p| p.version == version).map(|p| p.location.clone()))
            .ok_or_else(|| anyhow!("Target location for {} @ {} not found in lockfile.", name, version))?;

        let path_from_root = {
            let split: Vec<&str> = target_location.split('/').skip(1).collect();
            if split.is_empty() {
                String::new()
            } else {
                format!("{}", split.join("\"][\""))
            }
        };

        let lua_path = format!("{}{}[\"{}\"]", prefix, path_from_root, name);
        let init_lua = format!(
            "--Pointer file ({}/{})\nreturn require({})",
            target_location, name, lua_path
        );
        fs::write(path.join("init.lua"), init_lua)?;
    }

    for (bar, handle) in workers {
        bar.set_position(100);
        bar.finish();
        if let Err(e) = handle.join() {
            return Err(anyhow!("Fetch thread panicked: {:?}", e));
        }
    }
    mp.clear().ok();
    Ok(())
}*/
pub async fn make_directories(lockfile: &LockFile) -> Result<()> {
    // Make directories for all packages
    if !Path::new("packages").exists() {
        fs::create_dir_all("packages")?;
    } else {
        // Clear existing packages directory
        for entry in fs::read_dir("packages")? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            }
        }
    }

    // Create directories for each package version
    let mp = MultiProgress::new();
    let style = ProgressStyle::with_template("{bar:40.cyan/blue} {msg}")?
        .progress_chars("=> ");

    let mut workers = Vec::new();

    let mut path_cache : HashMap<String, HashMap<String, String>>   = HashMap::new();
    for (pkg_name, versions) in &lockfile.packages {
        for version_data in versions {
            let mut path_parts: Vec<&str> = version_data.location.split('/').collect();
            path_parts.remove(0);

            let path = format!("./packages/{}", path_parts.join("/packages/"));
            let dir_path = Path::new(&path).join(pkg_name);
            if !dir_path.exists() {
                fs::create_dir_all(&dir_path)?;
            }
            //TODO: Download and extract the package here

            let bar = mp.add(ProgressBar::new(100));
            bar.set_style(style.clone());
            bar.set_message(format!("{} @ {}", pkg_name, version_data.version));
            
            let url = "https://registry.forestpm.dev/public/eden.tgz".to_string();
            let dir_clone = dir_path.clone();
            let bar_clone = bar.clone();

            let handle = std::thread::spawn(move || -> Result<()> {
                fetch_and_extract(&url, &dir_clone, bar_clone)?;
                Ok(())
            });
            workers.push((bar, handle));
    
            // Store the path in cache
            path_cache.entry(pkg_name.clone())
                .or_default()
                .insert(version_data.version.clone(), path.clone());
        }
    }

    // Wait for all download threads to finish
    for (bar, handle) in workers {
        bar.set_position(100);
        bar.finish();
        if let Err(e) = handle.join() {
            return Err(anyhow!("Fetch thread panicked: {:?}", e));
        }
    }

    // Make pointer files
    for (pkg_name, versions) in &lockfile.packages {
        for version_data in versions {
            let true_path = path_cache
                .get(pkg_name)
                .and_then(|v| v.get(&version_data.version))
                .ok_or_else(|| anyhow!("Path for {} @ {} not found in cache.", pkg_name, version_data.version))?;

            

            for (dep_name, dep_version) in &version_data.dependencies {
                let mut init_lua = String::new();
                init_lua.push_str("--Pointer file\nreturn \n");

                let dep_true_path = path_cache
                    .get(dep_name)
                    .and_then(|v| v.get(dep_version))
                    .ok_or_else(|| anyhow!("Path for dependency {} @ {} not found in cache.", dep_name, dep_version))?;

                // Count shared ancestors on dep_true_path and true_path
                let mut shared_ancestors : u16 = 0;
                let dep_parts: Vec<&str> = dep_true_path.split('/').collect();
                let true_parts: Vec<&str> = true_path.split('/').collect();
                for (d, t) in dep_parts.iter().zip(true_parts.iter()) {
                    if d == t {
                        shared_ancestors += 1;
                    } else {
                        break;
                    }
                }

                let parent_count = dep_parts.len() - shared_ancestors as usize;
                let lua_path = format!("script{}", (".Parent").repeat(parent_count));
                // Create the relative path for the dependency
                
                let mut relative_path = dep_parts
                    .iter()
                    .skip(shared_ancestors as usize)
                    .map(|s| encode(s).into_owned())
                    .collect::<Vec<String>>()
                    .join("']['");


                relative_path = format!("{}['{}']", lua_path, relative_path);

                init_lua.push_str(&format!(
                    "require({})",
                    relative_path
                ));

                fs::write(format!("{}/{}/init.lua", true_path, dep_name), init_lua)?;
            }
        }
    }

    Ok(())
}

/// Generate a lockfile JSON string given the forest manifest & message spinner.
pub async fn lockfile_gen(forest_json: &Value, msg: &mut Message) -> Result<LockFile> {
    let roots : HashMap<String, String> = forest_json
        .get("dependencies")
        .and_then(|deps| deps.as_object())
        .map_or_else(HashMap::new, |deps| {
            deps.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
                .collect()
        }); 

    msg.update("Generating lockfile...");
    let lockfile_packages = get_lockfile_packages(roots).await
        .context("Failed to resolve lockfile packages")?;

    let lockfile : LockFile = LockFile {
        file_version: 1,
        packages: lockfile_packages
    };


    make_directories(&lockfile).await
        .context("Failed to create directories for lockfile packages")?;
    

    Ok(lockfile)
}