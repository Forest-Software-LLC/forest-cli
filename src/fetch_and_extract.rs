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
pub fn fetch_and_extract(url: &str, out_dir: &Path, bar: ProgressBar) -> Result<()> {
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
    let mut archive = Archive::new(decompressor);
    archive
        .unpack(out_dir)
        .with_context(|| format!("Failed to unpack archive into {}", out_dir.display()))?;

    // Ensure bar is complete
    bar.set_position(100);
    bar.finish();

    Ok(())
}
