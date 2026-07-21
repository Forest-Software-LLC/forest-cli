use anyhow::{Context, Result};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::message::{Message, MessageType};

/// Compile-time target triple (e.g. x86_64-unknown-linux-gnu), emitted by build.rs.
const TARGET: &str = env!("FOREST_TARGET");

/// Where releases live (same host the installer uses). Overridable to match the
/// installer's FOREST_INSTALL_BASE convention.
const DEFAULT_BASE: &str = "https://releases.forest.dev";

/// Throttle the passive "update available" check to once a day.
const CHECK_INTERVAL_SECS: u64 = 60 * 60 * 24;

fn release_base() -> String {
    std::env::var("FOREST_INSTALL_BASE").unwrap_or_else(|_| DEFAULT_BASE.to_string())
}

// ---- Release manifest (subset of cli/latest/latest.json) --------------------

#[derive(Deserialize)]
struct ReleaseFile {
    target: String,
    sha256: String,
    key: String,
    /// Full URL, present only when CDN_BASE was set at release time; otherwise
    /// we build it from base + key.
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize)]
struct ReleaseManifest {
    version: String,
    files: Vec<ReleaseFile>,
}

/// Fetch the release manifest and its detached SSH signature, and verify the
/// signature over the manifest's EXACT bytes before parsing anything. The
/// release host is untrusted: without a valid signature from a pinned offline
/// key (see release_verify.rs), the manifest is not a release.
async fn fetch_manifest(timeout: Option<Duration>) -> Result<ReleaseManifest> {
    let base = release_base();
    let mut builder = reqwest::Client::builder();
    if let Some(t) = timeout {
        builder = builder.timeout(t);
    }
    let client = builder.build().context("failed to build HTTP client")?;

    let get = |url: String| {
        let client = client.clone();
        async move {
            let resp = client
                .get(&url)
                .send()
                .await
                .context("failed to reach the release server")?;
            if !resp.status().is_success() {
                anyhow::bail!("{} returned HTTP {}", url, resp.status());
            }
            resp.bytes().await.context("failed to read response body")
        }
    };

    let manifest_bytes = get(format!("{base}/cli/latest/latest.json")).await?;
    let sig_bytes = get(format!("{base}/cli/latest/latest.json.sig"))
        .await
        .context("release signature is missing — refusing to trust an unsigned manifest")?;
    let sig_pem = std::str::from_utf8(&sig_bytes).context("release signature is not valid UTF-8")?;

    crate::release_verify::verify_manifest_signature(&manifest_bytes, sig_pem)?;

    serde_json::from_slice::<ReleaseManifest>(&manifest_bytes)
        .context("failed to parse the release manifest")
}

/// Is `latest` a newer semver than `current`? If we can't parse our own version
/// we treat the remote as authoritative (so a bad local build can still update).
fn is_newer(current: &str, latest: &str) -> bool {
    match (
        semver::Version::parse(current),
        semver::Version::parse(latest.trim_start_matches('v')),
    ) {
        (Ok(cur), Ok(lat)) => lat > cur,
        (Err(_), Ok(_)) => true,
        _ => false,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

// ---- `forest update` --------------------------------------------------------

/// Download the latest release for this platform, verify its SHA-256, and
/// replace the running binary in place. `check_only` reports availability
/// without installing.
pub async fn update_command(check_only: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");

    let mut msg = Message::new("Checking for updates...");
    let manifest = match fetch_manifest(None).await {
        Ok(m) => m,
        Err(e) => {
            msg.finish(MessageType::Fail, &format!("Update check failed: {e}"));
            return Ok(());
        }
    };
    record_check(&manifest.version); // keep the passive-check throttle fresh

    if !is_newer(current, &manifest.version) {
        msg.finish(
            MessageType::Success,
            &format!("forest is already up to date (v{current})"),
        );
        return Ok(());
    }

    if check_only {
        msg.finish(
            MessageType::Info,
            &format!(
                "Update available: v{current} -> v{}. Run `forest update` to install.",
                manifest.version
            ),
        );
        return Ok(());
    }

    let file = match manifest.files.iter().find(|f| f.target == TARGET) {
        Some(f) => f,
        None => {
            msg.finish(
                MessageType::Fail,
                &format!("No release build for this platform ({TARGET})."),
            );
            return Ok(());
        }
    };

    msg.update(&format!("Downloading v{}...", manifest.version));
    let download_url = file
        .url
        .clone()
        .unwrap_or_else(|| format!("{}/{}", release_base(), file.key));

    let bytes = match download(&download_url).await {
        Ok(b) => b,
        Err(e) => {
            msg.finish(MessageType::Fail, &format!("Download failed: {e}"));
            return Ok(());
        }
    };

    msg.update("Verifying checksum...");
    let actual = sha256_hex(&bytes);
    if !actual.eq_ignore_ascii_case(&file.sha256) {
        msg.finish(
            MessageType::Fail,
            &format!(
                "Checksum mismatch — refusing to install (expected {}, got {actual}).",
                file.sha256
            ),
        );
        return Ok(());
    }

    msg.update("Installing...");
    if let Err(e) = install(&bytes) {
        msg.finish(
            MessageType::Fail,
            &format!(
                "Could not replace the running binary: {e}. \
                 You may need to re-run the installer or use elevated permissions."
            ),
        );
        return Ok(());
    }

    msg.finish(
        MessageType::Success,
        &format!("Updated forest v{current} -> v{} 🌲", manifest.version),
    );
    Ok(())
}

async fn download(url: &str) -> Result<Vec<u8>> {
    let resp = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .context("network error")?
        .error_for_status()
        .context("server returned an error")?;
    let bytes = resp.bytes().await.context("failed to read download body")?;
    Ok(bytes.to_vec())
}

/// Stage the new binary in temp, mark it executable on Unix, then swap it over
/// the running executable. `self_replace` handles the Windows case where you
/// cannot delete a running .exe.
fn install(bytes: &[u8]) -> Result<()> {
    let tmp = std::env::temp_dir().join(format!("forest-update-{}", std::process::id()));
    fs::write(&tmp, bytes).context("failed to stage downloaded binary")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
            .context("failed to set executable bit")?;
    }

    let result = self_replace::self_replace(&tmp).context("failed to replace running executable");
    let _ = fs::remove_file(&tmp);
    result
}

// ---- Passive "update available" nudge --------------------------------------

#[derive(Serialize, Deserialize, Default)]
struct UpdateState {
    last_check: u64,
    latest_version: String,
}

fn state_file() -> Option<PathBuf> {
    let mut path = dirs::home_dir()?;
    path.push(".forest_update_check.json");
    Some(path)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn read_state() -> UpdateState {
    state_file()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn record_check(latest_version: &str) {
    let state = UpdateState {
        last_check: now_secs(),
        latest_version: latest_version.to_string(),
    };
    if let (Some(path), Ok(json)) = (state_file(), serde_json::to_string(&state)) {
        let _ = fs::write(path, json);
    }
}

/// Best-effort, throttled "update available" notice printed to stderr after a
/// command runs. It must never affect the actual command: silent on any error,
/// short network timeout, once a day, and skipped in CI / non-interactive
/// shells (or when FOREST_NO_UPDATE_CHECK is set).
pub async fn maybe_notify_update() {
    use std::io::IsTerminal;

    if std::env::var("CI").is_ok() || std::env::var("FOREST_NO_UPDATE_CHECK").is_ok() {
        return;
    }
    if !std::io::stderr().is_terminal() {
        return;
    }
    if now_secs().saturating_sub(read_state().last_check) < CHECK_INTERVAL_SECS {
        return;
    }

    let manifest = match fetch_manifest(Some(Duration::from_secs(3))).await {
        Ok(m) => m,
        Err(_) => return,
    };
    record_check(&manifest.version);

    let current = env!("CARGO_PKG_VERSION");
    if is_newer(current, &manifest.version) {
        eprintln!(
            "\n{}",
            format!(
                "⬆ forest v{} is available (you have v{current}). Run `forest update` to upgrade.",
                manifest.version
            )
            .yellow()
        );
    }
}
