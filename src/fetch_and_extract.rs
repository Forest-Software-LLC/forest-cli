use std::fs;
use std::io::{self, Read};
use std::path::Path;
use anyhow::{Context, Result};
use reqwest::blocking::Client;
use flate2::read::GzDecoder;
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

/// Download a .tgz from the given URL and extract it into `out_dir`, updating `bar`.
pub fn fetch_and_extract(url: &str, out_dir: &Path, archive_root: &str, bar: ProgressBar) -> Result<()> {
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
    let reader = ProgressReader::new(resp, bar.clone(), total);
    let decompressor = GzDecoder::new(reader);

    // Extract tar archive
    let root_path = Path::new(archive_root).to_path_buf();

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

        // Match either exact-file or under-directory semantics
        let matches_file = entry_path == root_path;
        let under_root_dir = entry_path.starts_with(&root_path) && entry_path != root_path;

        // Decide output destination or skip
        let dest: Option<std::path::PathBuf> = if is_license {
            Some(out_dir.join("LICENSE"))
        } else if matches_file {
            // Single-file case → place directly under out_dir with its basename
            entry_path
                .file_name()
                .map(|name| out_dir.join(name))
        } else if under_root_dir {
            // Directory subtree case → strip the prefix
            let rel = entry_path
                .strip_prefix(&root_path)
                .expect("strip_prefix must succeed");
            // prevent traversal in the relative path
            let has_traversal = rel.components().any(|c| matches!(c,
                std::path::Component::ParentDir | std::path::Component::RootDir
            ));
            if has_traversal {
                None
            } else {
                Some(out_dir.join(rel))
            }
        } else {
            None
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
