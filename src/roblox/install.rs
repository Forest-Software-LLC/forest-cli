//! Roblox install executor: the hoisted `Packages/` tree with pointer
//! `init.lua` shims. Moved verbatim from lockfile_gen.rs when the platform
//! seam was introduced; reached only via `Platform::install`.
//!
//! NOTE: the download worker pool below is mirrored in uefn/install.rs
//! (search: DOWNLOAD_WORKERS). Fixes to either pool likely apply to both.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use crate::cache::TarballCache;
use crate::lockfile_gen::{cdn_base, fetch_signed_url, InstallSummary, LockFile, DOWNLOAD_WORKERS};
use crate::lockfile_solver::DepSpec;
use crate::receipts;
use crate::roblox::extract::fetch_and_extract;
use crate::roblox::plan::plan_install;
use crate::roblox::PACKAGES_DIR;

/// One tarball to download + extract, queued to the bounded worker pool.
/// No bar here: the worker that picks the job up creates its progress line.
struct DownloadJob {
    url: String,
    name: String,
    version: String,
    integrity: String,
    dir: PathBuf,
    root: String,
}

pub async fn make_directories_roblox(
    lockfile: &LockFile,
    root_deps: HashMap<String, DepSpec>,
    force: bool,
) -> Result<InstallSummary> {
    // `_`/`.`-prefixed folders in packages/ are exempt from install cleanup
    // (e.g. Wally's `_Index`), so aliases must not claim those names.
    for (pkg_name, spec) in &root_deps {
        if spec.alias.starts_with('_') || spec.alias.starts_with('.') {
            return Err(anyhow!(
                "Alias '{}' for {} cannot start with '_' or '.'; rename it in forest.json",
                spec.alias, pkg_name
            ));
        }
    }

    // All path/pointer computation is pure and lives in roblox/plan.rs.
    let plan = plan_install(lockfile, &root_deps)?;

    if !Path::new(PACKAGES_DIR).exists() {
        fs::create_dir_all(PACKAGES_DIR)?;
    }

    // The tree describes itself: every installed dir carries a
    // `.forest-receipt` (written after its extraction succeeded) and pointer
    // dirs are recognized by their generated header — no bookkeeping outside
    // Packages/. `--force` simply refuses to trust any of it, and a tree from
    // an older forest (no receipts) reinstalls everything the same way.
    let tree = if force {
        crate::roblox::receipts::TreeScan::default()
    } else {
        crate::roblox::receipts::scan(Path::new(PACKAGES_DIR))
    };
    let rec = crate::roblox::receipts::reconcile(&plan, &tree);
    let (to_install, kept, stale_dirs) = (rec.to_install, rec.kept, rec.stale_dirs);

    // Stale dirs go FIRST: on case-insensitive filesystems (Windows/macOS) a
    // renamed alias's old dir would otherwise delete the freshly extracted
    // new one. exists() guard: children of already-deleted parents are gone.
    for dir in &stale_dirs {
        let p = Path::new(dir);
        if p.exists() {
            fs::remove_dir_all(p).with_context(|| format!("Failed to remove stale {}", dir))?;
        }
    }

    // The top level of Packages/ stays fully managed: any non-exempt dir that
    // isn't a desired root alias is junk or a pre-receipt leftover. (This is
    // also what clears old trees on --force and first-run-after-upgrade.)
    prune_top_level(&plan)?;

    // A reinstall target may hold old content (integrity/root changed) —
    // clear it before extraction.
    for &i in &to_install {
        let target = PathBuf::from(&plan.packages[i].path);
        if target.exists() {
            fs::remove_dir_all(&target)
                .with_context(|| format!("Failed to clear {}", plan.packages[i].path))?;
        }
    }

    let tarball_cache = TarballCache::open_default();

    // Private tarballs sit behind the CDN worker's HMAC gate and their signed
    // URLs expire in minutes, so they are never stored in the lockfile. Fetch a
    // fresh signed URL per private entry now (integrity cross-check inside
    // fetch_signed_url). The first request runs alone so a stale access token
    // refreshes exactly once through http.rs's 401 path — N concurrent
    // requests would race N refreshes against a rotating refresh token — then
    // the rest fetch concurrently, bounded like the downloads.
    // Only entries that actually download need a URL: kept packages make no
    // gateway calls at all, and cache-satisfied ones skip the round-trip too
    // (the lockfile hash is the trust anchor; cached bytes are re-verified
    // against it on read).
    let mut private_urls: HashMap<(String, String), String> = HashMap::new();
    let private_entries: Vec<(String, String, String)> = to_install.iter()
        .map(|&i| &plan.packages[i])
        .filter(|p| !p.public)
        .filter(|p| tarball_cache.as_ref().map_or(true, |c| c.lookup(&p.integrity).is_none()))
        .map(|p| (p.name.clone(), p.version.clone(), p.integrity.clone()))
        .collect();
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
        let prefetch: Result<()> = async {
            let (key, url) = fetch_signed_url(pkg, ver, integrity, "roblox".to_string()).await?;
            private_urls.insert(key, url);
            auth_bar.inc(1);

            let semaphore = Arc::new(tokio::sync::Semaphore::new(DOWNLOAD_WORKERS));
            let mut tasks = tokio::task::JoinSet::new();
            for (pkg, ver, integrity) in private_iter {
                let semaphore = Arc::clone(&semaphore);
                tasks.spawn(async move {
                    let _permit = semaphore.acquire_owned().await.expect("semaphore closed");
                    fetch_signed_url(pkg, ver, integrity, "roblox".to_string()).await
                });
            }
            while let Some(joined) = tasks.join_next().await {
                let (key, url) = joined.map_err(|e| anyhow!("Signed-URL task panicked: {e}"))??;
                private_urls.insert(key, url);
                auth_bar.inc(1);
            }
            Ok(())
        }.await;
        auth_bar.finish_and_clear();
        prefetch?;
    }

    // Download + extract only what reconciliation says is missing or changed.
    // The no-op path skips the MultiProgress entirely (no bar flash).
    if !to_install.is_empty() {
        // Overall bar on top; each worker adds a byte-accurate line below it
        // for the download it is actively running (created on pick-up,
        // cleared on completion) — a big tree shows at most DOWNLOAD_WORKERS
        // in-flight lines instead of one mostly-idle bar per package.
        let mp = MultiProgress::new();
        let total_bar = mp.add(ProgressBar::new(to_install.len() as u64));
        total_bar.set_style(
            ProgressStyle::with_template("{spinner:.green} Installing packages {bar:30.cyan/blue} {pos}/{len}")?
                .progress_chars("=> ")
                .tick_strings(crate::message::TICK_STRINGS),
        );
        total_bar.enable_steady_tick(std::time::Duration::from_millis(70));
        let job_style = ProgressStyle::with_template("  {bar:30.cyan/blue} {bytes:>10} {wide_msg}")?
            .progress_chars("=> ");

        let mut jobs: Vec<DownloadJob> = Vec::new();
        for &i in &to_install {
            let pkg = &plan.packages[i];
            let dir_path = PathBuf::from(&pkg.path);
            if !dir_path.exists() {
                fs::create_dir_all(&dir_path)?;
            }

            // Public tarballs are content-addressed: the integrity hash IS the
            // path, so a lockfile can't point the CLI anywhere else.
            let url = if pkg.public {
                format!("{}/public/{}.tgz", cdn_base(), pkg.integrity.trim())
            } else {
                // Cache-satisfied private entries have no signed URL; the
                // sentinel only surfaces if the entry vanishes between the
                // probe and the worker, failing that download loudly.
                private_urls
                    .get(&(pkg.name.clone(), pkg.version.clone()))
                    .cloned()
                    .unwrap_or_else(|| format!("forest-cache://{}", pkg.integrity.trim()))
            };
            jobs.push(DownloadJob {
                url,
                name: pkg.name.clone(),
                version: pkg.version.clone(),
                integrity: pkg.integrity.clone(),
                dir: dir_path,
                root: pkg.root.clone(),
            });
        }

        // Drain the queue with a small worker pool instead of one OS thread per
        // package. Workers keep draining after a failure so every bar is cleared
        // and all downloads run to completion before the FIRST error is reported
        // (same semantics as the old join-all loop).
        let n_workers = jobs.len().min(DOWNLOAD_WORKERS);
        let queue = Arc::new(Mutex::new(jobs));
        let first_err: Arc<Mutex<Option<anyhow::Error>>> = Arc::new(Mutex::new(None));
        let mut workers = Vec::new();
        for _ in 0..n_workers {
            let queue = Arc::clone(&queue);
            let first_err = Arc::clone(&first_err);
            let tarball_cache = tarball_cache.clone();
            let mp = mp.clone();
            let total_bar = total_bar.clone();
            let job_style = job_style.clone();
            workers.push(std::thread::spawn(move || {
                loop {
                    let job = queue.lock().expect("job queue poisoned").pop();
                    let Some(job) = job else { break };
                    // Length 1 renders empty until download_bytes learns the
                    // real size from Content-Length; cache hits never draw.
                    let bar = mp.add(ProgressBar::new(1));
                    bar.set_style(job_style.clone());
                    bar.set_message(format!("{} @ {}", job.name, job.version));
                    // The receipt is written only after ITS dir extracted
                    // successfully — per-package atomicity: a dir without a
                    // receipt (crash, partial extract) is never trusted.
                    let result = fetch_and_extract(
                        &job.url,
                        &job.integrity,
                        &job.dir,
                        &job.root,
                        bar.clone(),
                        tarball_cache.as_ref(),
                    )
                    .and_then(|_| {
                        receipts::write(&job.dir, &receipts::Receipt {
                            name: job.name.clone(),
                            version: job.version.clone(),
                            integrity: job.integrity.clone(),
                            root: job.root.clone(),
                        })
                    });
                    // Clear the bar even on failure, or its line sticks around
                    // garbling everything printed after it.
                    bar.finish_and_clear();
                    mp.remove(&bar);
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
    }

    // Pointer files are always regenerated: a few tiny idempotent writes,
    // self-healing, and immune to hoist-layout drift.
    for pointer in &plan.pointers {
        let target_dir = Path::new(&pointer.dir);
        if !target_dir.exists() {
            fs::create_dir_all(target_dir)?;
        }
        fs::write(target_dir.join("init.lua"), &pointer.init_lua)?;
    }

    Ok(InstallSummary { installed: to_install.len(), kept })
}

/// Keep the top level of `Packages/` fully managed even when installing
/// incrementally: any non-exempt dir that isn't a desired root alias is junk
/// or a pre-receipt leftover and gets removed. `_`/`.` entries are exempt —
/// a project mid-migration may share this directory with Wally's own
/// `Packages`, whose `_Index` must survive (only DIRS are removed, so
/// wally's root link scripts survive too). Case-insensitive membership
/// because Windows/macOS case-fold names (exact-case renames are handled by
/// the stale/reinstall path, not here).
fn prune_top_level(plan: &crate::roblox::plan::InstallPlan) -> Result<()> {
    let prefix = format!("./{}/", PACKAGES_DIR);
    let desired: std::collections::HashSet<String> = plan.packages.iter()
        .filter_map(|p| {
            let rest = p.path.strip_prefix(&prefix)?;
            if rest.contains('/') { None } else { Some(rest.to_ascii_lowercase()) }
        })
        .collect();

    for entry in fs::read_dir(PACKAGES_DIR)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('_') || name.starts_with('.') {
            continue;
        }
        if entry.file_type()?.is_dir() && !desired.contains(&name.to_ascii_lowercase()) {
            fs::remove_dir_all(entry.path())?;
        }
    }
    Ok(())
}
