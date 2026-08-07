use std::{fs, path::PathBuf};

use serde_json::Value;

use crate::utils::sha256_hex;

/// On-disk cache for per-version install metadata (`meta/<sha256>.json`
/// under the tarball cache dir). Same env knobs as the tarball cache:
/// `FOREST_CACHE_DIR` relocates, `FOREST_NO_CACHE=1` disables.
///
/// SECURITY: tarball cache entries are verified against a hash that comes
/// from elsewhere; these entries SUPPLY the hash and the dep list, so a
/// local attacker who can write this directory could steer a fresh lockfile
/// onto a poisoned tarball they also planted. The cache is therefore never
/// trusted on its own: the solver re-fetches every entry it used in one
/// concurrent confirmation pass before writing a lockfile, and a mismatch
/// evicts the entry and re-resolves without the cache. Every hash in a new
/// lockfile came from the registry that session.
#[derive(Clone)]
pub struct MetaCache {
    dir: PathBuf,
}

/// Install-relevant subset of a version-metadata document. Every metadata
/// source (fat list block, cache entry, per-version response) normalizes
/// through here, so the solver and the confirmation comparison see one
/// shape. `public_override` carries the fat response's package-level flag
/// into per-version blocks; short-lived fields like accessUrl are dropped.
pub fn trim_install_meta(source: &Value, public_override: Option<bool>) -> Value {
    let mut out = serde_json::Map::new();
    for field in [
        "dependencies",
        "integrity",
        "archiveRoot",
        "license",
        "licenseRating",
        "licenseCaveats",
        "licenseVerified",
        "compatVersion",
    ] {
        if let Some(v) = source.get(field) {
            if !v.is_null() {
                out.insert(field.to_string(), v.clone());
            }
        }
    }
    let public = public_override.or_else(|| source.get("public").and_then(Value::as_bool));
    if let Some(p) = public {
        out.insert("public".to_string(), Value::Bool(p));
    }
    Value::Object(out)
}

/// Flat hashed file names: Windows paths are case-insensitive but semver
/// prereleases are not, so "1.0.0-RC.1" and "1.0.0-rc.1" would collide as
/// file names. Names and platform fold before hashing; the version does not.
fn entry_key(platform: &str, full_name: &str, version: &str) -> String {
    let id = format!(
        "{}|{}|{}",
        platform.to_lowercase(),
        full_name.to_lowercase(),
        version.trim()
    );
    sha256_hex(id.as_bytes())
}

impl MetaCache {
    pub fn open_default() -> Option<MetaCache> {
        match std::env::var("FOREST_NO_CACHE") {
            Ok(v) if !v.is_empty() && v != "0" => return None,
            _ => {}
        }
        let dir = match std::env::var_os("FOREST_CACHE_DIR") {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ => dirs::home_dir()?.join(".forest").join("cache"),
        };
        Self::open_at(dir.join("meta"))
    }

    pub fn open_at(dir: PathBuf) -> Option<MetaCache> {
        fs::create_dir_all(&dir).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        }
        Some(MetaCache { dir })
    }

    fn path_for(&self, platform: &str, full_name: &str, version: &str) -> PathBuf {
        self.dir.join(format!("{}.json", entry_key(platform, full_name, version)))
    }

    /// Cached trimmed metadata, or None. Entries missing fields the solver
    /// hard-requires are evicted rather than surfaced: a corrupt cache file
    /// should be a miss, not a confusing resolve error.
    pub fn lookup(&self, platform: &str, full_name: &str, version: &str) -> Option<Value> {
        let path = self.path_for(platform, full_name, version);
        let bytes = fs::read(&path).ok()?;
        let value: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(_) => {
                let _ = fs::remove_file(&path);
                return None;
            }
        };
        let integrity_ok = value
            .get("integrity")
            .and_then(Value::as_str)
            .map_or(false, |s| !s.trim().is_empty());
        if !integrity_ok || !value.get("dependencies").map_or(false, Value::is_object) {
            let _ = fs::remove_file(&path);
            return None;
        }
        Some(value)
    }

    /// Best-effort store (tmp + rename, same as the tarball cache). Entries
    /// still rated `pending` are skipped: the rating settles seconds after
    /// publish and would make the next confirmation mismatch spuriously.
    pub fn store(&self, platform: &str, full_name: &str, version: &str, meta: &Value) {
        if meta.get("licenseRating").and_then(Value::as_str) == Some("pending") {
            return;
        }
        let Ok(bytes) = serde_json::to_vec(meta) else { return };
        let path = self.path_for(platform, full_name, version);
        let tmp_dir = self.dir.join("tmp");
        if fs::create_dir_all(&tmp_dir).is_err() {
            return;
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let tmp = tmp_dir.join(format!(
            "{}.{}.{nonce}.part",
            path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id()
        ));
        if fs::write(&tmp, &bytes).is_err() {
            let _ = fs::remove_file(&tmp);
            return;
        }
        if fs::rename(&tmp, &path).is_err() {
            let _ = fs::remove_file(&tmp);
        }
    }

    pub fn evict(&self, platform: &str, full_name: &str, version: &str) {
        let _ = fs::remove_file(self.path_for(platform, full_name, version));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_cache(tag: &str) -> MetaCache {
        let dir = std::env::temp_dir().join(format!("forest-meta-test-{}-{}", tag, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        MetaCache::open_at(dir).expect("temp meta cache dir")
    }

    fn sample_meta() -> Value {
        json!({
            "dependencies": { "a/b": { "version": "^1.0.0", "alias": "B" } },
            "integrity": "abc123",
            "archiveRoot": "",
            "license": "MIT",
            "licenseRating": "safe",
            "licenseCaveats": [],
            "licenseVerified": true,
            "public": true
        })
    }

    #[test]
    fn store_then_lookup_round_trips_and_evicts() {
        let cache = temp_cache("roundtrip");
        assert!(cache.lookup("roblox", "scope/pkg", "1.2.3").is_none());
        cache.store("roblox", "scope/pkg", "1.2.3", &sample_meta());
        // Name/platform casing folds; version casing does not.
        assert_eq!(cache.lookup("Roblox", "Scope/Pkg", "1.2.3"), Some(sample_meta()));
        assert!(cache.lookup("roblox", "scope/pkg", "1.2.3-RC.1").is_none());
        cache.evict("roblox", "scope/pkg", "1.2.3");
        assert!(cache.lookup("roblox", "scope/pkg", "1.2.3").is_none());
        let _ = fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn pending_license_ratings_are_never_stored() {
        let cache = temp_cache("pending");
        let mut meta = sample_meta();
        meta["licenseRating"] = json!("pending");
        cache.store("roblox", "scope/pkg", "1.2.3", &meta);
        assert!(cache.lookup("roblox", "scope/pkg", "1.2.3").is_none());
        let _ = fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn corrupt_or_field_missing_entries_miss_and_are_deleted() {
        let cache = temp_cache("corrupt");
        // Bypass store()'s shaping by planting files directly.
        let path = cache.path_for("roblox", "scope/pkg", "1.0.0");
        fs::write(&path, b"not json").unwrap();
        assert!(cache.lookup("roblox", "scope/pkg", "1.0.0").is_none());
        assert!(!path.exists());

        fs::write(&path, serde_json::to_vec(&json!({ "integrity": "" })).unwrap()).unwrap();
        assert!(cache.lookup("roblox", "scope/pkg", "1.0.0").is_none());
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&cache.dir);
    }

    #[test]
    fn trim_extracts_install_fields_and_injects_public() {
        let gateway_response = json!({
            "name": "Pkg", "scope": "Scope", "version": "1.2.3",
            "description": "ignored",
            "accessUrl": "https://signed.example/never-stored",
            "dependencies": { "a/b": "^1.0.0" },
            "integrity": "abc123",
            "archiveRoot": "src",
            "license": "MIT",
            "licenseRating": "safe",
            "licenseCaveats": [],
            "licenseVerified": false,
            "public": false,
            "ownerType": "user"
        });
        let trimmed = trim_install_meta(&gateway_response, None);
        assert!(trimmed.get("accessUrl").is_none());
        assert!(trimmed.get("description").is_none());
        assert_eq!(trimmed["integrity"], "abc123");
        assert_eq!(trimmed["public"], false);

        // Fat version-list blocks carry no public field of their own; the
        // package-level flag is injected. compatVersion survives when present.
        let block = json!({
            "dependencies": {}, "integrity": "def456", "archiveRoot": "",
            "license": "MIT", "licenseRating": "safe", "licenseCaveats": [],
            "licenseVerified": true, "compatVersion": "41.20"
        });
        let trimmed = trim_install_meta(&block, Some(true));
        assert_eq!(trimmed["public"], true);
        assert_eq!(trimmed["compatVersion"], "41.20");
    }
}
