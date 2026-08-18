//! Rojo-safe filesystem scratch machinery for mount mutation: every dir that
//! leaves the Packages mount is renamed out (one atomic watcher event) and
//! deleted elsewhere, and every dir that enters it is assembled elsewhere and
//! renamed in. A live `rojo serve` (7.7.0) unwrap-panics on watcher events
//! whose paths no longer resolve, so the mount must only ever see whole-unit
//! renames; see install.rs for the ordering rules and scripts/rojo-bench.ps1
//! for the regression net.

use std::fs;
use std::path::{Path, PathBuf};

/// Deleting dirs inside the mount crashes a live `rojo serve`:
/// remove_dir_all deletes children first, and rojo canonicalizes each
/// removed path's parent, so a child event processed after the parent is
/// gone panics the server (rojo 7.7.0 src/change_processor.rs:179). The bin
/// renames each doomed dir out of the mount instead (one atomic event with
/// a live parent) and deletes it outside any watched tree.
pub struct TrashBin {
    dir: PathBuf,
    counter: u64,
    created: bool,
}

impl TrashBin {
    /// `dir` comes from `scratch_dirs()`: same volume as the mount (so
    /// rename never degrades to copy+delete) and outside the watched tree.
    /// A leftover bin from a crashed run is swept here.
    pub fn new(dir: PathBuf) -> Self {
        if dir.exists() {
            let _ = fs::remove_dir_all(&dir);
        }
        TrashBin { dir, counter: 0, created: false }
    }

    /// Move `path` into the bin. The rename is retried hard: a watcher
    /// re-snapshotting the tree holds file handles inside it, and Windows
    /// denies renaming a dir with open children. If every retry fails this
    /// falls back to deleting in place, accepting the rojo crash risk.
    pub fn remove_dir_all(&mut self, path: &Path) -> std::io::Result<()> {
        if !self.created {
            fs::create_dir_all(&self.dir)?;
            self.created = true;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let target = self.dir.join(format!("{}-{}-{}", std::process::id(), self.counter, name));
        self.counter += 1;
        let mut result = Ok(());
        for attempt in 0..20 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            result = fs::rename(path, &target);
            if result.is_ok() {
                return result;
            }
        }
        result.or_else(|_| fs::remove_dir_all(path))
    }
}

impl Drop for TrashBin {
    /// Best effort on every exit path; a failure is swept by the next run.
    fn drop(&mut self) {
        if self.created {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

/// Staging ground for extraction: tarballs unpack outside the mount, then
/// each completed package dir renames into place as one atomic event.
/// Extracting in place streams hundreds of per-file create events to a live
/// rojo, and any path removed again before its event is processed panics
/// the server (rojo 7.7.0 src/change_processor.rs:172).
pub struct StagingArea {
    dir: PathBuf,
    created: bool,
}

impl StagingArea {
    /// Same placement rules as TrashBin: from `scratch_dirs()`, swept here.
    pub fn new(dir: PathBuf) -> Self {
        if dir.exists() {
            let _ = fs::remove_dir_all(&dir);
        }
        StagingArea { dir, created: false }
    }

    /// Fresh unique dir for one package's extraction.
    pub fn alloc(&mut self, idx: usize) -> std::io::Result<PathBuf> {
        let p = self.dir.join(format!("{}-{}", std::process::id(), idx));
        fs::create_dir_all(&p)?;
        self.created = true;
        Ok(p)
    }
}

impl Drop for StagingArea {
    /// Best effort on every exit path; a failure is swept by the next run.
    fn drop(&mut self) {
        if self.created {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

/// Where the trash and staging dirs live. System temp is preferred: rojo
/// watches the whole project root recursively (notify re-keys pending
/// staging events onto final mount paths), so project-local scratch still
/// leaks watcher events. Temp only works when it shares a volume with the
/// project (renames must move, never copy); otherwise both dirs fall back
/// to dot-named siblings of forest.json.
pub struct ScratchDirs {
    pub trash: PathBuf,
    pub staging: PathBuf,
}

pub fn scratch_dirs() -> ScratchDirs {
    let temp = std::env::temp_dir().join("forest-scratch");
    if same_volume(Path::new("."), &std::env::temp_dir()) {
        let pid = std::process::id();
        ScratchDirs {
            trash: temp.join(format!("{}-trash", pid)),
            staging: temp.join(format!("{}-stage", pid)),
        }
    } else {
        ScratchDirs {
            trash: PathBuf::from(".forest-trash"),
            staging: PathBuf::from(".forest-staging"),
        }
    }
}

/// Sweep project-local scratch dirs left by an older run (or the fallback
/// mode of a crashed one), regardless of where this run's scratch lives.
pub fn sweep_leftovers() {
    for leftover in [".forest-trash", ".forest-staging"] {
        if Path::new(leftover).exists() {
            let _ = fs::remove_dir_all(leftover);
        }
    }
}

/// Same-volume check WITHOUT writing anywhere: a rename probe inside the
/// project would itself be a create-then-remove event for the watcher.
#[cfg(windows)]
fn same_volume(a: &Path, b: &Path) -> bool {
    fn root(p: &Path) -> Option<std::ffi::OsString> {
        let canon = fs::canonicalize(p).ok()?;
        match canon.components().next()? {
            std::path::Component::Prefix(pre) => {
                Some(pre.as_os_str().to_ascii_uppercase())
            }
            _ => None,
        }
    }
    matches!((root(a), root(b)), (Some(x), Some(y)) if x == y)
}

#[cfg(not(windows))]
fn same_volume(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (fs::metadata(a), fs::metadata(b)) {
        (Ok(ma), Ok(mb)) => ma.dev() == mb.dev(),
        _ => false,
    }
}

/// Windows can transiently deny a dir rename (indexer or AV holding a child
/// open); a few short retries ride that out.
pub fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut last = None;
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    Err(last.expect("at least one rename attempt"))
}
