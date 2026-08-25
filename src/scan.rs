//! Directory scanning and basename grouping (REQUIREMENTS.md R3, R4).
//!
//! A "group" is every file sharing a basename stem: `A6709605.JPG`, `A6709605.ARW`, and
//! any `A6709605.*.pp3` / `.xmp` sidecars are one unit. The user navigates and deletes
//! groups, never individual files.
//!
//! Grouping is by stem rather than by a JPEG/RAW pairing rule so that unanticipated
//! sidecar conventions are swept along for free. The real data justifies this: RawTherapee
//! writes `A6701113.ARW.pp3`, i.e. full-name-plus-extension, which a JPEG/RAW pairing rule
//! would miss but a stem rule catches.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rayon::prelude::*;

/// Extensions we can display, best first. Lower index wins when a group has several.
const DISPLAY_EXTS: &[&str] = &["jpg", "jpeg", "arw"];

/// Role of a file within its group.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// A file we can decode and show.
    Displayable,
    /// Carried along on delete, never displayed.
    Sidecar,
}

#[derive(Clone, Debug)]
pub struct Member {
    pub path: PathBuf,
    pub role: Role,
    /// Lowercased extension, for display-preference ranking.
    pub ext: String,
}

/// Size and timestamp for one file.
///
/// Deliberately *not* part of [`Member`]. Filling these in requires one `stat` per file,
/// and on a cold cache over a large library that dominates startup entirely: ~4300 files
/// at sub-millisecond each is the ~3 s launch reported against R23. They feed two display
/// columns and a confirmation message, none of which is needed to show the first photo,
/// so they are gathered off the startup path -- see [`MetaStore`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileMeta {
    /// Zero if the file vanished before we could stat it (R11).
    pub size: u64,
    /// Modification time. For camera files this is effectively capture time.
    pub modified: Option<SystemTime>,
}

/// Sizes and timestamps, gathered in the background after the window is up.
///
/// Keyed by path rather than index deliberately: deleting a group shifts every later
/// index, and stale sizes attached to the wrong rows would be worse than absent ones.
/// Until a path has been stat'ed its row simply shows `-`.
#[derive(Default)]
pub struct MetaStore {
    map: std::sync::RwLock<HashMap<PathBuf, FileMeta>>,
}

impl MetaStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stat every path and publish the result in one write.
    ///
    /// The whole batch is built locally first, so the UI never blocks on a writer
    /// holding the lock, and the stats themselves run in parallel because each is a
    /// blocking syscall that mostly waits.
    pub fn fill(&self, paths: &[PathBuf]) {
        let gathered: Vec<(PathBuf, FileMeta)> = paths
            .par_iter()
            .map(|p| {
                let meta = std::fs::metadata(p).ok();
                (
                    p.clone(),
                    FileMeta {
                        size: meta.as_ref().map_or(0, |m| m.len()),
                        modified: meta.as_ref().and_then(|m| m.modified().ok()),
                    },
                )
            })
            .collect();
        let mut map = self.map.write().unwrap();
        map.extend(gathered);
    }

    pub fn get(&self, path: &Path) -> Option<FileMeta> {
        self.map.read().unwrap().get(path).copied()
    }

    /// Total bytes across a group, or `None` until its files have been stat'ed.
    pub fn group_bytes(&self, group: &Group) -> Option<u64> {
        let map = self.map.read().unwrap();
        let mut total = 0;
        let mut seen = false;
        for m in &group.members {
            if let Some(meta) = map.get(&m.path) {
                total += meta.size;
                seen = true;
            }
        }
        seen.then_some(total)
    }

    /// Earliest modification time in a group, used as the capture time.
    pub fn group_modified(&self, group: &Group) -> Option<SystemTime> {
        let map = self.map.read().unwrap();
        group
            .members
            .iter()
            .filter_map(|m| map.get(&m.path).and_then(|f| f.modified))
            .min()
    }

    pub fn len(&self) -> usize {
        self.map.read().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One navigable item: all files sharing a basename stem within a directory.
#[derive(Clone, Debug)]
pub struct Group {
    /// Directory containing the group.
    pub dir: PathBuf,
    /// Shared basename stem, e.g. `A6709605`.
    pub stem: String,
    pub members: Vec<Member>,
}

impl Group {
    /// The file to decode for display, preferring JPEG over RAW because a sidecar JPEG
    /// needs no preview extraction. Returns `None` for a sidecar-only group.
    pub fn display_path(&self) -> Option<&Path> {
        self.members
            .iter()
            .filter(|m| m.role == Role::Displayable)
            .min_by_key(|m| {
                DISPLAY_EXTS
                    .iter()
                    .position(|e| *e == m.ext)
                    .unwrap_or(usize::MAX)
            })
            .map(|m| m.path.as_path())
    }

    /// Every file to remove when this group is deleted.
    pub fn all_paths(&self) -> Vec<&Path> {
        self.members.iter().map(|m| m.path.as_path()).collect()
    }

    /// True when the group has a RAW but no sidecar JPEG, so display comes from the
    /// embedded preview.
    pub fn is_raw_only(&self) -> bool {
        self.display_path()
            .and_then(|p| p.extension())
            .is_some_and(|e| e.eq_ignore_ascii_case("arw"))
    }

    /// True when the group contains a RAW file at all.
    pub fn has_raw(&self) -> bool {
        self.members.iter().any(|m| m.ext == "arw")
    }

    /// True when the group contains a JPEG.
    pub fn has_jpeg(&self) -> bool {
        self.members.iter().any(|m| m.ext == "jpg" || m.ext == "jpeg")
    }

    /// Short label for what the group holds, e.g. `JPG+ARW`.
    ///
    /// This is what tells the user at a glance whether deleting the row will also take a
    /// RAW with it.
    pub fn kind(&self) -> &'static str {
        match (self.has_jpeg(), self.has_raw()) {
            (true, true) => "JPG+ARW",
            (true, false) => "JPG",
            (false, true) => "ARW",
            (false, false) => "-",
        }
    }

    /// Every file path in the group, for stat'ing in the background.
    pub fn member_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.members.iter().map(|m| &m.path)
    }
}

/// Human-readable byte count, e.g. `31.8 MB`.
pub fn format_size(bytes: u64) -> String {
    const MB: f64 = 1e6;
    const GB: f64 = 1e9;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if bytes > 0 {
        format!("{:.0} kB", b / 1e3)
    } else {
        "-".to_string()
    }
}

/// Format a timestamp as `YYYY-MM-DD HH:MM` in UTC.
///
/// Done by hand rather than pulling in a date crate: this is the only place a date is
/// ever displayed, and the civil-from-days conversion is a well-known closed form.
pub fn format_time(t: SystemTime) -> String {
    let secs = match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        // Pre-1970 timestamps are not worth handling for camera files.
        Err(_) => return "-".to_string(),
    };
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (hour, minute) = (rem / 3600, (rem % 3600) / 60);

    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    format!("{y:04}-{m:02}-{d:02} {hour:02}:{minute:02}")
}

/// Split a filename into the grouping stem and a lowercased final extension.
///
/// The stem is everything before the *first* dot, which is what makes
/// `A6701113.ARW.pp3` group with `A6701113.JPG`. Leading dots are preserved so hidden
/// files do not all collapse into one group with an empty stem.
fn split_name(name: &str) -> (String, String) {
    let (lead, rest) = match name.strip_prefix('.') {
        Some(r) => (".", r),
        None => ("", name),
    };
    let stem = rest.split('.').next().unwrap_or("");
    let ext = match rest.rsplit_once('.') {
        Some((_, e)) if !e.is_empty() => e.to_ascii_lowercase(),
        _ => String::new(),
    };
    (format!("{lead}{stem}"), ext)
}

fn role_for(ext: &str) -> Role {
    if DISPLAY_EXTS.contains(&ext) {
        Role::Displayable
    } else {
        Role::Sidecar
    }
}

/// Group the files of a single directory by stem.
///
/// Groups containing no displayable file are dropped: an orphaned `.pp3` is not something
/// to navigate to.
pub fn group_files<I, P>(dir: &Path, files: I) -> Vec<Group>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    // Purely string work: no file is opened or stat'ed here, which is what keeps a scan
    // proportional to `read_dir` rather than to the number of files (R23).
    let mut by_stem: HashMap<String, Vec<Member>> = HashMap::new();
    for f in files {
        let path = f.as_ref();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let (stem, ext) = split_name(name);
        if stem.is_empty() {
            continue;
        }
        by_stem.entry(stem).or_default().push(Member {
            path: path.to_path_buf(),
            role: role_for(&ext),
            ext,
        });
    }

    let mut groups: Vec<Group> = by_stem
        .into_iter()
        .filter(|(_, members)| members.iter().any(|m| m.role == Role::Displayable))
        .map(|(stem, mut members)| {
            // Stable order so the UI and delete lists are deterministic.
            members.sort_by(|a, b| a.path.cmp(&b.path));
            Group {
                dir: dir.to_path_buf(),
                stem,
                members,
            }
        })
        .collect();

    groups.sort_by(|a, b| a.stem.cmp(&b.stem));
    groups
}

/// A directory that contains at least one group, plus its groups.
#[derive(Clone, Debug)]
pub struct DirNode {
    pub path: PathBuf,
    pub groups: Vec<Group>,
}

/// Recursively scan `root`, returning directories that contain displayable groups.
///
/// Symlinked directories are not followed, to avoid cycles.
pub fn scan(root: &Path) -> Vec<DirNode> {
    // Phase 1: walk the tree. One `read_dir` per directory, and `file_type()` is served
    // from the directory entry, so nothing here stats a file.
    let mut listings: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files = Vec::new();
        let mut subdirs = Vec::new();

        for e in entries.flatten() {
            let path = e.path();
            // file_type() does not traverse symlinks, so this both classifies and
            // guards against link cycles.
            match e.file_type() {
                Ok(t) if t.is_dir() => subdirs.push(path),
                Ok(t) if t.is_file() => files.push(path),
                _ => {}
            }
        }

        listings.push((dir, files));
        subdirs.sort();
        // Reversed, because popping a stack inverts order.
        stack.extend(subdirs.into_iter().rev());
    }

    // Phase 2: stat and group, in parallel across directories. This is where a scan
    // actually spends its time -- thousands of blocking stats (R23). Order does not
    // matter because the result is sorted below, so the scan stays deterministic.
    let mut out: Vec<DirNode> = listings
        .into_par_iter()
        .filter_map(|(dir, files)| {
            let groups = group_files(&dir, &files);
            (!groups.is_empty()).then_some(DirNode { path: dir, groups })
        })
        .collect();

    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// The scan flattened into navigation order: directories in path order, groups by stem.
///
/// This is the sequence the prefetch ring indexes into.
pub fn flatten(dirs: &[DirNode]) -> Vec<Group> {
    dirs.iter().flat_map(|d| d.groups.iter().cloned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn splits_simple_name() {
        assert_eq!(split_name("A6709605.JPG"), ("A6709605".into(), "jpg".into()));
        assert_eq!(split_name("A6709605.ARW"), ("A6709605".into(), "arw".into()));
    }

    #[test]
    fn double_extension_sidecar_groups_with_stem() {
        // The real RawTherapee convention, and the reason grouping is by first dot.
        assert_eq!(
            split_name("A6701113.ARW.pp3"),
            ("A6701113".into(), "pp3".into())
        );
        assert_eq!(
            split_name("A6701113.JPG.pp3"),
            ("A6701113".into(), "pp3".into())
        );
    }

    #[test]
    fn handles_no_extension_and_hidden_files() {
        assert_eq!(split_name("README"), ("README".into(), "".into()));
        // Hidden files keep their dot so they don't all share an empty stem.
        assert_eq!(split_name(".hidden"), (".hidden".into(), "".into()));
        assert_eq!(split_name(".config.bak"), (".config".into(), "bak".into()));
    }

    #[test]
    fn groups_a_realistic_directory() {
        let files = [
            p("/d/A6709605.ARW"),
            p("/d/A6709605.JPG"),
            p("/d/A6709606.ARW"),
            p("/d/A6709606.JPG"),
            p("/d/A6701113.ARW"),
            p("/d/A6701113.JPG"),
            p("/d/A6701113.ARW.pp3"),
            p("/d/A6701113.JPG.pp3"),
        ];
        let groups = group_files(Path::new("/d"), files);

        assert_eq!(groups.len(), 3, "three stems");
        // Sorted by stem, so A6701113 comes first.
        assert_eq!(groups[0].stem, "A6701113");
        assert_eq!(
            groups[0].members.len(),
            4,
            "both images and both pp3 sidecars belong to the group"
        );
        // Deleting must take all four.
        assert_eq!(groups[0].all_paths().len(), 4);
        // JPEG is preferred for display over the ARW.
        assert_eq!(groups[0].display_path().unwrap(), Path::new("/d/A6701113.JPG"));
        assert!(!groups[0].is_raw_only());
    }

    #[test]
    fn raw_only_group_displays_from_raw() {
        let groups = group_files(Path::new("/d"), [p("/d/A001.ARW")]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].display_path().unwrap(), Path::new("/d/A001.ARW"));
        assert!(groups[0].is_raw_only(), "must fall back to embedded preview");
    }

    #[test]
    fn sidecar_only_group_is_dropped() {
        // An orphaned pp3 is not navigable.
        let groups = group_files(Path::new("/d"), [p("/d/A001.ARW.pp3")]);
        assert!(groups.is_empty());
    }

    #[test]
    fn extension_case_is_ignored() {
        let groups = group_files(Path::new("/d"), [p("/d/a.jpg"), p("/d/b.JPG")]);
        assert_eq!(groups.len(), 2);
        for g in &groups {
            assert_eq!(g.members[0].role, Role::Displayable);
        }
    }

    #[test]
    fn lowercase_jpeg_export_is_its_own_group() {
        // A6701135.JPG and A6701135_Export.jpg differ in stem, so they must not merge.
        let groups = group_files(
            Path::new("/d"),
            [p("/d/A6701135.JPG"), p("/d/A6701135_Export.jpg")],
        );
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].stem, "A6701135");
        assert_eq!(groups[1].stem, "A6701135_Export");
    }

    #[test]
    fn names_with_spaces_and_many_dots() {
        let (stem, ext) = split_name("my photo.v2.final.JPG");
        assert_eq!(stem, "my photo");
        assert_eq!(ext, "jpg");
        // Grouped together with its sidecar despite the messy name.
        let groups = group_files(
            Path::new("/d"),
            [p("/d/my photo.v2.final.JPG"), p("/d/my photo.xmp")],
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].members.len(), 2);
    }

    #[test]
    fn kind_label_distinguishes_jpg_from_pairs() {
        let pair = group_files(Path::new("/d"), [p("/d/a.JPG"), p("/d/a.ARW")]);
        assert_eq!(pair[0].kind(), "JPG+ARW");

        let jpg = group_files(Path::new("/d"), [p("/d/b.JPG")]);
        assert_eq!(jpg[0].kind(), "JPG");

        let raw = group_files(Path::new("/d"), [p("/d/c.ARW")]);
        assert_eq!(raw[0].kind(), "ARW");

        // Sidecars alone do not make a kind, and such groups are dropped anyway.
        assert!(group_files(Path::new("/d"), [p("/d/d.pp3")]).is_empty());
    }

    #[test]
    fn has_raw_and_has_jpeg_cover_both_extensions() {
        let g = group_files(Path::new("/d"), [p("/d/a.jpeg"), p("/d/a.ARW")]);
        assert!(g[0].has_jpeg() && g[0].has_raw());
        assert_eq!(g[0].kind(), "JPG+ARW");
    }

    #[test]
    fn size_formatting_covers_the_useful_range() {
        assert_eq!(format_size(0), "-");
        assert_eq!(format_size(5_000), "5 kB");
        assert_eq!(format_size(11_607_851), "11.6 MB");
        assert_eq!(format_size(43_405_099), "43.4 MB");
        assert_eq!(format_size(2_500_000_000), "2.50 GB");
    }

    #[test]
    fn time_formatting_matches_known_instants() {
        use std::time::{Duration, UNIX_EPOCH};
        let at = |s: u64| format_time(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(at(0), "1970-01-01 00:00");
        assert_eq!(at(946_684_800), "2000-01-01 00:00");
        // A leap day, which a naive conversion would get wrong.
        assert_eq!(at(1_583_020_800), "2020-03-01 00:00");
        assert_eq!(at(1_582_934_400), "2020-02-29 00:00");
        assert_eq!(at(1_724_328_000), "2024-08-22 12:00");
        // Pre-epoch is not worth handling and must not panic.
        assert_eq!(
            format_time(UNIX_EPOCH - Duration::from_secs(10)),
            "-"
        );
    }

    #[test]
    fn group_bytes_sums_every_member() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path();
        std::fs::write(dir.join("x.JPG"), vec![0u8; 1000]).unwrap();
        std::fs::write(dir.join("x.ARW"), vec![0u8; 2500]).unwrap();
        let g = group_files(dir, [dir.join("x.JPG"), dir.join("x.ARW")]);

        let store = MetaStore::new();
        let paths: Vec<PathBuf> = g[0].member_paths().cloned().collect();
        store.fill(&paths);

        assert_eq!(store.group_bytes(&g[0]), Some(3500));
        assert!(store.group_modified(&g[0]).is_some());
    }

    /// Scanning must not touch the filesystem beyond `read_dir`, or startup is
    /// proportional to the number of files rather than to the number of directories.
    /// This is what made a cold launch take ~3 s on a large library (R23).
    #[test]
    fn grouping_reports_nothing_until_the_meta_pass_runs() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path();
        std::fs::write(dir.join("x.JPG"), vec![0u8; 1000]).unwrap();
        let g = group_files(dir, [dir.join("x.JPG")]);

        let store = MetaStore::new();
        assert!(store.is_empty(), "grouping must not have stat'ed anything");
        assert_eq!(store.group_bytes(&g[0]), None, "unknown, not a wrong zero");
        assert_eq!(store.group_modified(&g[0]), None);

        store.fill(&[dir.join("x.JPG")]);
        assert_eq!(store.group_bytes(&g[0]), Some(1000));
    }

    #[test]
    fn missing_file_is_stat_ed_as_zero_not_a_panic() {
        // R11: stat fails for a path that vanished; must not panic or poison the group.
        let g = group_files(Path::new("/nonexistent"), [p("/nonexistent/z.JPG")]);
        let store = MetaStore::new();
        store.fill(&[p("/nonexistent/z.JPG")]);

        assert_eq!(store.group_bytes(&g[0]), Some(0));
        assert!(store.group_modified(&g[0]).is_none());
        assert_eq!(format_size(0), "-");
    }

    #[test]
    fn display_preference_is_jpg_then_jpeg_then_arw() {
        let groups = group_files(
            Path::new("/d"),
            [p("/d/x.arw"), p("/d/x.jpeg"), p("/d/x.jpg")],
        );
        assert_eq!(groups[0].display_path().unwrap(), Path::new("/d/x.jpg"));
    }
}
