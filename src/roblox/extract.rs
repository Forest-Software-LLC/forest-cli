//! Roblox tarball extraction: Rojo folder-module semantics. The archive root
//! (the package's declared init file) picks the source directory, the root
//! file is renamed to `init.<ext>` so the installed folder is requirable,
//! and a top-level LICENSE is hoisted. Trusted-byte acquisition (cache /
//! download / hash gate) is shared core (src/fetch_and_extract.rs).
//!
//! Runnable script sources (`*.server.lua(u)` / `*.client.lua(u)`, and
//! `.meta.json` files that set RunContext or a script className) install
//! as packaged and are reported back so the install can warn about them.

use std::fs;
use std::io::{self, Cursor, Read};
use std::path::Path;
use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use tar::Archive;

use crate::cache::TarballCache;
use crate::fetch_and_extract::{obtain_verified_bytes, OnBytes};

/// Suffixes Rojo syncs as Script/LocalScript instances.
const RUNNABLE_SUFFIXES: [&str; 4] = [
    ".server.lua",
    ".server.luau",
    ".client.lua",
    ".client.luau",
];

/// What extraction observed about the installed files.
#[derive(Debug, Default)]
pub struct ExtractReport {
    pub script_sources: Vec<String>,
}

/// A .meta.json that turns its instance into something runnable: an explicit
/// non-Legacy RunContext, or a Script/LocalScript className (init.meta.json
/// can class a whole folder).
fn meta_declares_script(bytes: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return false;
    };
    let run_context = value
        .get("properties")
        .and_then(|p| p.get("RunContext"))
        .and_then(|r| r.as_str());
    if matches!(run_context, Some(rc) if rc != "Legacy") {
        return true;
    }
    matches!(
        value.get("className").and_then(|c| c.as_str()),
        Some("Script") | Some("LocalScript")
    )
}

fn rel_display(dest: &Path, out_dir: &Path) -> String {
    dest.strip_prefix(out_dir)
        .unwrap_or(dest)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Install a package tarball into `out_dir`: serve it from the
/// content-addressed cache when possible, otherwise download from `url` and
/// warm the cache. Either way the SHA-256 is checked against the lockfile's
/// integrity hash BEFORE extraction.
pub fn fetch_and_extract(
    url: &str,
    expected_sha256: &str,
    out_dir: &Path,
    archive_root: &str,
    on_bytes: OnBytes<'_>,
    cache: Option<&TarballCache>,
) -> Result<ExtractReport> {
    let bytes = obtain_verified_bytes(url, expected_sha256, out_dir, on_bytes, cache)?;
    extract_tgz(bytes, out_dir, archive_root)
}

/// Unpack already-verified tgz bytes into `out_dir`, honoring `archive_root`.
fn extract_tgz(bytes: Vec<u8>, out_dir: &Path, archive_root: &str) -> Result<ExtractReport> {
    let decompressor = GzDecoder::new(Cursor::new(bytes));

    // Tar entry paths are always forward-slashed, but versions published
    // from Windows before the gateway normalized `root` carry backslash
    // archiveRoots (e.g. `AnimNation\init.luau`); on mac/linux that parses
    // as a single component and the prefix matching below never fires.
    let archive_root = archive_root.replace('\\', "/");
    let root_path = Path::new(&archive_root).to_path_buf();

    // `archive_root` is the package's init file (e.g. `src/init.luau`). In Roblox a
    // folder module is `init.luau` plus its sibling files/subfolders, so the real
    // source root is the DIRECTORY that contains the init file; we must extract
    // everything in it, not just the init file itself. A top-level root file (no
    // parent directory) means the archive root IS the source root (e.g. Wally
    // packages like ambergracesoftware/remote ship `init.luau` plus sibling
    // modules at top level), so the whole archive is extracted.
    let root_dir = root_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty());

    // Roblox can only `require` the package folder if its module file is named
    // `init.<ext>`, but packages may declare any file as their root (e.g.
    // `ProfileStore.luau`). Rename the root file on extraction so the installed
    // folder is always requirable.
    let renamed_init: Option<String> = root_path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|stem| *stem != "init")
        .map(|_| {
            let ext = root_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("luau");
            format!("init.{ext}")
        });

    let mut archive = Archive::new(decompressor);
    let entries = archive.entries().context("Failed to read archive entries")?;
    let mut report = ExtractReport::default();

    for entry in entries {
        let mut entry = entry.context("Failed to read a tar entry")?;
        let header = entry.header().clone();
        let entry_type = header.entry_type();

        // Path inside the tar (forward slashes), convert to PathBuf
        let entry_path = entry.path().context("Invalid tar entry path")?;
        let entry_path = entry_path.to_path_buf();

        // forest.json and Rojo project files are authoring metadata, not part
        // of the installed module. Rojo reads *.project.json files inside the
        // mounted tree and errors when their paths don't exist post-extraction.
        let is_authoring_metadata = entry_path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|name| name == "forest.json" || name.ends_with(".project.json"))
            .unwrap_or(false);
        if is_authoring_metadata {
            continue;
        }

        // Script/LocalScript sources install as packaged and get reported.
        // The declared root never counts: the init rename below makes it an
        // inert ModuleScript.
        let is_runnable_script = entry_path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|name| {
                RUNNABLE_SUFFIXES.iter().any(|suffix| name.ends_with(suffix))
            })
            .unwrap_or(false)
            && entry_path != root_path;

        // Detect a top-level LICENSE
        let is_top_level = entry_path.components().count() == 1;
        let is_license = is_top_level
            && entry_path
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s == "LICENSE")
                .unwrap_or(false);

        // Decide output destination or skip
        let dest: Option<std::path::PathBuf> = if is_license {
            Some(out_dir.join("LICENSE"))
        } else if let Some(dir) = root_dir {
            // Folder-module case → extract the whole source directory (init file plus
            // all sibling .lua/.luau files and nested folders), stripping its prefix.
            if entry_path.starts_with(dir) && entry_path.as_path() != dir {
                let rel = entry_path
                    .strip_prefix(dir)
                    .expect("strip_prefix must succeed");
                // prevent traversal in the relative path
                let has_traversal = rel.components().any(|c| matches!(c,
                    std::path::Component::ParentDir | std::path::Component::RootDir
                ));
                if has_traversal {
                    None
                } else if entry_path == root_path {
                    match &renamed_init {
                        Some(name) => Some(out_dir.join(name)),
                        None => Some(out_dir.join(rel)),
                    }
                } else {
                    Some(out_dir.join(rel))
                }
            } else {
                None
            }
        } else {
            // Top-level root file → the archive root is the source root: extract
            // every entry (root plus sibling files/subfolders) under out_dir.
            let has_traversal = entry_path.components().any(|c| matches!(c,
                std::path::Component::ParentDir | std::path::Component::RootDir
            ));
            if has_traversal {
                None
            } else if entry_path == root_path {
                match &renamed_init {
                    Some(name) => Some(out_dir.join(name)),
                    None => Some(out_dir.join(&entry_path)),
                }
            } else {
                Some(out_dir.join(&entry_path))
            }
        };

        if let Some(dest_path) = dest {
            if entry_type.is_dir() {
                fs::create_dir_all(&dest_path)
                    .with_context(|| format!("Failed to create dir {}", dest_path.display()))?;
            } else if entry_type.is_file() {
                if let Some(parent) = dest_path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("Failed to create parent dir {}", parent.display()))?;
                }
                let is_meta = dest_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .map(|name| name.ends_with(".meta.json"))
                    .unwrap_or(false);
                if is_meta {
                    // Buffered so the content can be inspected; meta files
                    // are tiny.
                    let mut buf = Vec::new();
                    entry.read_to_end(&mut buf)
                        .with_context(|| format!("Failed to read {}", dest_path.display()))?;
                    fs::write(&dest_path, &buf)
                        .with_context(|| format!("Failed to write {}", dest_path.display()))?;
                    if meta_declares_script(&buf) {
                        report.script_sources.push(rel_display(&dest_path, out_dir));
                    }
                } else {
                    let mut out = fs::File::create(&dest_path)
                        .with_context(|| format!("Failed to create {}", dest_path.display()))?;
                    io::copy(&mut entry, &mut out)
                        .with_context(|| format!("Failed to write {}", dest_path.display()))?;
                }
                if is_runnable_script {
                    report.script_sources.push(rel_display(&dest_path, out_dir));
                }
            } else {
                // Skip symlinks and other types for safety
                continue;
            }
        }
    }

    report.script_sources.sort();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch_and_extract::test_util::{make_tgz, make_tgz_with, serve_once, temp_out_dir};
    use crate::utils::sha256_hex;

    #[test]
    fn extracts_when_integrity_matches() {
        let tgz = make_tgz("return {}");
        let hash = sha256_hex(&tgz);
        let url = serve_once(tgz);
        let out = temp_out_dir("ok");

        fetch_and_extract(&url, &hash, &out, "src/init.luau", &|_| {}, None).unwrap();

        let extracted = fs::read_to_string(out.join("init.luau")).unwrap();
        assert_eq!(extracted, "return {}");
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn renames_top_level_root_file_to_init() {
        // Single-file package whose root isn't named init (e.g. ProfileStore).
        let tgz = make_tgz_with(&[("ProfileStore.luau", "return {} -- ps")]);
        let hash = sha256_hex(&tgz);
        let url = serve_once(tgz);
        let out = temp_out_dir("rename-top");

        fetch_and_extract(&url, &hash, &out, "ProfileStore.luau", &|_| {}, None).unwrap();

        let extracted = fs::read_to_string(out.join("init.luau")).unwrap();
        assert_eq!(extracted, "return {} -- ps");
        assert!(!out.join("ProfileStore.luau").exists(), "root file must be renamed, not duplicated");
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn renames_nested_root_file_to_init_keeping_siblings() {
        let tgz = make_tgz_with(&[
            ("src/Module.lua", "return {} -- root"),
            ("src/Helper.lua", "return {} -- helper"),
        ]);
        let hash = sha256_hex(&tgz);
        let url = serve_once(tgz);
        let out = temp_out_dir("rename-nested");

        fetch_and_extract(&url, &hash, &out, "src/Module.lua", &|_| {}, None).unwrap();

        // Root renamed with its extension preserved; siblings keep their names.
        assert_eq!(fs::read_to_string(out.join("init.lua")).unwrap(), "return {} -- root");
        assert_eq!(fs::read_to_string(out.join("Helper.lua")).unwrap(), "return {} -- helper");
        assert!(!out.join("Module.lua").exists(), "root file must be renamed, not duplicated");
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn extracts_siblings_of_top_level_init() {
        // Wally-style layout: init.luau plus sibling modules at the archive root
        // (e.g. ambergracesoftware/remote).
        let tgz = make_tgz_with(&[
            ("init.luau", "return {} -- root"),
            ("Event.luau", "return {} -- event"),
            ("Nested/Deep.luau", "return {} -- deep"),
            ("LICENSE", "MIT"),
            ("wally.toml", "[package]"),
        ]);
        let hash = sha256_hex(&tgz);
        let url = serve_once(tgz);
        let out = temp_out_dir("top-level-siblings");

        fetch_and_extract(&url, &hash, &out, "init.luau", &|_| {}, None).unwrap();

        assert_eq!(fs::read_to_string(out.join("init.luau")).unwrap(), "return {} -- root");
        assert_eq!(fs::read_to_string(out.join("Event.luau")).unwrap(), "return {} -- event");
        assert_eq!(fs::read_to_string(out.join("Nested").join("Deep.luau")).unwrap(), "return {} -- deep");
        assert_eq!(fs::read_to_string(out.join("LICENSE")).unwrap(), "MIT");
        assert!(out.join("wally.toml").exists());
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn drops_manifest_and_rojo_project_files() {
        // Rojo reads any *.project.json in the mounted tree and errors on
        // paths that don't exist after extraction; forest.json is authoring
        // metadata. Neither belongs in the installed package.
        let tgz = make_tgz_with(&[
            ("init.luau", "return {} -- root"),
            ("forest.json", "{}"),
            ("default.project.json", "{}"),
            ("Nested/dev.project.json", "{}"),
            ("Nested/Deep.luau", "return {} -- deep"),
        ]);
        let hash = sha256_hex(&tgz);
        let url = serve_once(tgz);
        let out = temp_out_dir("drop-metadata");

        fetch_and_extract(&url, &hash, &out, "init.luau", &|_| {}, None).unwrap();

        assert!(out.join("init.luau").exists());
        assert!(out.join("Nested").join("Deep.luau").exists());
        assert!(!out.join("forest.json").exists());
        assert!(!out.join("default.project.json").exists());
        assert!(!out.join("Nested").join("dev.project.json").exists());
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn drops_metadata_inside_a_folder_module_root() {
        let tgz = make_tgz_with(&[
            ("forest.json", "{}"),
            ("src/init.luau", "return {} -- root"),
            ("src/forest.json", "{}"),
            ("src/default.project.json", "{}"),
        ]);
        let hash = sha256_hex(&tgz);
        let url = serve_once(tgz);
        let out = temp_out_dir("drop-metadata-nested");

        fetch_and_extract(&url, &hash, &out, "src/init.luau", &|_| {}, None).unwrap();

        assert!(out.join("init.luau").exists());
        assert!(!out.join("forest.json").exists());
        assert!(!out.join("default.project.json").exists());
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn installs_and_reports_runnable_scripts() {
        // Script/LocalScript sources install as packaged (SmartBone clones
        // its Runtime template into Actors) and are reported so the install
        // can warn about them.
        let tgz = make_tgz_with(&[
            ("src/init.luau", "return {} -- root"),
            ("src/Helper.luau", "return {} -- helper"),
            ("src/Runtime.client.luau", "print('runtime')"),
            ("src/Nested/init.server.luau", "print('server folder')"),
            ("src/Nested/Deep.luau", "return {} -- deep"),
        ]);
        let hash = sha256_hex(&tgz);
        let url = serve_once(tgz);
        let out = temp_out_dir("report-scripts");

        let report = fetch_and_extract(&url, &hash, &out, "src/init.luau", &|_| {}, None).unwrap();

        assert!(out.join("init.luau").exists());
        assert!(out.join("Helper.luau").exists());
        assert_eq!(fs::read_to_string(out.join("Runtime.client.luau")).unwrap(), "print('runtime')");
        assert!(out.join("Nested").join("init.server.luau").exists(),
            "an init.server file classes the whole folder as a Script and must survive");
        assert_eq!(report.script_sources, vec![
            "Nested/init.server.luau".to_string(),
            "Runtime.client.luau".to_string(),
        ]);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn reports_scripts_in_top_level_root_layout() {
        let tgz = make_tgz_with(&[
            ("init.luau", "return {} -- root"),
            ("Runner.server.luau", "print('runner')"),
        ]);
        let hash = sha256_hex(&tgz);
        let url = serve_once(tgz);
        let out = temp_out_dir("report-top-level");

        let report = fetch_and_extract(&url, &hash, &out, "init.luau", &|_| {}, None).unwrap();

        assert!(out.join("init.luau").exists());
        assert!(out.join("Runner.server.luau").exists());
        assert_eq!(report.script_sources, vec!["Runner.server.luau".to_string()]);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn root_declared_as_server_script_becomes_module_init() {
        // The init rename makes the root an inert ModuleScript, so it is
        // neither installed as a script nor reported as one.
        let tgz = make_tgz_with(&[("Main.server.luau", "return {} -- root")]);
        let hash = sha256_hex(&tgz);
        let url = serve_once(tgz);
        let out = temp_out_dir("root-exempt");

        let report = fetch_and_extract(&url, &hash, &out, "Main.server.luau", &|_| {}, None).unwrap();

        assert_eq!(fs::read_to_string(out.join("init.luau")).unwrap(), "return {} -- root");
        assert!(!out.join("Main.server.luau").exists());
        assert!(report.script_sources.is_empty());
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn reports_meta_json_that_makes_an_instance_runnable() {
        let tgz = make_tgz_with(&[
            ("src/init.luau", "return {} -- root"),
            // A non-Legacy RunContext runs wherever it is mounted.
            ("src/Worker.luau", "return {}"),
            ("src/Worker.meta.json", r#"{"properties":{"RunContext":"Client"}}"#),
            // Explicit Legacy is the inert default, not worth a warning.
            ("src/Calm.luau", "return {}"),
            ("src/Calm.meta.json", r#"{"properties":{"RunContext":"Legacy"}}"#),
            // init.meta.json can class a whole folder as a script.
            ("src/Runner/init.meta.json", r#"{"className":"LocalScript"}"#),
            // Unrelated meta content stays quiet.
            ("src/Quiet.meta.json", r#"{"ignoreUnknownInstances":true}"#),
        ]);
        let hash = sha256_hex(&tgz);
        let url = serve_once(tgz);
        let out = temp_out_dir("report-meta");

        let report = fetch_and_extract(&url, &hash, &out, "src/init.luau", &|_| {}, None).unwrap();

        assert!(out.join("Worker.meta.json").exists());
        assert!(out.join("Quiet.meta.json").exists());
        assert_eq!(report.script_sources, vec![
            "Runner/init.meta.json".to_string(),
            "Worker.meta.json".to_string(),
        ]);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn rejects_and_extracts_nothing_when_integrity_differs() {
        let tgz = make_tgz("return {} -- tampered");
        let wrong_hash = sha256_hex(b"something else entirely");
        let url = serve_once(tgz);
        let out = temp_out_dir("tampered");

        let err = fetch_and_extract(&url, &wrong_hash, &out, "src/init.luau", &|_| {}, None)
            .unwrap_err();

        assert!(err.to_string().contains("Integrity check failed"), "unexpected error: {err}");
        assert!(!out.join("init.luau").exists(), "tampered archive must not be extracted");
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn rejects_empty_integrity_before_downloading() {
        let out = temp_out_dir("empty");
        // URL is never contacted - an unaddressable entry must fail fast.
        let err = fetch_and_extract("http://127.0.0.1:1/never.tgz", "  ", &out, "src/init.luau", &|_| {}, None)
            .unwrap_err();
        assert!(err.to_string().contains("no integrity hash"), "unexpected error: {err}");
    }

    #[test]
    fn cache_hit_extracts_without_network() {
        let tgz = make_tgz("return {} -- from cache");
        let hash = sha256_hex(&tgz);
        let cache = TarballCache::open_at(temp_out_dir("cache-hit-store")).unwrap();
        cache.store(&hash, &tgz);
        let out = temp_out_dir("cache-hit-out");

        // Port 1 is never listening: success proves the bytes came from the
        // cache, not the network.
        fetch_and_extract(
            "http://127.0.0.1:1/never.tgz",
            &hash,
            &out,
            "src/init.luau",
            &|_| {},
            Some(&cache),
        )
        .unwrap();

        assert_eq!(fs::read_to_string(out.join("init.luau")).unwrap(), "return {} -- from cache");
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn download_populates_cache_for_next_time() {
        let tgz = make_tgz("return {} -- warm me");
        let hash = sha256_hex(&tgz);
        let url = serve_once(tgz.clone());
        let cache = TarballCache::open_at(temp_out_dir("cache-warm")).unwrap();
        let out = temp_out_dir("cache-warm-out");

        fetch_and_extract(&url, &hash, &out, "src/init.luau", &|_| {}, Some(&cache)).unwrap();

        assert_eq!(cache.lookup(&hash).as_deref(), Some(tgz.as_slice()));
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn corrupt_cache_entry_falls_back_to_network_and_heals() {
        let tgz = make_tgz("return {} -- fresh");
        let hash = sha256_hex(&tgz);
        let url = serve_once(tgz.clone());
        let cache_dir = temp_out_dir("cache-heal");
        let cache = TarballCache::open_at(cache_dir.clone()).unwrap();
        // Plant garbage under the right entry name, as if the file rotted.
        fs::write(cache_dir.join(format!("{hash}.tgz")), b"garbage").unwrap();
        let out = temp_out_dir("cache-heal-out");

        fetch_and_extract(&url, &hash, &out, "src/init.luau", &|_| {}, Some(&cache)).unwrap();

        assert_eq!(fs::read_to_string(out.join("init.luau")).unwrap(), "return {} -- fresh");
        // The rotten entry was replaced by the verified download.
        assert_eq!(cache.lookup(&hash).as_deref(), Some(tgz.as_slice()));
        let _ = fs::remove_dir_all(&out);
    }
}
