//! Roblox tarball extraction: Rojo folder-module semantics. The archive root
//! (the package's declared init file) picks the source directory, the root
//! file is renamed to `init.<ext>` so the installed folder is requirable,
//! and a top-level LICENSE is hoisted. Trusted-byte acquisition (cache /
//! download / hash gate) is shared core (src/fetch_and_extract.rs).

use std::fs;
use std::io::{self, Cursor};
use std::path::Path;
use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use indicatif::ProgressBar;
use tar::Archive;

use crate::cache::TarballCache;
use crate::fetch_and_extract::obtain_verified_bytes;

/// Install a package tarball into `out_dir`: serve it from the
/// content-addressed cache when possible, otherwise download from `url` and
/// warm the cache. Either way the SHA-256 is checked against the lockfile's
/// integrity hash BEFORE extraction.
pub fn fetch_and_extract(
    url: &str,
    expected_sha256: &str,
    out_dir: &Path,
    archive_root: &str,
    bar: ProgressBar,
    cache: Option<&TarballCache>,
) -> Result<()> {
    let bytes = obtain_verified_bytes(url, expected_sha256, out_dir, &bar, cache)?;
    extract_tgz(bytes, out_dir, archive_root)?;

    bar.finish();

    Ok(())
}

/// Unpack already-verified tgz bytes into `out_dir`, honoring `archive_root`.
fn extract_tgz(bytes: Vec<u8>, out_dir: &Path, archive_root: &str) -> Result<()> {
    let decompressor = GzDecoder::new(Cursor::new(bytes));

    // Tar entry paths are always forward-slashed, but versions published
    // from Windows before the gateway normalized `root` carry backslash
    // archiveRoots (e.g. `AnimNation\init.luau`) — on mac/linux that parses
    // as a single component and the prefix matching below never fires.
    let archive_root = archive_root.replace('\\', "/");
    let root_path = Path::new(&archive_root).to_path_buf();

    // `archive_root` is the package's init file (e.g. `src/init.luau`). In Roblox a
    // folder module is `init.luau` plus its sibling files/subfolders, so the real
    // source root is the DIRECTORY that contains the init file — we must extract
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

    for entry in entries {
        let mut entry = entry.context("Failed to read a tar entry")?;
        let header = entry.header().clone();
        let entry_type = header.entry_type();

        // Path inside the tar (forward slashes), convert to PathBuf
        let entry_path = entry.path().context("Invalid tar entry path")?;
        let entry_path = entry_path.to_path_buf();

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
                let mut out = fs::File::create(&dest_path)
                    .with_context(|| format!("Failed to create {}", dest_path.display()))?;
                io::copy(&mut entry, &mut out)
                    .with_context(|| format!("Failed to write {}", dest_path.display()))?;
            } else {
                // Skip symlinks and other types for safety
                continue;
            }
        }
    }

    Ok(())
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

        fetch_and_extract(&url, &hash, &out, "src/init.luau", ProgressBar::hidden(), None).unwrap();

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

        fetch_and_extract(&url, &hash, &out, "ProfileStore.luau", ProgressBar::hidden(), None).unwrap();

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

        fetch_and_extract(&url, &hash, &out, "src/Module.lua", ProgressBar::hidden(), None).unwrap();

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

        fetch_and_extract(&url, &hash, &out, "init.luau", ProgressBar::hidden(), None).unwrap();

        assert_eq!(fs::read_to_string(out.join("init.luau")).unwrap(), "return {} -- root");
        assert_eq!(fs::read_to_string(out.join("Event.luau")).unwrap(), "return {} -- event");
        assert_eq!(fs::read_to_string(out.join("Nested").join("Deep.luau")).unwrap(), "return {} -- deep");
        assert_eq!(fs::read_to_string(out.join("LICENSE")).unwrap(), "MIT");
        assert!(out.join("wally.toml").exists());
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn rejects_and_extracts_nothing_when_integrity_differs() {
        let tgz = make_tgz("return {} -- tampered");
        let wrong_hash = sha256_hex(b"something else entirely");
        let url = serve_once(tgz);
        let out = temp_out_dir("tampered");

        let err = fetch_and_extract(&url, &wrong_hash, &out, "src/init.luau", ProgressBar::hidden(), None)
            .unwrap_err();

        assert!(err.to_string().contains("Integrity check failed"), "unexpected error: {err}");
        assert!(!out.join("init.luau").exists(), "tampered archive must not be extracted");
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn rejects_empty_integrity_before_downloading() {
        let out = temp_out_dir("empty");
        // URL is never contacted - an unaddressable entry must fail fast.
        let err = fetch_and_extract("http://127.0.0.1:1/never.tgz", "  ", &out, "src/init.luau", ProgressBar::hidden(), None)
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
            ProgressBar::hidden(),
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

        fetch_and_extract(&url, &hash, &out, "src/init.luau", ProgressBar::hidden(), Some(&cache)).unwrap();

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

        fetch_and_extract(&url, &hash, &out, "src/init.luau", ProgressBar::hidden(), Some(&cache)).unwrap();

        assert_eq!(fs::read_to_string(out.join("init.luau")).unwrap(), "return {} -- fresh");
        // The rotten entry was replaced by the verified download.
        assert_eq!(cache.lookup(&hash).as_deref(), Some(tgz.as_slice()));
        let _ = fs::remove_dir_all(&out);
    }
}
