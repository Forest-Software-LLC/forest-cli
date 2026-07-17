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
    let mut parts: Vec<&str> = name.split('/').collect();
    if parts.len() == 1 {
        panic!("Invalid package name format");
    }
    if parts[0].starts_with('@') {
        parts[0] = &parts[0][1..];
        return PackageName { name: parts[1].to_string(), scope: parts[0].to_string(), full_name: name.to_string() };
    }
    PackageName { name: parts[1].to_string(), scope: parts[0].to_string(), full_name: name.to_string() }
}

/// Case-insensitive HashMap lookup for package-name keys: Exact match wins; otherwise the first case-insensitive hit.
pub fn get_ci<'a, V>(map: &'a HashMap<String, V>, key: &str) -> Option<&'a V> {
    map.get(key).or_else(|| {
        map.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    })
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