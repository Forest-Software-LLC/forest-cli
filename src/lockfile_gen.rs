use std::{collections::HashMap, fs, path::{Path, PathBuf}, sync::{Arc, Mutex}};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use urlencoding::encode;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

use reqwest::Method;
use crate::cache::TarballCache;
use crate::http::packages_api_request;
use crate::install_plan::plan_install;
use crate::receipts;
use crate::utils::{digest_package_name, normalize_forest_deps};
use crate::lockfile_solver::{get_lockfile_packages, DepSpec, LockfileEntry};
use crate::message::{Message, MessageType};
use crate::fetch_and_extract::fetch_and_extract;


/// The overall lockfile structure.
#[derive(Debug, Serialize, Deserialize)]
pub struct LockFile {
    pub file_version: u32,
    pub packages: HashMap<String, Vec<LockfileEntry>>,
}

/// Tarballs are content-addressed on the CDN (`/{public|private}/{sha256}.tgz`),
/// so public download URLs are derived from the lockfile's integrity hash rather
/// than stored in the lockfile. Overridable for local stacks, following
/// update.rs's FOREST_INSTALL_BASE convention.
const DEFAULT_CDN_BASE: &str = "https://registry.forest.dev";

pub const PACKAGES_DIR: &str = "Packages";

fn cdn_base() -> String {
    std::env::var("FOREST_CDN_BASE").unwrap_or_else(|_| DEFAULT_CDN_BASE.to_string())
}

/// How many tarballs download (and signed URLs prefetch) at once. Bounded so
/// a large tree doesn't spawn hundreds of OS threads and TLS connections.
const DOWNLOAD_WORKERS: usize = 8;

/// One tarball to download + extract, queued to the bounded worker pool.
struct DownloadJob {
    url: String,
    name: String,
    version: String,
    integrity: String,
    dir: PathBuf,
    root: String,
    bar: ProgressBar,
}

/// Fetch the short-lived signed download URL for one private package version,
/// cross-checking the registry's integrity hash against the lockfile's before
/// anything is downloaded.
async fn fetch_signed_url(
    pkg_name: String,
    version: String,
    lockfile_integrity: String,
    platform: String,
) -> Result<((String, String), String)> {
    let name = digest_package_name(&pkg_name);
    let path = format!(
        "v1/package/{}/{}/{}/{}",
        encode(&name.scope),
        encode(&platform),
        encode(&name.name),
        encode(&version)
    );
    let (info, status) = packages_api_request(&path, Method::GET, None, None).await
        .with_context(|| format!("Failed to fetch access URL for {}@{}", pkg_name, version))?;
    if !status.is_success() {
        return Err(anyhow!(
            "Failed to fetch access URL for {}@{}: HTTP {}",
            pkg_name, version, status
        ));
    }
    let registry_integrity = info.get("integrity")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if !registry_integrity.eq_ignore_ascii_case(lockfile_integrity.trim()) {
        return Err(anyhow!(
            "Integrity mismatch for {}@{}: lockfile has {} but the registry reports {}. \
             Refusing to install — if this version was republished, delete forest-lock.json and re-run `forest install`.",
            pkg_name, version, lockfile_integrity, registry_integrity
        ));
    }
    let url = info.get("accessUrl")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Registry returned no access URL for {}@{}", pkg_name, version))?;
    Ok(((pkg_name, version), url.to_string()))
}

/// What an install run actually did — lets callers print "up to date"
/// instead of implying work happened.
pub struct InstallSummary {
    pub installed: usize,
    #[allow(dead_code)]
    pub kept: usize,
}

pub async fn make_directories(lockfile: &LockFile, root_deps: HashMap<String, DepSpec>, platform: &str, force: bool) -> Result<InstallSummary> {
    // `_`/`.`-prefixed folders in packages/ are exempt from install cleanup
    // (e.g. Wally's `_Index`), so aliases must not claim those names.
    for (pkg_name, spec) in &root_deps {
        if spec.alias.starts_with('_') || spec.alias.starts_with('.') {
            return Err(anyhow!(
                "Alias '{}' for {} cannot start with '_' or '.' — rename it in forest.json",
                spec.alias, pkg_name
            ));
        }
    }

    // All path/pointer computation is pure and lives in install_plan.rs.
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
        receipts::TreeScan::default()
    } else {
        receipts::scan(Path::new(PACKAGES_DIR))
    };
    let rec = receipts::reconcile(&plan, &tree);
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
        let (key, url) = fetch_signed_url(pkg, ver, integrity, platform.to_string()).await?;
        private_urls.insert(key, url);

        let semaphore = Arc::new(tokio::sync::Semaphore::new(DOWNLOAD_WORKERS));
        let mut tasks = tokio::task::JoinSet::new();
        for (pkg, ver, integrity) in private_iter {
            let semaphore = Arc::clone(&semaphore);
            let platform = platform.to_string();
            tasks.spawn(async move {
                let _permit = semaphore.acquire_owned().await.expect("semaphore closed");
                fetch_signed_url(pkg, ver, integrity, platform).await
            });
        }
        while let Some(joined) = tasks.join_next().await {
            let (key, url) = joined.map_err(|e| anyhow!("Signed-URL task panicked: {e}"))??;
            private_urls.insert(key, url);
        }
    }

    // Download + extract only what reconciliation says is missing or changed.
    // The no-op path skips the MultiProgress entirely (no bar flash).
    if !to_install.is_empty() {
        let mp = MultiProgress::new();
        let style = ProgressStyle::with_template("{bar:40.cyan/blue} {msg}")?
            .progress_chars("=> ");

        let mut jobs: Vec<DownloadJob> = Vec::new();
        for &i in &to_install {
            let pkg = &plan.packages[i];
            let dir_path = PathBuf::from(&pkg.path);
            if !dir_path.exists() {
                fs::create_dir_all(&dir_path)?;
            }

            let bar = mp.add(ProgressBar::new(100));
            bar.set_style(style.clone());
            bar.set_message(format!("{} @ {}", pkg.name, pkg.version));

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
                bar,
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
            workers.push(std::thread::spawn(move || {
                loop {
                    let job = queue.lock().expect("job queue poisoned").pop();
                    let Some(job) = job else { break };
                    // Clear the bar even on failure, or its line sticks around
                    // garbling everything printed after it.
                    // The receipt is written only after ITS dir extracted
                    // successfully — per-package atomicity: a dir without a
                    // receipt (crash, partial extract) is never trusted.
                    let result = fetch_and_extract(
                        &job.url,
                        &job.integrity,
                        &job.dir,
                        &job.root,
                        job.bar.clone(),
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
                    job.bar.finish_and_clear();
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
fn prune_top_level(plan: &crate::install_plan::InstallPlan) -> Result<()> {
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

/// Generate a lockfile JSON string given the forest manifest & message spinner.
pub async fn lockfile_gen(forest_json: &Value, msg: &mut Message, force: bool) -> Result<LockFile> {
    let roots = normalize_forest_deps(forest_json);
    let platform: String = forest_json
        .get("platform")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Missing platform in forest.json"))?
        .to_string(); // clone the value so we don't hold a borrow

    msg.update("Resolving dependencies...");
    let (lockfile_packages, license_warnings) = get_lockfile_packages(roots.clone(), platform.clone()).await
        .context("Failed to resolve lockfile packages")?;

    // Surface registry license-safety ratings for anything caution/unsafe in
    // the resolved tree (direct and transitive) before files land on disk.
    // One line per package — the full caveats live in `forest audit`.
    for warning in &license_warnings {
        msg.emit(MessageType::Warn, &warning.headline());
    }
    if !license_warnings.is_empty() {
        msg.emit(
            MessageType::Info,
            "Run `forest audit` for license details (automated review — not legal advice).",
        );
    }

    let lockfile : LockFile = LockFile {
        file_version: 2,
        packages: lockfile_packages
    };

    // make_directories draws its own download bars — hide the spinner while
    // they own the terminal, or the two draw systems leave stuck lines.
    msg.pause();
    make_directories(&lockfile, roots, &platform, force).await
        .context("Failed to create directories for lockfile packages")?;
    msg.resume();

    Ok(lockfile)
}