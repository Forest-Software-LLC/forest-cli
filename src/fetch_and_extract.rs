use std::fs;
use std::io::{self, Cursor, Read};
use std::path::Path;
use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use tar::Archive;
use indicatif::ProgressBar;

/// Reader wrapper that tracks progress and updates an indicatif ProgressBar.
struct ProgressReader<R> {
    inner: R,
    bar: ProgressBar,
    total: u64,
    transferred: u64,
}

impl<R: Read> ProgressReader<R> {
    fn new(inner: R, bar: ProgressBar, total: u64) -> Self {
        ProgressReader {
            inner,
            bar,
            total,
            transferred: 0,
        }
    }
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 && self.total > 0 {
            self.transferred += n as u64;
            let pct = (self.transferred as f64 / self.total as f64 * 100.0).round() as u64;
            self.bar.set_position(pct);
        }
        Ok(n)
    }
}

/// Download a .tgz from the given URL, verify its SHA-256 against the lockfile's
/// integrity hash, and extract it into `out_dir`, updating `bar`.
///
/// The archive is buffered in full (packages are capped at 10 MB by the registry)
/// so the hash is checked BEFORE any file is written — a tampered tarball must
/// never be partially extracted.
pub fn fetch_and_extract(url: &str, expected_sha256: &str, out_dir: &Path, archive_root: &str, bar: ProgressBar) -> Result<()> {
    if expected_sha256.trim().is_empty() {
        // An unaddressable entry, not a weaker check: without the hash there is
        // no trusted way to fetch this package.
        bail!("Lockfile entry has no integrity hash — delete forest-lock.json and re-run `forest install`");
    }

    // Ensure output directory exists
    fs::create_dir_all(out_dir)
        .with_context(|| format!("Failed to create directory {}", out_dir.display()))?;

    // Perform HTTP GET
    let client = Client::new();
    let resp = client
        .get(url)
        .send()
        .context("HTTP request failed")?
        .error_for_status()
        .context("Non-success HTTP status code")?;

    // Determine total size (if provided)
    let total = resp
        .content_length()
        .unwrap_or(0);

    // Wrap response reader with progress
    let mut reader = ProgressReader::new(resp, bar.clone(), total);

    let mut bytes = Vec::with_capacity(total as usize);
    reader
        .read_to_end(&mut bytes)
        .context("Failed to download archive")?;

    let actual = {
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    if !actual.eq_ignore_ascii_case(expected_sha256.trim()) {
        bail!(
            "Integrity check failed: expected sha256 {} but downloaded content hashes to {}. \
             The package may have been tampered with — nothing was extracted.",
            expected_sha256, actual
        );
    }

    let decompressor = GzDecoder::new(Cursor::new(bytes));

    // Extract tar archive
    let root_path = Path::new(archive_root).to_path_buf();

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

    // Ensure bar is complete
    bar.set_position(100);
    bar.finish();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    /// Build a package tgz containing the given (path, content) entries.
    fn make_tgz_with(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            for (path, content) in entries {
                let data = content.as_bytes();
                let mut header = tar::Header::new_gnu();
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append_data(&mut header, *path, data).unwrap();
            }
            builder.finish().unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes).unwrap();
        gz.finish().unwrap()
    }

    /// Build a minimal valid package tgz: src/init.luau with the given content.
    fn make_tgz(content: &str) -> Vec<u8> {
        make_tgz_with(&[("src/init.luau", content)])
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Serve `body` once over HTTP on an ephemeral port; returns the URL.
    fn serve_once(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Drain the request headers
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
        });
        format!("http://{}/public/test.tgz", addr)
    }

    fn temp_out_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("forest-test-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn extracts_when_integrity_matches() {
        let tgz = make_tgz("return {}");
        let hash = sha256_hex(&tgz);
        let url = serve_once(tgz);
        let out = temp_out_dir("ok");

        fetch_and_extract(&url, &hash, &out, "src/init.luau", ProgressBar::hidden()).unwrap();

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

        fetch_and_extract(&url, &hash, &out, "ProfileStore.luau", ProgressBar::hidden()).unwrap();

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

        fetch_and_extract(&url, &hash, &out, "src/Module.lua", ProgressBar::hidden()).unwrap();

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

        fetch_and_extract(&url, &hash, &out, "init.luau", ProgressBar::hidden()).unwrap();

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

        let err = fetch_and_extract(&url, &wrong_hash, &out, "src/init.luau", ProgressBar::hidden())
            .unwrap_err();

        assert!(err.to_string().contains("Integrity check failed"), "unexpected error: {err}");
        assert!(!out.join("init.luau").exists(), "tampered archive must not be extracted");
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn rejects_empty_integrity_before_downloading() {
        let out = temp_out_dir("empty");
        // URL is never contacted — an unaddressable entry must fail fast.
        let err = fetch_and_extract("http://127.0.0.1:1/never.tgz", "  ", &out, "src/init.luau", ProgressBar::hidden())
            .unwrap_err();
        assert!(err.to_string().contains("no integrity hash"), "unexpected error: {err}");
    }
}
