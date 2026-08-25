//! Group deletion with undo (REQUIREMENTS.md R4, R8).
//!
//! Deletion always moves files to the freedesktop trash, never unlinks them. Culling is
//! fast and misfires are certain, so recoverability is the point rather than a nicety.
//!
//! Per R11 the tree changes underneath us: a file that has already disappeared counts as
//! a **success**, not a failure. The user's intent was "this should not be here", and it
//! is not there.

use std::path::{Path, PathBuf};

/// Outcome of trashing one group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deletion {
    /// Stem of the group that was deleted, for display in the undo log.
    pub stem: String,
    /// Files actually moved to the trash, in the order they were moved.
    pub trashed: Vec<PathBuf>,
    /// Files that were already gone. Not an error (R11).
    pub already_absent: Vec<PathBuf>,
    /// Files that could not be trashed, with the reason.
    pub failed: Vec<(PathBuf, String)>,
}

impl Deletion {
    /// True when nothing was left behind unintentionally.
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty()
    }

    /// Human-readable one-liner for the status bar.
    pub fn summary(&self) -> String {
        let mut s = format!("trashed {} ({} files)", self.stem, self.trashed.len());
        if !self.already_absent.is_empty() {
            s += &format!(", {} already gone", self.already_absent.len());
        }
        if !self.failed.is_empty() {
            s += &format!(", {} FAILED", self.failed.len());
        }
        s
    }
}

/// Moves files to the trash. Abstracted so deletion logic is testable without
/// depending on a working desktop trash implementation in CI or a sandbox.
pub trait Bin: Send + Sync {
    fn send(&self, path: &Path) -> Result<(), String>;
    /// Restore a previously trashed path. Best-effort.
    fn restore(&self, path: &Path) -> Result<(), String>;
}

/// The real freedesktop trash.
pub struct SystemBin;

impl Bin for SystemBin {
    fn send(&self, path: &Path) -> Result<(), String> {
        trash::delete(path).map_err(|e| e.to_string())
    }

    fn restore(&self, path: &Path) -> Result<(), String> {
        // The `trash` crate can only restore by matching entries in the trash listing,
        // so find the most recent entry whose original path is the one we want.
        let items = trash::os_limited::list().map_err(|e| e.to_string())?;
        let mut candidates: Vec<_> = items
            .into_iter()
            .filter(|i| i.original_path() == path)
            .collect();
        if candidates.is_empty() {
            return Err(format!("{} not found in trash", path.display()));
        }
        // Most recently deleted first.
        candidates.sort_by_key(|i| i.time_deleted);
        let newest = candidates.pop().expect("candidates is non-empty");
        trash::os_limited::restore_all([newest]).map_err(|e| e.to_string())
    }
}

/// Deletion history, newest last.
#[derive(Default)]
pub struct History {
    entries: Vec<Deletion>,
    limit: usize,
}

impl History {
    /// `limit` caps retained undo steps; 0 means unlimited.
    pub fn new(limit: usize) -> Self {
        Self {
            entries: Vec::new(),
            limit,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn last(&self) -> Option<&Deletion> {
        self.entries.last()
    }

    fn push(&mut self, d: Deletion) {
        self.entries.push(d);
        if self.limit > 0 && self.entries.len() > self.limit {
            self.entries.remove(0);
        }
    }
}

/// Move every file of a group to the trash.
///
/// Files are attempted independently: one failure does not abort the rest, because a
/// half-deleted group is worse than a fully-attempted one.
pub fn delete_group(bin: &dyn Bin, stem: &str, paths: &[&Path]) -> Deletion {
    let mut d = Deletion {
        stem: stem.to_string(),
        trashed: Vec::new(),
        already_absent: Vec::new(),
        failed: Vec::new(),
    };

    for p in paths {
        // R11: already gone is the desired end state, so treat it as success.
        if !p.exists() {
            d.already_absent.push(p.to_path_buf());
            continue;
        }
        match bin.send(p) {
            Ok(()) => d.trashed.push(p.to_path_buf()),
            // Racing with an external delete: also fine.
            Err(_) if !p.exists() => d.already_absent.push(p.to_path_buf()),
            Err(e) => d.failed.push((p.to_path_buf(), e)),
        }
    }

    d
}

/// Delete a group and record it for undo.
pub fn delete_and_record(
    bin: &dyn Bin,
    history: &mut History,
    stem: &str,
    paths: &[&Path],
) -> Deletion {
    let d = delete_group(bin, stem, paths);
    // Only record something that can actually be undone.
    if !d.trashed.is_empty() {
        history.push(d.clone());
    }
    d
}

/// Result of an undo attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restore {
    pub stem: String,
    pub restored: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, String)>,
}

impl Restore {
    pub fn summary(&self) -> String {
        let mut s = format!("restored {} ({} files)", self.stem, self.restored.len());
        if !self.failed.is_empty() {
            s += &format!(", {} FAILED", self.failed.len());
        }
        s
    }
}

/// Undo the most recent deletion.
///
/// Returns `None` when there is nothing to undo. The entry is consumed either way, so a
/// partially restorable deletion is not retried forever.
pub fn undo(bin: &dyn Bin, history: &mut History) -> Option<Restore> {
    let d = history.entries.pop()?;
    let mut r = Restore {
        stem: d.stem,
        restored: Vec::new(),
        failed: Vec::new(),
    };
    for p in d.trashed {
        match bin.restore(&p) {
            Ok(()) => r.restored.push(p),
            Err(e) => r.failed.push((p, e)),
        }
    }
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// A trash that moves files into a holding directory, so restore is real.
    struct FakeBin {
        store: PathBuf,
        /// trashed path -> where it now lives
        moved: Mutex<HashMap<PathBuf, PathBuf>>,
        /// Paths whose deletion should fail, to exercise the error path.
        refuse: Vec<PathBuf>,
    }

    impl FakeBin {
        fn new(store: PathBuf) -> Self {
            Self {
                store,
                moved: Mutex::new(HashMap::new()),
                refuse: Vec::new(),
            }
        }
    }

    impl Bin for FakeBin {
        fn send(&self, path: &Path) -> Result<(), String> {
            if self.refuse.contains(&path.to_path_buf()) {
                return Err("permission denied".into());
            }
            let name = path.file_name().ok_or("no file name")?;
            let dest = self.store.join(name);
            std::fs::rename(path, &dest).map_err(|e| e.to_string())?;
            self.moved.lock().unwrap().insert(path.to_path_buf(), dest);
            Ok(())
        }

        fn restore(&self, path: &Path) -> Result<(), String> {
            let dest = self
                .moved
                .lock()
                .unwrap()
                .remove(path)
                .ok_or_else(|| format!("{} not in trash", path.display()))?;
            std::fs::rename(&dest, path).map_err(|e| e.to_string())
        }
    }

    /// Create a group of files on disk, returning their paths.
    fn make_group(dir: &Path, stem: &str, exts: &[&str]) -> Vec<PathBuf> {
        exts.iter()
            .map(|e| {
                let p = dir.join(format!("{stem}.{e}"));
                std::fs::write(&p, b"x").unwrap();
                p
            })
            .collect()
    }

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let td = tempfile::tempdir().unwrap();
        let work = td.path().join("photos");
        let store = td.path().join("trash");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&store).unwrap();
        (td, work, store)
    }

    #[test]
    fn deletes_every_file_in_the_group() {
        let (_td, work, store) = setup();
        // The real shape: RAW, JPEG and both RawTherapee sidecars.
        let paths = make_group(&work, "A6701113", &["ARW", "JPG", "ARW.pp3", "JPG.pp3"]);
        let refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
        let bin = FakeBin::new(store);

        let d = delete_group(&bin, "A6701113", &refs);

        assert!(d.is_clean(), "{d:?}");
        assert_eq!(d.trashed.len(), 4, "all four files must go");
        assert!(d.already_absent.is_empty());
        for p in &paths {
            assert!(!p.exists(), "{} should be gone", p.display());
        }
    }

    #[test]
    fn missing_files_count_as_success() {
        // R11: the user deleted it elsewhere while we were running.
        let (_td, work, store) = setup();
        let paths = make_group(&work, "A001", &["JPG", "ARW"]);
        std::fs::remove_file(&paths[0]).unwrap();
        let refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
        let bin = FakeBin::new(store);

        let d = delete_group(&bin, "A001", &refs);

        assert!(d.is_clean(), "an already-absent file is not a failure");
        assert_eq!(d.already_absent.len(), 1);
        assert_eq!(d.trashed.len(), 1);
    }

    #[test]
    fn one_failure_does_not_abort_the_rest() {
        let (_td, work, store) = setup();
        let paths = make_group(&work, "A002", &["JPG", "ARW", "xmp"]);
        let mut bin = FakeBin::new(store);
        bin.refuse.push(paths[1].clone());
        let refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();

        let d = delete_group(&bin, "A002", &refs);

        assert!(!d.is_clean());
        assert_eq!(d.failed.len(), 1);
        assert_eq!(d.trashed.len(), 2, "the other two must still be trashed");
        assert!(paths[1].exists(), "the refused file stays put");
    }

    #[test]
    fn undo_restores_the_whole_group() {
        let (_td, work, store) = setup();
        let paths = make_group(&work, "A003", &["JPG", "ARW", "ARW.pp3"]);
        let refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
        let bin = FakeBin::new(store);
        let mut hist = History::new(0);

        delete_and_record(&bin, &mut hist, "A003", &refs);
        assert_eq!(hist.len(), 1);
        for p in &paths {
            assert!(!p.exists());
        }

        let r = undo(&bin, &mut hist).expect("something to undo");
        assert_eq!(r.restored.len(), 3);
        assert!(r.failed.is_empty(), "{r:?}");
        for p in &paths {
            assert!(p.exists(), "{} should be back", p.display());
        }
        assert!(hist.is_empty());
    }

    #[test]
    fn undo_unwinds_in_reverse_order() {
        let (_td, work, store) = setup();
        let a = make_group(&work, "A", &["JPG"]);
        let b = make_group(&work, "B", &["JPG"]);
        let bin = FakeBin::new(store);
        let mut hist = History::new(0);

        delete_and_record(&bin, &mut hist, "A", &[a[0].as_path()]);
        delete_and_record(&bin, &mut hist, "B", &[b[0].as_path()]);

        // Most recent first.
        assert_eq!(undo(&bin, &mut hist).unwrap().stem, "B");
        assert!(b[0].exists());
        assert!(!a[0].exists());

        assert_eq!(undo(&bin, &mut hist).unwrap().stem, "A");
        assert!(a[0].exists());
    }

    #[test]
    fn undo_on_empty_history_is_none() {
        let (_td, _work, store) = setup();
        let bin = FakeBin::new(store);
        let mut hist = History::new(0);
        assert!(undo(&bin, &mut hist).is_none());
    }

    #[test]
    fn nothing_trashed_means_nothing_to_undo() {
        // Deleting a group whose files are all already gone must not create a
        // history entry that would "restore" nothing.
        let (_td, work, store) = setup();
        let ghost = work.join("gone.JPG");
        let bin = FakeBin::new(store);
        let mut hist = History::new(0);

        let d = delete_and_record(&bin, &mut hist, "gone", &[ghost.as_path()]);

        assert!(d.is_clean());
        assert_eq!(d.already_absent.len(), 1);
        assert!(hist.is_empty(), "no undoable work was done");
    }

    #[test]
    fn history_respects_its_limit() {
        let (_td, work, store) = setup();
        let bin = FakeBin::new(store);
        let mut hist = History::new(3);

        for i in 0..6 {
            let stem = format!("G{i}");
            let p = make_group(&work, &stem, &["JPG"]);
            delete_and_record(&bin, &mut hist, &stem, &[p[0].as_path()]);
        }

        assert_eq!(hist.len(), 3, "oldest entries are dropped");
        // The newest is retained.
        assert_eq!(hist.last().unwrap().stem, "G5");
    }

    #[test]
    fn failed_restore_is_reported_and_not_retried() {
        let (_td, work, store) = setup();
        let paths = make_group(&work, "A004", &["JPG"]);
        let bin = FakeBin::new(store.clone());
        let mut hist = History::new(0);
        delete_and_record(&bin, &mut hist, "A004", &[paths[0].as_path()]);

        // Remove the file from the fake trash so restore cannot succeed.
        std::fs::remove_file(store.join("A004.JPG")).unwrap();

        let r = undo(&bin, &mut hist).unwrap();
        assert_eq!(r.failed.len(), 1);
        assert!(r.restored.is_empty());
        // Consumed regardless, so the user is not stuck retrying.
        assert!(hist.is_empty());
    }

    #[test]
    fn summaries_are_informative() {
        let d = Deletion {
            stem: "A6701113".into(),
            trashed: vec![PathBuf::from("a"), PathBuf::from("b")],
            already_absent: vec![PathBuf::from("c")],
            failed: vec![(PathBuf::from("d"), "nope".into())],
        };
        let s = d.summary();
        assert!(s.contains("A6701113") && s.contains("2 files"));
        assert!(s.contains("already gone") && s.contains("FAILED"));
    }
}
