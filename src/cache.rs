use std::{fs, path::PathBuf};

use crate::utils::sha256_hex;

/// Content-addressed tarball cache: `<dir>/<sha256>.tgz`.
///
/// Best-effort by design: a cache problem must never fail an install, so
/// `store` swallows errors and `lookup` treats anything unexpected as a miss.
/// Every read is re-hashed against its key, so a corrupt or tampered entry
/// can never be extracted — it is deleted and re-downloaded instead.
///
/// Private packages are cached too: the hash addresses the *bytes*, the
/// signed URL only authorizes the fetch, and a user with a cache entry
/// necessarily downloaded it while authorized (they keep the extracted tree
/// in Packages/ regardless). The cache lives in the user's home directory.
#[derive(Clone)]
pub struct TarballCache {
    dir: PathBuf,
}

/// The integrity hash doubles as the cache file name: refuse anything that
/// is not a plain 64-char hex sha256, so a hostile lockfile can't turn the
/// "hash" into a path.
fn normalize_key(integrity: &str) -> Option<String> {
    let key = integrity.trim().to_ascii_lowercase();
    if key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(key)
    } else {
        None
    }
}

impl TarballCache {
    /// Default location: `~/.forest/cache` (alongside the other `~/.forest_*`
    /// files). `FOREST_CACHE_DIR` overrides the location; `FOREST_NO_CACHE=1`
    /// disables caching entirely.
    pub fn open_default() -> Option<TarballCache> {
        match std::env::var("FOREST_NO_CACHE") {
            Ok(v) if !v.is_empty() && v != "0" => return None,
            _ => {}
        }
        let dir = match std::env::var_os("FOREST_CACHE_DIR") {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ => dirs::home_dir()?.join(".forest").join("cache"),
        };
        Self::open_at(dir)
    }

    /// Open a cache rooted at an explicit directory (tests use this so they
    /// never touch process-wide env vars or the real home directory).
    pub fn open_at(dir: PathBuf) -> Option<TarballCache> {
        fs::create_dir_all(&dir).ok()?;
        Some(TarballCache { dir })
    }

    /// Cached bytes for this integrity hash, re-verified on every read.
    /// A mismatching entry is deleted and reported as a miss.
    pub fn lookup(&self, integrity: &str) -> Option<Vec<u8>> {
        let key = normalize_key(integrity)?;
        let path = self.dir.join(format!("{key}.tgz"));
        let bytes = fs::read(&path).ok()?;
        if sha256_hex(&bytes) == key {
            Some(bytes)
        } else {
            let _ = fs::remove_file(&path);
            None
        }
    }

    /// Best-effort store via temp file + rename, so concurrent installs never
    /// observe a half-written entry. Losing a rename race is fine — the
    /// winner wrote identical content (the name IS the content hash).
    pub fn store(&self, integrity: &str, bytes: &[u8]) {
        let Some(key) = normalize_key(integrity) else { return };
        // Callers verify before storing; re-checking here makes the cache
        // impossible to poison through any future call site.
        if sha256_hex(bytes) != key {
            return;
        }
        let path = self.dir.join(format!("{key}.tgz"));
        if path.exists() {
            return;
        }
        let tmp_dir = self.dir.join("tmp");
        if fs::create_dir_all(&tmp_dir).is_err() {
            return;
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let tmp = tmp_dir.join(format!("{key}.{}.{nonce}.part", std::process::id()));
        if fs::write(&tmp, bytes).is_err() {
            let _ = fs::remove_file(&tmp);
            return;
        }
        if fs::rename(&tmp, &path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cache(tag: &str) -> TarballCache {
        let dir = std::env::temp_dir().join(format!("forest-cache-test-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        TarballCache::open_at(dir).expect("temp cache dir")
    }

    #[test]
    fn store_then_lookup_round_trips() {
        let cache = temp_cache("roundtrip");
        let bytes = b"pretend tarball bytes".to_vec();
        let hash = sha256_hex(&bytes);

        assert!(cache.lookup(&hash).is_none(), "empty cache must miss");
        cache.store(&hash, &bytes);
        assert_eq!(cache.lookup(&hash).as_deref(), Some(bytes.as_slice()));
        // Hash case/whitespace from a hand-edited lockfile still hits.
        assert!(cache.lookup(&format!("  {}  ", hash.to_uppercase())).is_some());

        let _ = fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn corrupt_entry_is_deleted_and_missed() {
        let cache = temp_cache("corrupt");
        let bytes = b"real content".to_vec();
        let hash = sha256_hex(&bytes);

        // Plant garbage under the right name, as if the file rotted on disk.
        fs::write(cache.dir.join(format!("{hash}.tgz")), b"garbage").unwrap();

        assert!(cache.lookup(&hash).is_none(), "corrupt entry must miss");
        assert!(
            !cache.dir.join(format!("{hash}.tgz")).exists(),
            "corrupt entry must be deleted"
        );

        let _ = fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn non_hex_integrity_is_rejected() {
        let cache = temp_cache("badkey");
        // Path-traversal shaped, wrong length, and non-hex keys all refuse.
        for bad in ["../../evil", "abc123", &"zz".repeat(32)] {
            cache.store(bad, b"data");
            assert!(cache.lookup(bad).is_none(), "key {bad:?} must never hit");
        }
        // Nothing but the tmp dir may have been created.
        let entries: Vec<_> = fs::read_dir(&cache.dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name() != "tmp")
            .collect();
        assert!(entries.is_empty(), "bad keys must not create cache files");

        let _ = fs::remove_dir_all(&cache.dir);
    }
}
