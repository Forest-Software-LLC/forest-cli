// use tokio::sync::mpsc::error; // Not needed for logging

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