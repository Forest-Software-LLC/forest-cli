use std::{collections::HashMap, fs, path::Path};
use anyhow::{anyhow, Context, Ok, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use urlencoding::encode;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::utils::normalize_forest_deps;
use crate::lockfile_solver::{get_lockfile_packages, DepSpec, LockfileEntry};
use crate::message::Message;
use crate::fetch_and_extract::fetch_and_extract;
//use crate::utils::digest_package_name;


/// The overall lockfile structure.
#[derive(Debug, Serialize, Deserialize)]
pub struct LockFile {
    pub file_version: u32,
    pub packages: HashMap<String, Vec<LockfileEntry>>,
}

pub async fn make_directories(lockfile: &LockFile, root_deps: HashMap<String, DepSpec>) -> Result<()> {
    // `_`/`.`-prefixed folders in packages/ are exempt from install cleanup
    // (e.g. Wally's `_Index`), so aliases must not claim those names.
    for (pkg_name, spec) in &root_deps {
        if spec.alias.starts_with('_') || spec.alias.starts_with('.') {
            return Err(anyhow!(
                "Alias '{}' for {} cannot start with '_' or '.' — rename it in forest.json",
                spec.alias, pkg_name
            ));
        }
    }

    // Make directories for all packages
    if !Path::new("packages").exists() {
        fs::create_dir_all("packages")?;
    } else {
        // Clear existing packages directory, skipping `_` and dot-prefixed
        // entries: on case-insensitive filesystems `packages` is the same
        // directory as Wally's `Packages`, whose `_Index` must survive.
        for entry in fs::read_dir("packages")? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('_') || name.starts_with('.') {
                continue;
            }
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
       // let pkg_name_info = digest_package_name(pkg_name_string);
        for version_data in versions {
            let mut path_parts: Vec<&str> = version_data.location.split('/').collect();
            path_parts.remove(0);

            /*// get all packages with the same number of path parts
            let same_level_packages: Vec<String> = lockfile.packages.keys()
                .filter(|k| {
                    let path = lockfile.packages.get(*k)
                        .and_then(|v| v.first())
                        .map_or(false, |v| v.location.split('/').count()-1 == path_parts.len());
                    
                    path
                })
                .cloned()
                .collect();

            let dir_pkg_name = &get_dupe_name(pkg_name.clone(), same_level_packages);
            */

            let mut dir_pkg_name = root_deps.get(pkg_name)
                .and_then(|d| Some(d.alias.clone()))
                .unwrap_or_else(|| String::new());
            /*
                to find the real dir_pkg_name, backtrack from path_parts, starting from the top. 

                ex: pkg loc /a/n/c/d

                look for package from root_deps alias with alias "a"
                look for package "n" in found rootpackage deps
                repeat:
                look for packages in lockfile with location "a/n"
                look for package with alias "c" in found package deps

             */

            // Resolve alias path for the package 
            fn backtrack_name(
                lockfile: &LockFile,
                dep_name: &str,
                dep_version: &str,
                end_goal: &str,
                remaining_segments: &[&str]
            ) -> Result<String> {

                let deps = lockfile.packages
                    .get(dep_name)
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
                    } else if sub_dep_spec.alias == remaining_segments[0] {
                        println!("Trying next");
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

            if !path_parts.is_empty() {
                let target_root_dep_alias = path_parts[0];
                let mut root_dep_name = String::new();

                for (dep_name, dep_info) in &root_deps {
                    if dep_info.alias == target_root_dep_alias {
                        root_dep_name = dep_name.clone();

                        break;
                    }
                }

                let root_dep_ver = &lockfile.packages
                    .get(&root_dep_name)
                    .ok_or_else(|| anyhow!("Root dependency {} not found", root_dep_name))?
                    .iter()
                    .find(|v| v.location == "~")
                    .ok_or_else(|| anyhow!("Root Version not found for {}", root_dep_name))?;

                dir_pkg_name = backtrack_name(
                    &lockfile, 
                    &root_dep_name, 
                    &root_dep_ver.version, 
                    &pkg_name, 
                    &path_parts.clone()[1..]
                )?;
            } else if dir_pkg_name.is_empty() {
                return Err(anyhow!("Failed to determine directory package name for {}", pkg_name));
            }

            // Path stuff

            let mut path: String = format!("./packages/{}/packages", path_parts.join("/packages/"));
            if path_parts.is_empty() {
                path = "./packages".to_string();
            }

            let dir_path = Path::new(&path).join(&dir_pkg_name);
            if !dir_path.exists() {
                fs::create_dir_all(&dir_path)?;
            }

            let bar = mp.add(ProgressBar::new(100));
            bar.set_style(style.clone());
            bar.set_message(format!("{} @ {}", pkg_name, version_data.version));
            
            let url = version_data.resolved.clone();
            let dir_clone = dir_path.clone();
            let bar_clone = bar.clone();
            let root_clone = version_data.root.clone(); 

            let handle = std::thread::spawn(move || -> Result<()> {
                fetch_and_extract(&url, &dir_clone, &root_clone, bar_clone)?;
                bar.finish_and_clear();
                Ok(())
            });
            workers.push(handle);
    
            // Store the path in cache
            path_cache.entry(pkg_name.clone())
                .or_default()
                .insert(version_data.version.clone(), format!("{}/{}", path.clone(), dir_pkg_name));
        }
    }

    // Wait for all download threads to finish
    for handle in workers {
        if let Err(e) = handle.join() {
            return Err(anyhow!("Fetch thread panicked: {:?}", e));
        }
    }
 
    // Make pointer files
    for (pkg_name, versions) in &lockfile.packages {
        for version_data in versions {
            let cache_result = path_cache
                .get(pkg_name)
                .and_then(|v| v.get(&version_data.version))
                .ok_or_else(|| anyhow!("Path for {} @ {} not found in cache.", pkg_name, version_data.version))?;

            let mut true_path = cache_result.clone();
            true_path.push_str("/packages");
            

            for (dep_name, dep_version) in &version_data.dependencies {
                
                let dep_true_path = path_cache
                    .get(dep_name)
                    .and_then(|v| v.get(&dep_version.version))
                    .ok_or_else(|| anyhow!("Path for dependency {} @ {} not found in cache.", dep_name, &dep_version.version))?;

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

                if dep_parts.len() - 1 == true_parts.len() {
                    //println!("Skipping pointer creation for {}: {} (same level)", pkg_name, dep_name);
                    // If the dependency is at the same level as the package, skip pointer creation
                    continue;
                }


                // ./a/b/c/d/e

                // ./a/b/c/d1/e2 

                // -- 3 shared ancestors
                // (script.Parent).Parent.Parent['d']['e']
                let parent_count = true_parts.len() - shared_ancestors as usize;
                let lua_path = format!("script.Parent{}", (".Parent").repeat(parent_count + 1));
                // Create the relative path for the dependency
                
                let mut relative_path = dep_parts
                    .iter()
                    .skip(shared_ancestors as usize)
                    .map(|s| encode(s).into_owned())
                    .collect::<Vec<String>>()
                    .join("']['");


                relative_path = format!("{}['{}']", lua_path, relative_path);

                let mut init_lua = String::new();
                init_lua.push_str("--Pointer file generated by Forest Package Manager.\n");
                init_lua.push_str(&format!(
                    "return require({})",
                    relative_path
                ));

                //println!("Creating pointer file for {}: {}/{}", pkg_name, true_path, dep_name);

                let target_dir = Path::new(&true_path).join(dep_name);
                if !target_dir.exists() {
                    fs::create_dir_all(&target_dir)?;
                }
                fs::write(target_dir.join("init.lua"), init_lua)?;
            }
        }
    }

    Ok(())
}

/// Generate a lockfile JSON string given the forest manifest & message spinner.
pub async fn lockfile_gen(forest_json: &Value, msg: &mut Message) -> Result<LockFile> {
    let roots = normalize_forest_deps(forest_json);
    let platform: String = forest_json
        .get("platform")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing platform in forest.json"))?
        .to_string(); // clone the value so we don't hold a borrow

    msg.update("Resolving dependencies...");
    let lockfile_packages = get_lockfile_packages(roots.clone(), platform).await
        .context("Failed to resolve lockfile packages")?;

    let lockfile : LockFile = LockFile {
        file_version: 1,
        packages: lockfile_packages
    };

    make_directories(&lockfile, roots).await
        .context("Failed to create directories for lockfile packages")?;
    
    Ok(lockfile)
}