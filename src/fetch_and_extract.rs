//! Platform-blind tarball handling: trusted-byte acquisition (cache /
//! download / SHA-256 gate) and verbatim extraction. Platform-opinionated
//! extraction (the Roblox init-rename folder-module rules) lives in
//! roblox/extract.rs on top of `obtain_verified_bytes`.

use std::fs;
use std::io::{self, Cursor, Read};
use std::path::Path;
use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use tar::Archive;
use indicatif::ProgressBar;

use crate::cache::TarballCache;
use crate::utils::sha256_hex;

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

/// Verbatim variant: same trusted acquisition (cache/download + hash gate),
/// but the archive is unpacked exactly as archived. No init rename, no
/// archive-root prefix stripping, no LICENSE hoisting; the traversal and
/// symlink guards stay. Used by UEFN, where the folder IS the package.
pub fn fetch_and_extract_verbatim(
    url: &str,
    expected_sha256: &str,
    out_dir: &Path,
    bar: ProgressBar,
    cache: Option<&TarballCache>,
) -> Result<()> {
    let bytes = obtain_verified_bytes(url, expected_sha256, out_dir, &bar, cache)?;
    extract_tgz_verbatim(bytes, out_dir)?;

    bar.set_position(100);
    bar.finish();

    Ok(())
}

/// Shared acquisition path: cache lookup (already integrity-checked by
/// `lookup`'s re-hash) or download + verify + cache-warm. Bytes never reach
/// an extractor without hashing to the lockfile's integrity value.
///
/// The archive is buffered in full (packages are capped at 10 MB by the
/// registry) so the hash is checked BEFORE any file is written; a tampered
/// tarball must never be partially extracted.
pub(crate) fn obtain_verified_bytes(
    url: &str,
    expected_sha256: &str,
    out_dir: &Path,
    bar: &ProgressBar,
    cache: Option<&TarballCache>,
) -> Result<Vec<u8>> {
    if expected_sha256.trim().is_empty() {
        // An unaddressable entry, not a weaker check: without the hash there is
        // no trusted way to fetch this package.
        bail!("Lockfile entry has no integrity hash — delete forest-lock.json and re-run `forest install`");
    }

    // Ensure output directory exists
    fs::create_dir_all(out_dir)
        .with_context(|| format!("Failed to create directory {}", out_dir.display()))?;

    match cache.and_then(|c| c.lookup(expected_sha256)) {
        Some(bytes) => Ok(bytes),
        None => {
            let bytes = download_bytes(url, bar)?;
            verify_integrity(&bytes, expected_sha256)?;
            if let Some(c) = cache {
                c.store(expected_sha256, &bytes);
            }
            Ok(bytes)
        }
    }
}

/// HTTP GET the tarball, updating `bar` as bytes arrive.
fn download_bytes(url: &str, bar: &ProgressBar) -> Result<Vec<u8>> {
    // Shared client: connection reuse across the download workers.
    let client = crate::http::blocking_client();
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
    Ok(bytes)
}

/// The security gate: bytes may not reach the extractor unless they hash to
/// the lockfile's integrity value.
fn verify_integrity(bytes: &[u8], expected_sha256: &str) -> Result<()> {
    let actual = sha256_hex(bytes);
    if !actual.eq_ignore_ascii_case(expected_sha256.trim()) {
        bail!(
            "Integrity check failed: expected sha256 {} but downloaded content hashes to {}. \
             The package may have been tampered with — nothing was extracted.",
            expected_sha256, actual
        );
    }
    Ok(())
}

/// Unpack already-verified tgz bytes into `out_dir` exactly as archived.
/// Entries with traversal components are skipped; symlinks and other
/// non-file/non-dir types are skipped for safety.
fn extract_tgz_verbatim(bytes: Vec<u8>, out_dir: &Path) -> Result<()> {
    let decompressor = GzDecoder::new(Cursor::new(bytes));
    let mut archive = Archive::new(decompressor);
    let entries = archive.entries().context("Failed to read archive entries")?;

    for entry in entries {
        let mut entry = entry.context("Failed to read a tar entry")?;
        let entry_type = entry.header().entry_type();
        let entry_path = entry.path().context("Invalid tar entry path")?.to_path_buf();

        let has_traversal = entry_path.components().any(|c| {
            matches!(c, std::path::Component::ParentDir | std::path::Component::RootDir)
        });
        if has_traversal {
            continue;
        }
        let dest_path = out_dir.join(&entry_path);
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
            continue;
        }
    }

    Ok(())
}

/// Test fixtures shared by this module's tests and roblox/extract.rs's.
#[cfg(test)]
pub(crate) mod test_util {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Build a package tgz containing the given (path, content) entries.
    pub(crate) fn make_tgz_with(entries: &[(&str, &str)]) -> Vec<u8> {
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

    /// Build a minimal valid Roblox package tgz: src/init.luau with the given content.
    pub(crate) fn make_tgz(content: &str) -> Vec<u8> {
        make_tgz_with(&[("src/init.luau", content)])
    }

    /// Like make_tgz_with, but writes the entry name bytes directly into the
    /// header so traversal paths bypass tar::Builder's own validation — the
    /// shape a malicious archive would actually take.
    pub(crate) fn make_tgz_with_raw_names(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            for (path, content) in entries {
                let data = content.as_bytes();
                let mut header = tar::Header::new_gnu();
                {
                    let gnu = header.as_gnu_mut().expect("gnu header");
                    gnu.name[..path.len()].copy_from_slice(path.as_bytes());
                }
                header.set_size(data.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder.append(&header, data as &[u8]).unwrap();
            }
            builder.finish().unwrap();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes).unwrap();
        gz.finish().unwrap()
    }

    /// Serve `body` once over HTTP on an ephemeral port; returns the URL.
    pub(crate) fn serve_once(body: Vec<u8>) -> String {
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

    pub(crate) fn temp_out_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("forest-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::*;
    use super::*;

    #[test]
    fn verbatim_keeps_every_path_as_archived() {
        // The exact inverse of the Roblox rename behavior: nothing moves.
        let tgz = make_tgz_with(&[
            ("Calc.verse", "Double<public>(X:int):int = X + X"),
            ("forest.json", "{\"name\":\"Calc\"}"),
            ("Sub/Extra.verse", "Helper():int = 1"),
            ("README.md", "# Calc"),
            ("LICENSE", "MIT"),
        ]);
        let hash = sha256_hex(&tgz);
        let url = serve_once(tgz);
        let out = temp_out_dir("verbatim");

        fetch_and_extract_verbatim(&url, &hash, &out, ProgressBar::hidden(), None).unwrap();

        assert_eq!(fs::read_to_string(out.join("Calc.verse")).unwrap(), "Double<public>(X:int):int = X + X");
        assert!(out.join("forest.json").exists());
        assert!(out.join("Sub").join("Extra.verse").exists());
        assert!(out.join("README.md").exists());
        assert_eq!(fs::read_to_string(out.join("LICENSE")).unwrap(), "MIT");
        assert!(!out.join("init.luau").exists(), "verbatim must never invent init files");
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn verbatim_skips_traversal_entries() {
        let tgz = make_tgz_with_raw_names(&[
            ("ok.verse", "F():int = 1"),
            ("../evil.verse", "G():int = 2"),
        ]);
        let hash = sha256_hex(&tgz);
        let url = serve_once(tgz);
        let out = temp_out_dir("verbatim-traversal");

        fetch_and_extract_verbatim(&url, &hash, &out, ProgressBar::hidden(), None).unwrap();

        assert!(out.join("ok.verse").exists());
        assert!(!out.parent().unwrap().join("evil.verse").exists(), "traversal entry must be skipped");
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn verbatim_rejects_tampered_bytes_before_extraction() {
        let tgz = make_tgz_with(&[("X.verse", "F():int = 1")]);
        let wrong_hash = sha256_hex(b"other bytes");
        let url = serve_once(tgz);
        let out = temp_out_dir("verbatim-tampered");

        let err = fetch_and_extract_verbatim(&url, &wrong_hash, &out, ProgressBar::hidden(), None).unwrap_err();
        assert!(err.to_string().contains("Integrity check failed"));
        assert!(!out.join("X.verse").exists());
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn verbatim_cache_hit_extracts_without_network() {
        let tgz = make_tgz_with(&[("Y.verse", "F():int = 2")]);
        let hash = sha256_hex(&tgz);
        let cache = TarballCache::open_at(temp_out_dir("verbatim-cache")).unwrap();
        cache.store(&hash, &tgz);
        let out = temp_out_dir("verbatim-cache-out");

        fetch_and_extract_verbatim("http://127.0.0.1:1/never.tgz", &hash, &out, ProgressBar::hidden(), Some(&cache)).unwrap();
        assert!(out.join("Y.verse").exists());
        let _ = fs::remove_dir_all(&out);
    }
}
