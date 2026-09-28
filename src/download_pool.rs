//! Shared download tail for the platform install executors: signed-URL
//! prefetch for private packages, then a bounded worker pool that acquires
//! each tarball (cache or download, hash-verified) and hands it to the
//! platform's installer closure. Owns the progress bars, the byte counter,
//! and the drain-everything-before-the-first-error semantics, so the two
//! executors cannot drift apart.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use indicatif::{HumanBytes, ProgressBar, ProgressStyle};

use crate::cache::TarballCache;
use crate::fetch_and_extract::OnBytes;
use crate::install_report::{DeniedPackage, DenyReason};
use crate::lockfile_gen::{cdn_base, fetch_signed_url};

/// How many tarballs download (and signed URLs prefetch) at once. Bounded so
/// a large tree doesn't spawn hundreds of OS threads and TLS connections.
pub const DOWNLOAD_WORKERS: usize = 8;

/// One package to download and install. `extra` carries whatever the
/// platform's installer needs beyond the shared fields (Roblox: archive root
/// and container; UEFN: nothing).
pub struct DownloadJob<E> {
    pub name: String,
    pub version: String,
    pub integrity: String,
    /// Target dir; created before the workers start.
    pub dir: PathBuf,
    pub public: bool,
    pub extra: E,
}

/// Run every job through the pool. `install_one` does the platform half:
/// acquire+extract into job.dir (via its fetch_and_extract flavor) and write
/// the receipt; it runs on worker threads. Public tarballs are addressed by
/// their integrity hash on the CDN; private ones get a fresh signed URL
/// here, the first alone so a stale access token refreshes exactly once
/// through http.rs's 401 path, the rest concurrently. Cache-satisfied
/// private entries skip the round-trip entirely (the lockfile hash is the
/// trust anchor; cached bytes are re-verified on read).
///
/// A private package the registry refuses is skipped, not fatal. It's
/// recorded in install_report for main's error and returned so the executor
/// leaves its slot empty.
///
/// All jobs run to completion before the first error is reported, so every
/// bar is cleared and no partial state hides behind an early return.
pub async fn download_all<E: Send + 'static>(
    jobs: Vec<DownloadJob<E>>,
    platform: &str,
    install_one: impl Fn(&DownloadJob<E>, &str, OnBytes<'_>, Option<&TarballCache>) -> Result<()>
        + Send
        + Sync
        + 'static,
) -> Result<Vec<DeniedPackage>> {
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let tarball_cache = TarballCache::open_default();

    // Private tarballs sit behind the CDN worker's HMAC gate and their
    // signed URLs expire in minutes, so they are never stored in the
    // lockfile; fetch fresh ones now (integrity cross-check inside
    // fetch_signed_url). One request per version, even at several paths.
    let mut private_urls: HashMap<(String, String), String> = HashMap::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut private_entries: Vec<(String, String, String)> = jobs
        .iter()
        .filter(|j| !j.public)
        .filter(|j| tarball_cache.as_ref().map_or(true, |c| c.lookup(&j.integrity).is_none()))
        .filter(|j| seen.insert((j.name.clone(), j.version.clone())))
        .map(|j| (j.name.clone(), j.version.clone(), j.integrity.clone()))
        .collect();

    // Anonymous requests for private packages always 404, so don't ask.
    let mut denied: Vec<DeniedPackage> = Vec::new();
    if !private_entries.is_empty() && !crate::api_token::has_credential() {
        denied = private_entries
            .drain(..)
            .map(|(name, version, _)| DeniedPackage { name, version, reason: DenyReason::NotLoggedIn })
            .collect();
    }

    let mut private_iter = private_entries.into_iter();
    if let Some((pkg, ver, integrity)) = private_iter.next() {
        // These round-trips run with the install spinner paused; a counter
        // keeps the terminal alive while a tree of private packages authorizes.
        let auth_bar = ProgressBar::new((private_iter.len() + 1) as u64);
        auth_bar.set_style(
            ProgressStyle::with_template("{spinner:.green} Authorizing private packages {pos}/{len}")?
                .tick_strings(crate::message::TICK_STRINGS),
        );
        auth_bar.enable_steady_tick(std::time::Duration::from_millis(70));

        // Collected (not `?`-propagated) so the bar's line is cleared before
        // any error message prints under it.
        let platform_owned = platform.to_string();
        let prefetch: Result<()> = async {
            let first = fetch_signed_url(pkg, ver, integrity, platform_owned.clone()).await;
            file_signed_url(first, &mut private_urls, &mut denied)?;
            auth_bar.inc(1);

            let semaphore = Arc::new(tokio::sync::Semaphore::new(DOWNLOAD_WORKERS));
            let mut tasks = tokio::task::JoinSet::new();
            for (pkg, ver, integrity) in private_iter {
                let semaphore = Arc::clone(&semaphore);
                let platform = platform_owned.clone();
                tasks.spawn(async move {
                    let _permit = semaphore.acquire_owned().await.expect("semaphore closed");
                    fetch_signed_url(pkg, ver, integrity, platform).await
                });
            }
            while let Some(joined) = tasks.join_next().await {
                let result = joined.map_err(|e| anyhow!("Signed-URL task panicked: {e}"))?;
                file_signed_url(result, &mut private_urls, &mut denied)?;
                auth_bar.inc(1);
            }
            Ok(())
        }
        .await;
        auth_bar.finish_and_clear();
        prefetch?;
    }

    crate::install_report::record(&denied);
    let denied_keys: HashSet<(String, String)> =
        denied.iter().map(|d| (d.name.clone(), d.version.clone())).collect();
    let jobs: Vec<DownloadJob<E>> = jobs
        .into_iter()
        .filter(|j| !denied_keys.contains(&(j.name.clone(), j.version.clone())))
        .collect();
    if jobs.is_empty() {
        return Ok(denied);
    }
    for job in &jobs {
        if !job.dir.exists() {
            std::fs::create_dir_all(&job.dir)?;
        }
    }

    // One line for the whole phase: package count plus a downloaded-bytes
    // counter. Per-download bars rendered as empty rails on cache-heavy
    // installs; the counter still proves liveness while a big tarball holds
    // the count still, and stays absent when everything comes from cache.
    let total_bar = ProgressBar::new(jobs.len() as u64);
    total_bar.set_style(
        ProgressStyle::with_template("{spinner:.green} Installing packages {bar:30.cyan/blue} {pos}/{len} {msg}")?
            .progress_chars("=>-")
            .tick_strings(crate::message::TICK_STRINGS),
    );
    total_bar.enable_steady_tick(std::time::Duration::from_millis(70));
    // Byte counter shown in the bar message. A mutex, not an atomic: update
    // and display must be one critical section or out-of-order set_message
    // calls make the counter run backwards.
    let downloaded = Arc::new(Mutex::new(0u64));

    // Resolve each job's URL up front. Public tarballs are content-addressed
    // (the hash IS the path, so a lockfile can't point the CLI anywhere
    // else); cache-satisfied private entries have no signed URL, and the
    // sentinel only surfaces if the cache entry vanishes between the probe
    // and the worker, failing that download loudly.
    let jobs: Vec<(DownloadJob<E>, String)> = jobs
        .into_iter()
        .map(|job| {
            let url = if job.public {
                format!("{}/public/{}.tgz", cdn_base(), job.integrity.trim())
            } else {
                private_urls
                    .get(&(job.name.clone(), job.version.clone()))
                    .cloned()
                    .unwrap_or_else(|| format!("forest-cache://{}", job.integrity.trim()))
            };
            (job, url)
        })
        .collect();

    // Drain the queue with a small worker pool instead of one OS thread per
    // package. Workers keep draining after a failure so every bar is cleared
    // and all downloads run to completion before the FIRST error is reported.
    let n_workers = jobs.len().min(DOWNLOAD_WORKERS);
    let queue = Arc::new(Mutex::new(jobs));
    let first_err: Arc<Mutex<Option<anyhow::Error>>> = Arc::new(Mutex::new(None));
    let install_one = Arc::new(install_one);
    let mut workers = Vec::new();
    for _ in 0..n_workers {
        let queue = Arc::clone(&queue);
        let first_err = Arc::clone(&first_err);
        let tarball_cache = tarball_cache.clone();
        let total_bar = total_bar.clone();
        let downloaded = Arc::clone(&downloaded);
        let install_one = Arc::clone(&install_one);
        workers.push(std::thread::spawn(move || {
            // indicatif throttles redraws, so per-chunk set_message is cheap.
            let on_bytes = |delta: u64| {
                let mut total = downloaded.lock().expect("byte counter poisoned");
                *total += delta;
                total_bar.set_message(format!("{}", HumanBytes(*total)));
            };
            loop {
                let entry = queue.lock().expect("job queue poisoned").pop();
                let Some((job, url)) = entry else { break };
                let result = install_one(&job, &url, &on_bytes, tarball_cache.as_ref());
                total_bar.inc(1);
                if let Err(e) = result {
                    first_err.lock().expect("error slot poisoned").get_or_insert(e);
                }
            }
        }));
    }
    for handle in workers {
        if let Err(e) = handle.join() {
            let mut slot = first_err.lock().expect("error slot poisoned");
            if slot.is_none() {
                *slot = Some(anyhow!("Fetch thread panicked: {:?}", e));
            }
        }
    }
    total_bar.finish_and_clear();
    let pool_err = first_err.lock().expect("error slot poisoned").take();
    if let Some(e) = pool_err {
        return Err(e);
    }
    Ok(denied)
}

/// File one signed-URL result: the URL, a refusal to skip, or a hard error
/// (integrity mismatch, network, 5xx) that still aborts the install.
fn file_signed_url(
    result: Result<((String, String), String)>,
    urls: &mut HashMap<(String, String), String>,
    denied: &mut Vec<DeniedPackage>,
) -> Result<()> {
    match result {
        Ok((key, url)) => {
            urls.insert(key, url);
            Ok(())
        }
        Err(e) => {
            denied.push(e.downcast::<DeniedPackage>()?);
            Ok(())
        }
    }
}
