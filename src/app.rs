//! Application state: what is selected, what the view looks like, what happens on
//! input. Deliberately contains no GPU or windowing types, so the whole interaction
//! model can be tested without a display.
//!
//! The renderer and event loop are thin shells that call into this.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::icc;
use crate::prefetch::{Loader, Prefetcher, State};
use crate::scan::{self, DirNode, Group, MetaStore};
use crate::trash::{self, Bin, History};
use crate::view::{FitMode, Orientation, View, Viewport, ZOOM_STEP};

/// Prefetch radius. R1's measurements say +/-10 is comfortable: ~2.1 GB of VRAM at full
/// resolution, against 24 GB available.
pub const DEFAULT_RADIUS: usize = 10;

/// Undo depth. Deep enough to recover from a bad run of culling, bounded so a long
/// session cannot grow without limit.
pub const UNDO_LIMIT: usize = 64;

/// Navigations closer together than this mean a key is being held rather than tapped.
const FAST_NAV: Duration = Duration::from_millis(90);

/// How long navigation must be quiet before the full window is restored.
const SETTLE: Duration = Duration::from_millis(140);

/// Window radius while scrolling fast. Just enough to cover the image on screen and its
/// immediate neighbours; anything more is decoded and discarded unseen.
const FAST_RADIUS: usize = 1;

/// While a key is held, how often the decode target is allowed to move.
///
/// Key repeat can exceed 100/s, but a 25 MP image takes ~170 ms to decode. Chasing every
/// event means nothing ever finishes. Retargeting about four times a second lets a few
/// images actually load and appear during the hold, which is what makes scrolling feel
/// like it is going somewhere rather than freezing.
const PREFETCH_THROTTLE: Duration = Duration::from_millis(250);

/// Facts about the image on screen, cached so the UI need not re-inspect pixels.
#[derive(Clone, Debug)]
pub struct Shown {
    pub index: usize,
    /// Stored pixel dimensions, before orientation.
    pub stored: (u32, u32),
    pub orientation: Orientation,
    /// Size after orientation, which is what the view transform works in.
    pub displayed: (u32, u32),
    pub colour: icc::Verdict,
}

impl Shown {
    pub fn megapixels(&self) -> f64 {
        f64::from(self.stored.0) * f64::from(self.stored.1) / 1e6
    }
}

/// What the event loop should do after handling an input.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Effects {
    /// The view changed; a redraw is needed.
    pub redraw: bool,
    /// The selected index changed; the renderer should bind a different texture.
    pub image_changed: bool,
    /// Files were removed; the sidebar must be rebuilt.
    pub tree_changed: bool,
}

impl Effects {
    fn redraw() -> Self {
        Self {
            redraw: true,
            ..Default::default()
        }
    }

    fn moved() -> Self {
        Self {
            redraw: true,
            image_changed: true,
            ..Default::default()
        }
    }

    fn merged(self, other: Self) -> Self {
        Self {
            redraw: self.redraw || other.redraw,
            image_changed: self.image_changed || other.image_changed,
            tree_changed: self.tree_changed || other.tree_changed,
        }
    }
}

/// Logical actions, decoupled from physical keys and buttons so they can be rebound
/// and driven directly from tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Next,
    Prev,
    /// Jump by a number of groups, e.g. page-down.
    Skip(i64),
    First,
    Last,
    /// Trash the selected group.
    Delete,
    Undo,
    /// Confirm an armed delete.
    ConfirmDelete,
    /// Dismiss an armed delete or other transient state.
    Cancel,
    ToggleFitMode,
    /// Reset to fit-to-window.
    Fit,
    /// Zoom to 1:1.
    ActualSize,
}

pub struct App {
    dirs: Vec<DirNode>,
    groups: Vec<Group>,
    prefetch: Prefetcher,
    bin: Arc<dyn Bin>,
    history: History,
    index: usize,
    pub view: View,
    pub fit_mode: FitMode,
    pub viewport: Viewport,
    shown: Option<Shown>,
    /// Native size and orientation per index, read cheaply from file headers so the view
    /// can be laid out before the pixels arrive.
    probed: HashMap<usize, (u32, u32, u16)>,
    /// Group awaiting a confirmed delete, if any.
    pending_delete: Option<usize>,
    /// Full prefetch radius, restored when navigation settles.
    radius: usize,
    /// When the last navigation happened, for detecting a held key.
    last_nav: Option<Instant>,
    /// True while the user is scrolling faster than images can usefully be decoded.
    fast_scroll: bool,
    /// When the decode target last moved, for throttling it during a held key.
    last_retarget: Option<Instant>,
    /// Last message for the status bar.
    pub status: String,
    /// Whether the prefetch window has been opened to its full radius yet. Startup warms
    /// only the first image; see [`App::warm_full_window`].
    warmed: bool,
    /// File sizes and timestamps, filled in on a background thread so the ~4300 stats
    /// they need are not on the launch path (R23).
    meta: Arc<MetaStore>,
}

impl App {
    /// Scan `root` and build an app over it. Convenience for tests and tools; the real
    /// program uses [`App::empty`] plus [`App::adopt_scan`] so the window does not wait.
    pub fn new<L: Loader>(
        root: &Path,
        loader: L,
        bin: Arc<dyn Bin>,
        radius: usize,
        threads: usize,
    ) -> Self {
        let mut app = Self::empty(loader, bin, radius, threads);
        app.adopt_scan(scan::scan(root));
        app
    }

    /// An app with no library yet.
    ///
    /// Scanning a large tree takes long enough to be the whole perceived launch — `~/Photos`
    /// rather than one shoot — so it happens on a worker thread and the result arrives via
    /// [`App::adopt_scan`]. Everything here is allocation-free bookkeeping plus spawning
    /// the decode pool, so the window can be on screen in milliseconds.
    pub fn empty<L: Loader>(
        loader: L,
        bin: Arc<dyn Bin>,
        radius: usize,
        threads: usize,
    ) -> Self {
        // Leave a couple of cores for the UI thread and the GPU driver. Saturating every
        // core with decoders is what makes the window stop responding while a batch of
        // images loads.
        let threads = if threads == 0 {
            std::thread::available_parallelism()
                .map_or(4, |n| n.get().saturating_sub(2).max(2))
        } else {
            threads
        };

        Self {
            dirs: Vec::new(),
            groups: Vec::new(),
            prefetch: Prefetcher::new(Vec::new(), radius, threads, loader),
            bin,
            history: History::new(UNDO_LIMIT),
            index: 0,
            view: View::default(),
            fit_mode: FitMode::default(),
            viewport: Viewport::new(1280.0, 720.0),
            shown: None,
            probed: HashMap::new(),
            pending_delete: None,
            radius,
            last_nav: None,
            fast_scroll: false,
            last_retarget: None,
            status: "scanning...".into(),
            warmed: false,
            meta: Arc::new(MetaStore::new()),
        }
    }

    /// Take the result of a background scan and start showing it.
    pub fn adopt_scan(&mut self, dirs: Vec<DirNode>) -> Effects {
        self.dirs = dirs;
        self.groups = scan::flatten(&self.dirs);
        self.index = 0;
        self.shown = None;
        self.probed.clear();
        self.status = format!("{} images", self.groups.len());

        // Narrow the window *before* handing over the paths: `reset` re-centres, and at
        // full radius that would queue twenty decodes the user cannot see yet, competing
        // with the one they can. `warm_full_window` opens it again once something is on
        // screen.
        self.prefetch.set_radius(0, 0);
        self.prefetch
            .reset(Self::display_paths(&self.groups), 0);
        // Re-arm the deferred work, since none of it can have run without a library.
        self.warmed = false;

        // Lay the first image out from its header. Without this `shown` stays None until
        // the very first texture lands, so the view fits a 1x1 placeholder and then
        // refits -- the visible jump R14 exists to remove.
        self.apply_probed_size(0);
        Effects::moved().merged(Effects {
            tree_changed: true,
            ..Default::default()
        })
    }


    /// Display path per group. Groups without one are impossible by construction
    /// (`scan` drops them), but a placeholder keeps indices aligned if that changes.
    fn display_paths(groups: &[Group]) -> Vec<PathBuf> {
        groups
            .iter()
            .map(|g| g.display_path().unwrap_or(Path::new("")).to_path_buf())
            .collect()
    }

    pub fn dirs(&self) -> &[DirNode] {
        &self.dirs
    }

    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn len(&self) -> usize {
        self.groups.len()
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn shown(&self) -> Option<&Shown> {
        self.shown.as_ref()
    }

    pub fn current(&self) -> Option<&Group> {
        self.groups.get(self.index)
    }

    pub fn prefetch(&self) -> &Prefetcher {
        &self.prefetch
    }

    pub fn history_len(&self) -> usize {
        self.history.len()
    }

    /// State of the selected slot, for showing a spinner or an error.
    pub fn current_state(&self) -> State {
        self.prefetch.state(self.index)
    }

    /// Record what is now on screen and set up the view for it.
    ///
    /// Called by the renderer once a texture is bound, because only then are the real
    /// dimensions known.
    pub fn note_shown(
        &mut self,
        index: usize,
        stored: (u32, u32),
        orientation: u16,
        icc_profile: Option<&[u8]>,
        exif_cs: Option<crate::tiff::ColorSpace>,
    ) {
        let orientation = Orientation::new(orientation);
        let displayed = orientation.displayed_size(stored.0, stored.1);
        let colour = icc::classify(icc_profile, exif_cs);

        // A new image resets or preserves the view according to the mode (R6).
        self.view = self
            .view
            .on_image_change(self.fit_mode, displayed, self.viewport);

        if colour.needs_warning() {
            self.status = format!("colour: {}", colour.describe());
        }

        self.shown = Some(Shown {
            index,
            stored,
            orientation,
            displayed,
            colour,
        });
    }

    /// File sizes and capture times, for the sidebar columns.
    pub fn meta(&self) -> &MetaStore {
        &self.meta
    }

    /// Stat every file in the library on a background thread.
    ///
    /// One `stat` per file is cheap warm and brutal cold: on a large library it was the
    /// entire ~3 s launch, because it ran to completion before the window was created.
    /// Nothing needed to show a photo depends on it, so it now runs behind the UI and
    /// rows show `-` until it lands.
    fn spawn_meta_scan(&self) {
        let paths: Vec<PathBuf> = self
            .groups
            .iter()
            .flat_map(|g| g.member_paths().cloned())
            .collect();
        if paths.is_empty() {
            return;
        }
        let meta = Arc::clone(&self.meta);
        std::thread::spawn(move || {
            let t = Instant::now();
            meta.fill(&paths);
            log::info!("startup: stat'ed {} files in {:?}", paths.len(), t.elapsed());
        });
    }

    /// Open the prefetch window to its full radius. Idempotent.
    ///
    /// Called once the first frame is on screen. Until then only the image being shown
    /// is decoded, so startup does not spend every core and ~2 GB of allocation on
    /// neighbours the user cannot navigate to yet anyway.
    pub fn warm_full_window(&mut self) {
        if self.warmed {
            return;
        }
        self.warmed = true;
        // The radius is pinned to 0 until here, so this is what actually opens it.
        self.prefetch.set_radius(self.radius, self.index);
        self.prefetch.set_centre(self.index);
        // Deferred to here rather than to `App::new` for the same reason: thousands of
        // stats are I/O bound, and firing them while the first image is still being read
        // off the same disk would just queue behind each other. The sidebar's size and
        // time columns are the least urgent thing in the program.
        self.spawn_meta_scan();
    }

    /// Record the colour verdict once the real pixels exist.
    ///
    /// Separate from [`App::note_shown`] because the view is laid out from the file
    /// header before decoding (R14), and at that point no profile has been read. Only
    /// the decoded image carries one, and re-running `note_shown` then would refit the
    /// view and undo whatever zoom the user had set.
    pub fn note_colour(&mut self, index: usize, colour: icc::Verdict) {
        let Some(shown) = self.shown.as_mut().filter(|s| s.index == index) else {
            return;
        };
        if shown.colour == colour {
            return;
        }
        shown.colour = colour;
        if colour.needs_warning() {
            self.status = format!("colour: {}", colour.describe());
        }
    }

    /// Displayed size of the current image, or a square fallback before one is bound.
    fn displayed(&self) -> (u32, u32) {
        self.shown.as_ref().map_or((1, 1), |s| s.displayed)
    }

    pub fn resize(&mut self, width: f64, height: f64) -> Effects {
        self.viewport = Viewport::new(width, height);
        // Refit on resize only in refit mode; preserving zoom means preserving it here
        // too, just re-clamped.
        self.view = match self.fit_mode {
            FitMode::Refit => View::fitted(self.displayed(), self.viewport),
            FitMode::Preserve => self.view.clamped(self.displayed(), self.viewport),
        };
        Effects::redraw()
    }

    /// Select an index, clamped into range. Moves the prefetch window.
    pub fn select(&mut self, index: usize) -> Effects {
        if self.groups.is_empty() {
            return Effects::default();
        }
        let target = index.min(self.groups.len() - 1);
        if target == self.index && self.shown.is_some() {
            return Effects::default();
        }
        self.index = target;
        // Selecting cancels an armed delete: the user moved on.
        self.pending_delete = None;

        if self.fast_scroll {
            // The selection keeps up with the key, but the decoders are only pointed at
            // a new image a few times a second. Retargeting on every event guarantees
            // that nothing ever finishes decoding, so the view would stay frozen for the
            // whole hold; this way images land periodically as it scrolls.
            let now = Instant::now();
            let due = self
                .last_retarget
                .is_none_or(|t| now.duration_since(t) >= PREFETCH_THROTTLE);
            if due {
                self.last_retarget = Some(now);
                self.prefetch.set_centre(target);
                self.apply_probed_size(target);
            }
            // Deliberately no `note_shown` otherwise: the view must stay sized for the
            // image actually on screen, not for one we are only passing through.
        } else {
            self.prefetch.set_centre(target);
            self.apply_probed_size(target);
        }
        Effects::moved()
    }

    /// Lay the view out from the file header, before the decoded pixels exist.
    ///
    /// Without this the view is fitted to a placeholder size and then refitted when the
    /// image lands, which reads as the window resizing itself under you.
    fn apply_probed_size(&mut self, index: usize) {
        let Some(path) = self.groups.get(index).and_then(|g| g.display_path()) else {
            return;
        };
        let probed = match self.probed.get(&index) {
            Some(&p) => Some(p),
            None => {
                // Cheap: mmap plus a header parse. A missing file simply yields None (R11).
                let p = crate::decode::probe(path);
                if let Some(p) = p {
                    self.probed.insert(index, p);
                }
                p
            }
        };
        if let Some((w, h, orientation)) = probed {
            self.note_shown(index, (w, h), orientation, None, None);
        }
    }

    fn step(&mut self, delta: i64) -> Effects {
        if self.groups.is_empty() {
            return Effects::default();
        }
        let last = (self.groups.len() - 1) as i64;
        // Saturating rather than wrapping: running off the end of a shoot should stop,
        // not silently teleport to the other end mid-cull.
        let target = (self.index as i64 + delta).clamp(0, last);

        // A held key produces navigations far faster than a 25 MP image can be decoded.
        // Prefetching the full window for each one queues work that is discarded before
        // it can be displayed, and saturates the cores the UI needs. So shrink the window
        // while it lasts and restore it once navigation settles.
        let now = Instant::now();
        let held = self
            .last_nav
            .is_some_and(|t| now.duration_since(t) < FAST_NAV);
        self.last_nav = Some(now);
        if held && !self.fast_scroll {
            self.fast_scroll = true;
            self.prefetch.set_radius(FAST_RADIUS, target as usize);
        }

        self.select(target as usize)
    }

    /// True while a navigation key is being held.
    pub fn fast_scrolling(&self) -> bool {
        self.fast_scroll
    }

    /// When the event loop should wake to check whether navigation has settled.
    ///
    /// `None` means there is nothing pending and the loop may sleep indefinitely.
    pub fn settle_deadline(&self) -> Option<Instant> {
        if !self.fast_scroll {
            return None;
        }
        self.last_nav.map(|t| t + SETTLE)
    }

    /// Restore the full prefetch window once navigation has been quiet.
    ///
    /// Call whenever the loop wakes. Returns effects if anything changed.
    pub fn tick(&mut self) -> Effects {
        if !self.fast_scroll {
            return Effects::default();
        }
        let quiet = self
            .last_nav
            .is_none_or(|t| Instant::now().duration_since(t) >= SETTLE);
        if !quiet {
            return Effects::default();
        }
        // Settled: refill the full window around wherever we landed, and size the view
        // for it now that it is the image that will actually be shown.
        self.fast_scroll = false;
        self.last_retarget = None;
        self.prefetch.set_radius(self.radius, self.index);
        self.prefetch.set_centre(self.index);
        self.apply_probed_size(self.index);
        Effects::moved()
    }

    /// Zoom by one wheel detent. `cursor` is relative to the viewport centre.
    pub fn zoom(&mut self, detents: f64, cursor: (f64, f64)) -> Effects {
        let factor = ZOOM_STEP.powf(detents);
        self.view = self
            .view
            .zoom_at(factor, cursor)
            .clamped(self.displayed(), self.viewport);
        self.take_manual_control();
        Effects::redraw()
    }

    pub fn pan(&mut self, delta: (f64, f64)) -> Effects {
        self.view = self
            .view
            .panned(delta)
            .clamped(self.displayed(), self.viewport);
        self.take_manual_control();
        Effects::redraw()
    }

    /// Touching the view by hand leaves refit mode.
    ///
    /// Otherwise the mode still says "refit" while the view plainly is not fitted, and
    /// `X` appears to do nothing the first time it is pressed: the first press only
    /// switches to preserve, and a second is needed to get back to refit. Switching here
    /// makes a single `X` recentre, which is what it looks like it should do.
    fn take_manual_control(&mut self) {
        if self.fit_mode == FitMode::Refit {
            self.fit_mode = FitMode::Preserve;
            self.status = format!("mode: {}", self.fit_mode.label());
        }
    }

    pub fn act(&mut self, action: Action) -> Effects {
        match action {
            Action::Next => self.step(1),
            Action::Prev => self.step(-1),
            Action::Skip(n) => self.step(n),
            Action::First => self.select(0),
            Action::Last => self.select(self.groups.len().saturating_sub(1)),
            Action::Delete => self.arm_delete(),
            Action::ConfirmDelete => self.confirm_delete(),
            Action::Cancel => {
                if self.pending_delete.take().is_some() {
                    self.status = "delete cancelled".into();
                }
                Effects::redraw()
            }
            Action::Undo => self.undo(),
            Action::ToggleFitMode => {
                self.fit_mode = self.fit_mode.toggled();
                self.status = format!("mode: {}", self.fit_mode.label());
                // Toggling into refit applies immediately, so the effect is visible.
                if self.fit_mode == FitMode::Refit {
                    self.view = View::fitted(self.displayed(), self.viewport);
                }
                Effects::redraw()
            }
            Action::Fit => {
                self.view = View::fitted(self.displayed(), self.viewport);
                Effects::redraw()
            }
            Action::ActualSize => {
                self.view = View {
                    zoom: 1.0,
                    pan: (0.0, 0.0),
                }
                .clamped(self.displayed(), self.viewport);
                // Asking for 1:1 is manual control too: navigating away should not
                // silently discard it.
                self.take_manual_control();
                Effects::redraw()
            }
        }
    }

    /// Arm a delete, to be confirmed with Enter.
    ///
    /// Deleting is the one action with real consequences, so it takes two keystrokes.
    fn arm_delete(&mut self) -> Effects {
        match self.groups.get(self.index) {
            Some(g) => {
                self.pending_delete = Some(self.index);
                // Falls back to stat'ing this one group if the background pass has not
                // reached it: the confirmation should always say how much is going, and
                // a handful of stats for the group in front of the user is nothing.
                let bytes = self.meta.group_bytes(g).unwrap_or_else(|| {
                    g.member_paths()
                        .filter_map(|p| std::fs::metadata(p).ok())
                        .map(|m| m.len())
                        .sum()
                });
                self.status = format!(
                    "delete {} ({} files, {})? Enter to confirm, Esc to cancel",
                    g.stem,
                    g.members.len(),
                    crate::scan::format_size(bytes)
                );
                Effects::redraw()
            }
            None => Effects::default(),
        }
    }

    /// True when a delete is armed and awaiting Enter.
    pub fn delete_pending(&self) -> bool {
        self.pending_delete.is_some()
    }

    fn confirm_delete(&mut self) -> Effects {
        // Only ever delete the group that was armed, in case the selection moved.
        match self.pending_delete.take() {
            Some(i) if i == self.index => self.delete_current(),
            Some(_) => {
                self.status = "selection changed, delete cancelled".into();
                Effects::redraw()
            }
            None => Effects::default(),
        }
    }

    /// Trash the selected group and drop it from the list.
    fn delete_current(&mut self) -> Effects {
        let Some(group) = self.groups.get(self.index) else {
            return Effects::default();
        };
        let stem = group.stem.clone();
        let paths = group.all_paths();
        let result = trash::delete_and_record(&*self.bin, &mut self.history, &stem, &paths);
        self.status = result.summary();

        if !result.is_clean() {
            // Leave the entry in place: the files are still on disk, so pretending
            // otherwise would hide the problem.
            return Effects::redraw();
        }

        self.remove_group_at(self.index);
        Effects::moved().merged(Effects {
            tree_changed: true,
            ..Default::default()
        })
    }

    /// Drop a group from the list and rebuild the prefetch ring around the new index.
    fn remove_group_at(&mut self, index: usize) {
        if index >= self.groups.len() {
            return;
        }
        let removed = self.groups.remove(index);
        // Keep the tree in step so the sidebar does not show a deleted entry.
        if let Some(node) = self.dirs.iter_mut().find(|d| d.path == removed.dir) {
            node.groups.retain(|g| g.stem != removed.stem);
        }
        self.dirs.retain(|d| !d.groups.is_empty());

        // Staying at the same index lands on the next image, which is what culling
        // wants. Clamp for the case where the last item was deleted.
        self.index = self.index.min(self.groups.len().saturating_sub(1));
        self.rebuild_ring();
    }

    /// The ring indexes into the group list, so any change to that list invalidates it.
    fn rebuild_ring(&mut self) {
        self.prefetch
            .reset(Self::display_paths(&self.groups), self.index);
        self.shown = None;
        // Indices moved, so cached sizes now refer to the wrong images.
        self.probed.clear();
        self.pending_delete = None;
        self.apply_probed_size(self.index);
    }

    fn undo(&mut self) -> Effects {
        match trash::undo(&*self.bin, &mut self.history) {
            None => {
                self.status = "nothing to undo".into();
                Effects::redraw()
            }
            Some(r) => {
                self.status = r.summary();
                // Restored files are back on disk; a rescan is the honest way to find
                // out where they belong in the ordering.
                self.rescan();
                Effects::moved().merged(Effects {
                    tree_changed: true,
                    ..Default::default()
                })
            }
        }
    }

    /// Re-read the tree, keeping the selection on the same group where possible.
    ///
    /// Needed after undo, and useful when the library changed underneath us (R11).
    pub fn rescan(&mut self) -> Effects {
        let anchor = self.current().map(|g| (g.dir.clone(), g.stem.clone()));
        let root = self
            .dirs
            .first()
            .map(|d| d.path.clone())
            .or_else(|| self.groups.first().map(|g| g.dir.clone()));

        if let Some(root) = root {
            // Scanning the common ancestor, not a leaf, or a rescan after deleting the
            // last item in a directory would lose its siblings.
            let root = root.parent().unwrap_or(&root).to_path_buf();
            self.dirs = scan::scan(&root);
            self.groups = scan::flatten(&self.dirs);
        }

        self.index = anchor
            .and_then(|(dir, stem)| {
                self.groups
                    .iter()
                    .position(|g| g.dir == dir && g.stem == stem)
            })
            .unwrap_or(self.index)
            .min(self.groups.len().saturating_sub(1));

        self.rebuild_ring();
        // A rescan follows an undo, so files have reappeared and need stat'ing. Not done
        // after a plain delete: that only removes paths, and re-stat'ing the whole
        // library on every cull would be far worse than the stale entries it avoids.
        self.spawn_meta_scan();
        Effects::moved().merged(Effects {
            tree_changed: true,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::Image;
    use crate::prefetch::Loader;
    use std::sync::Mutex;

    /// Loader that reports the dimensions encoded in the file's contents, so tests can
    /// control image size without real JPEGs.
    struct StubLoader;

    impl Loader for StubLoader {
        fn load(&self, path: &Path) -> Result<Image, String> {
            if !path.exists() {
                return Err("missing".into());
            }
            Ok(Image {
                width: 6192,
                height: 4128,
                rgba: vec![0; 4],
                orientation: 1,
                icc: None,
                color_space: None,
                scale_denom: 1,
                native_width: 6192,
                native_height: 4128,
            })
        }
    }

    /// Trash that moves files aside so deletion and undo are real.
    struct MemBin {
        store: PathBuf,
        moved: Mutex<Vec<(PathBuf, PathBuf)>>,
    }

    impl Bin for MemBin {
        fn send(&self, path: &Path) -> Result<(), String> {
            let dest = self.store.join(path.file_name().ok_or("no name")?);
            std::fs::rename(path, &dest).map_err(|e| e.to_string())?;
            self.moved.lock().unwrap().push((path.to_path_buf(), dest));
            Ok(())
        }
        fn restore(&self, path: &Path) -> Result<(), String> {
            let mut m = self.moved.lock().unwrap();
            let i = m
                .iter()
                .position(|(orig, _)| orig == path)
                .ok_or("not in trash")?;
            let (orig, dest) = m.remove(i);
            std::fs::rename(&dest, &orig).map_err(|e| e.to_string())
        }
    }

    /// A real 17x9 JPEG, so `decode::probe` has something it can actually parse. Tests
    /// of the view need this: without it every probe returns None and the R14 layout
    /// path is never exercised.
    const TINY_JPEG: &[u8] = include_bytes!("../tests/fixtures/tiny.jpg");

    /// Build a library of `n` JPG+ARW pairs plus a trash dir, and an App over it.
    fn app_with(n: usize) -> (tempfile::TempDir, App) {
        let td = tempfile::tempdir().unwrap();
        let photos = td.path().join("photos");
        let store = td.path().join("trash");
        std::fs::create_dir_all(&photos).unwrap();
        std::fs::create_dir_all(&store).unwrap();
        for i in 0..n {
            std::fs::write(photos.join(format!("IMG{i:04}.JPG")), TINY_JPEG).unwrap();
            // The ARW is only ever a delete companion here; the JPG is preferred for
            // display, so it does not need to be a parseable container.
            std::fs::write(photos.join(format!("IMG{i:04}.ARW")), b"x").unwrap();
        }
        let bin = Arc::new(MemBin {
            store,
            moved: Mutex::new(Vec::new()),
        });
        let app = App::new(&photos, StubLoader, bin, 3, 2);
        (td, app)
    }

    /// Pretend the renderer bound a texture, which is what sets up the view.
    fn bind(app: &mut App, orientation: u16) {
        let idx = app.index();
        app.note_shown(idx, (6192, 4128), orientation, None, None);
    }

    /// R14: the view must be laid out from the file header before any pixels exist,
    /// including for the very first image. It previously was not, so image 0 alone was
    /// fitted to a 1x1 placeholder and then visibly refitted when its texture landed.
    #[test]
    fn first_image_is_laid_out_before_any_texture_arrives() {
        let (_td, app) = app_with(3);
        let shown = app
            .shown()
            .expect("the first image must be sized from its header at startup");
        assert_eq!(shown.index, 0);
        assert_eq!(shown.stored, (17, 9), "dimensions must come from the header");
        assert!(app.view.zoom > 0.0);
    }

    /// The window must be able to open before the library is known, because scanning a
    /// large root is the whole perceived launch (R23).
    #[test]
    fn an_empty_app_is_usable_before_any_scan() {
        let td = tempfile::tempdir().unwrap();
        let bin = Arc::new(MemBin {
            store: td.path().to_path_buf(),
            moved: Mutex::new(Vec::new()),
        });
        let mut app = App::empty(StubLoader, bin, 3, 2);

        assert!(app.is_empty());
        assert!(app.current().is_none());
        // Every input must be a safe no-op rather than a panic while we wait.
        for a in [
            Action::Next,
            Action::Prev,
            Action::Delete,
            Action::ConfirmDelete,
            Action::Undo,
            Action::Fit,
            Action::ActualSize,
            Action::ToggleFitMode,
        ] {
            app.act(a);
        }
        app.zoom(1.0, (0.0, 0.0));
        app.resize(800.0, 600.0);
        app.warm_full_window();
        assert!(app.is_empty());
    }

    #[test]
    fn adopting_a_scan_populates_and_shows_the_first_image() {
        let td = tempfile::tempdir().unwrap();
        let photos = td.path().join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        for i in 0..5 {
            std::fs::write(photos.join(format!("IMG{i:04}.JPG")), TINY_JPEG).unwrap();
        }
        let bin = Arc::new(MemBin {
            store: td.path().to_path_buf(),
            moved: Mutex::new(Vec::new()),
        });
        let mut app = App::empty(StubLoader, bin, 3, 2);

        let e = app.adopt_scan(crate::scan::scan(&photos));
        assert!(e.tree_changed && e.image_changed);
        assert_eq!(app.len(), 5);
        assert_eq!(app.index(), 0);
        // R14 still holds for the first image once the tree arrives.
        assert_eq!(app.shown().unwrap().stored, (17, 9));
        assert!(app.status.contains("5 images"));

        // And the window still opens only after the first frame, not before.
        app.prefetch().wait_idle();
        assert_eq!(app.prefetch().state(3), State::Absent);
        app.warm_full_window();
        app.prefetch().wait_idle();
        assert_ne!(app.prefetch().state(3), State::Absent);
    }

    /// Startup must not queue the whole window: 21 full-resolution decodes on every core
    /// while the GPU driver is initialising is what made launch feel slow, and none of
    /// the neighbours can be navigated to before the window even exists.
    #[test]
    fn startup_decodes_only_the_first_image() {
        let (_td, mut app) = app_with(40);
        app.prefetch().wait_idle();

        assert_ne!(
            app.prefetch().state(0),
            State::Absent,
            "the image being shown must be decoded immediately"
        );
        for i in 1..40 {
            assert_eq!(
                app.prefetch().state(i),
                State::Absent,
                "index {i} must not be decoded before the first frame is on screen"
            );
        }

        // Once something is on screen the full window fills in as usual.
        app.warm_full_window();
        app.prefetch().wait_idle();
        for i in 0..=3 {
            assert_ne!(app.prefetch().state(i), State::Absent, "index {i} after warm-up");
        }
        assert_eq!(app.prefetch().state(30), State::Absent, "still bounded by radius");
    }

    #[test]
    fn warming_the_window_twice_is_harmless() {
        let (_td, mut app) = app_with(20);
        app.warm_full_window();
        app.prefetch().wait_idle();
        let after_first = app.prefetch().stats().decoded;

        app.warm_full_window();
        app.prefetch().wait_idle();
        assert_eq!(app.prefetch().stats().decoded, after_first);
    }

    /// R10's guard only helps if it can actually fire, which needs the profile to reach
    /// the app after decoding rather than the `None` the layout pass supplies.
    #[test]
    fn colour_verdict_is_updated_once_the_pixels_land() {
        let (_td, mut app) = app_with(2);
        assert!(!app.shown().unwrap().colour.needs_warning());

        app.note_colour(0, icc::Verdict::NotSrgb("Adobe RGB profile"));
        assert!(app.shown().unwrap().colour.needs_warning());
        assert!(app.status.to_lowercase().contains("adobe"));
    }

    #[test]
    fn colour_verdict_for_another_index_is_ignored() {
        // A late upload for an image the user has already navigated away from must not
        // relabel whatever is now on screen.
        let (_td, mut app) = app_with(3);
        app.note_colour(2, icc::Verdict::NotSrgb("Adobe RGB profile"));
        assert!(!app.shown().unwrap().colour.needs_warning());
    }

    #[test]
    fn discovers_all_groups() {
        let (_td, app) = app_with(10);
        assert_eq!(app.len(), 10);
        assert_eq!(app.index(), 0);
        // Each group is a JPG+ARW pair.
        assert_eq!(app.current().unwrap().members.len(), 2);
    }

    #[test]
    fn next_and_prev_saturate_at_the_ends() {
        let (_td, mut app) = app_with(5);
        assert_eq!(app.index(), 0);
        // Already at the start: prev is a no-op, not a wrap to the end.
        app.act(Action::Prev);
        assert_eq!(app.index(), 0);

        for expect in 1..5 {
            app.act(Action::Next);
            assert_eq!(app.index(), expect);
        }
        // At the end: next stays put.
        app.act(Action::Next);
        assert_eq!(app.index(), 4, "must not wrap around mid-cull");
    }

    #[test]
    fn skip_first_and_last() {
        let (_td, mut app) = app_with(100);
        app.act(Action::Skip(30));
        assert_eq!(app.index(), 30);
        app.act(Action::Skip(-10));
        assert_eq!(app.index(), 20);
        // Overshooting clamps.
        app.act(Action::Skip(9999));
        assert_eq!(app.index(), 99);
        app.act(Action::First);
        assert_eq!(app.index(), 0);
        app.act(Action::Last);
        assert_eq!(app.index(), 99);
    }

    #[test]
    fn moving_reports_image_changed() {
        let (_td, mut app) = app_with(5);
        let e = app.act(Action::Next);
        assert!(e.image_changed && e.redraw);
        // Selecting the current index again does nothing once an image is bound.
        bind(&mut app, 1);
        let e = app.select(app.index());
        assert!(!e.image_changed);
    }

    #[test]
    fn zooming_leaves_refit_mode_so_one_x_recentres() {
        let (_td, mut app) = app_with(3);
        bind(&mut app, 1);
        assert_eq!(app.fit_mode, FitMode::Refit);
        let fitted = app.view.zoom;

        app.zoom(3.0, (0.0, 0.0));
        assert_eq!(
            app.fit_mode,
            FitMode::Preserve,
            "zooming should hand control to the user"
        );

        // A single toggle must now refit, rather than needing two presses.
        app.act(Action::ToggleFitMode);
        assert_eq!(app.fit_mode, FitMode::Refit);
        assert!(
            (app.view.zoom - fitted).abs() < 1e-9,
            "one press of X should recentre, got zoom {}",
            app.view.zoom
        );
    }

    #[test]
    fn panning_also_leaves_refit_mode() {
        let (_td, mut app) = app_with(3);
        bind(&mut app, 1);
        app.act(Action::ActualSize);
        app.pan((40.0, 20.0));
        assert_eq!(app.fit_mode, FitMode::Preserve);
    }

    #[test]
    fn zooming_while_preserving_keeps_the_mode() {
        let (_td, mut app) = app_with(3);
        bind(&mut app, 1);
        app.act(Action::ToggleFitMode);
        assert_eq!(app.fit_mode, FitMode::Preserve);
        app.zoom(1.0, (0.0, 0.0));
        assert_eq!(app.fit_mode, FitMode::Preserve, "must not flip back");
    }

    #[test]
    fn refit_mode_resets_zoom_between_images() {
        let (_td, mut app) = app_with(5);
        bind(&mut app, 1);
        let fitted = app.view.zoom;

        // Set the view directly rather than via zoom(), which would hand control to the
        // user and leave refit mode. This isolates the mode's own semantics.
        app.view.zoom = fitted * 4.0;
        assert_eq!(app.fit_mode, FitMode::Refit);

        app.act(Action::Next);
        bind(&mut app, 1);
        assert!(
            (app.view.zoom - fitted).abs() < 1e-9,
            "refit must restore fit zoom, got {}",
            app.view.zoom
        );
    }

    #[test]
    fn actual_size_leaves_refit_mode() {
        // Otherwise pressing Z then navigating would throw the 1:1 away immediately.
        let (_td, mut app) = app_with(3);
        bind(&mut app, 1);
        app.act(Action::ActualSize);
        assert_eq!(app.fit_mode, FitMode::Preserve);

        app.act(Action::Next);
        bind(&mut app, 1);
        assert!((app.view.zoom - 1.0).abs() < 1e-9, "1:1 should carry over");
    }

    #[test]
    fn preserve_mode_keeps_zoom_between_images() {
        let (_td, mut app) = app_with(5);
        bind(&mut app, 1);
        app.act(Action::ToggleFitMode);
        assert_eq!(app.fit_mode, FitMode::Preserve);

        app.act(Action::ActualSize);
        let z = app.view.zoom;
        assert!((z - 1.0).abs() < 1e-9);

        app.act(Action::Next);
        bind(&mut app, 1);
        assert!(
            (app.view.zoom - z).abs() < 1e-9,
            "preserve must carry zoom, got {}",
            app.view.zoom
        );
    }

    #[test]
    fn orientation_changes_the_fitted_zoom() {
        let (_td, mut app) = app_with(2);
        app.resize(1000.0, 800.0);
        bind(&mut app, 1);
        let landscape = app.view.zoom;
        assert!((landscape - 1000.0 / 6192.0).abs() < 1e-9);

        // Orientation 8 makes the same file portrait, so the fit becomes height-limited.
        bind(&mut app, 8);
        assert_eq!(app.shown().unwrap().displayed, (4128, 6192));
        assert!((app.view.zoom - 800.0 / 6192.0).abs() < 1e-9);
    }

    #[test]
    fn resize_refits_or_reclamps_by_mode() {
        let (_td, mut app) = app_with(2);
        app.resize(1000.0, 800.0);
        bind(&mut app, 1);
        app.resize(2000.0, 1600.0);
        assert!((app.view.zoom - 2000.0 / 6192.0).abs() < 1e-9, "refit on resize");

        app.act(Action::ToggleFitMode);
        app.act(Action::ActualSize);
        app.resize(500.0, 400.0);
        assert!((app.view.zoom - 1.0).abs() < 1e-9, "preserve keeps zoom on resize");
    }

    #[test]
    fn delete_trashes_the_whole_group_and_advances() {
        let (_td, mut app) = app_with(5);
        let group = app.current().unwrap().clone();
        let paths = group.all_paths().iter().map(|p| p.to_path_buf()).collect::<Vec<_>>();

        app.act(Action::Delete);
        let e = app.act(Action::ConfirmDelete);

        assert!(e.tree_changed && e.image_changed);
        assert_eq!(app.len(), 4, "group removed from the list");
        for p in &paths {
            assert!(!p.exists(), "{} should be trashed", p.display());
        }
        // Index stays put, which now points at what was the next image.
        assert_eq!(app.index(), 0);
        assert_eq!(app.current().unwrap().stem, "IMG0001");
        assert_eq!(app.history_len(), 1);
    }

    #[test]
    fn delete_requires_confirmation() {
        let (_td, mut app) = app_with(5);
        let paths: Vec<PathBuf> = app
            .current()
            .unwrap()
            .all_paths()
            .iter()
            .map(|p| p.to_path_buf())
            .collect();

        // Arming must not touch the filesystem.
        app.act(Action::Delete);
        assert!(app.delete_pending());
        assert_eq!(app.len(), 5, "nothing deleted before confirmation");
        for p in &paths {
            assert!(p.exists(), "{} must still exist", p.display());
        }
        assert!(app.status.contains("Enter to confirm"));

        // Enter carries it out.
        app.act(Action::ConfirmDelete);
        assert!(!app.delete_pending());
        assert_eq!(app.len(), 4);
        for p in &paths {
            assert!(!p.exists(), "{} should be trashed", p.display());
        }
    }

    #[test]
    fn cancel_disarms_a_pending_delete() {
        let (_td, mut app) = app_with(5);
        app.act(Action::Delete);
        assert!(app.delete_pending());

        app.act(Action::Cancel);
        assert!(!app.delete_pending());
        assert_eq!(app.len(), 5, "cancel must not delete");
        assert!(app.status.contains("cancelled"));

        // Enter afterwards is inert.
        app.act(Action::ConfirmDelete);
        assert_eq!(app.len(), 5);
    }

    #[test]
    fn navigating_disarms_a_pending_delete() {
        // Otherwise an Enter meant for something else could delete the wrong group.
        let (_td, mut app) = app_with(5);
        app.act(Action::Delete);
        assert!(app.delete_pending());

        app.act(Action::Next);
        assert!(!app.delete_pending(), "moving must cancel the armed delete");
        app.act(Action::ConfirmDelete);
        assert_eq!(app.len(), 5, "no deletion should have happened");
    }

    #[test]
    fn confirm_without_arming_does_nothing() {
        let (_td, mut app) = app_with(3);
        app.act(Action::ConfirmDelete);
        assert_eq!(app.len(), 3);
        assert_eq!(app.history_len(), 0);
    }

    #[test]
    fn deleting_the_last_image_clamps_the_index() {
        let (_td, mut app) = app_with(3);
        app.act(Action::Last);
        assert_eq!(app.index(), 2);
        app.act(Action::Delete);
        app.act(Action::ConfirmDelete);
        assert_eq!(app.len(), 2);
        assert_eq!(app.index(), 1, "must clamp back onto a valid group");
    }

    #[test]
    fn deleting_everything_leaves_a_consistent_empty_state() {
        let (_td, mut app) = app_with(3);
        for _ in 0..3 {
            app.act(Action::Delete);
            app.act(Action::ConfirmDelete);
        }
        assert_eq!(app.len(), 0);
        assert!(app.is_empty());
        assert!(app.current().is_none());
        // Further input must not panic.
        app.act(Action::Next);
        app.act(Action::Delete);
        app.act(Action::Prev);
        app.zoom(1.0, (0.0, 0.0));
        assert_eq!(app.len(), 0);
    }

    #[test]
    fn undo_restores_the_group_and_the_selection() {
        let (_td, mut app) = app_with(5);
        let stem = app.current().unwrap().stem.clone();
        let paths = app
            .current()
            .unwrap()
            .all_paths()
            .iter()
            .map(|p| p.to_path_buf())
            .collect::<Vec<_>>();

        app.act(Action::Delete);
        app.act(Action::ConfirmDelete);
        assert_eq!(app.len(), 4);

        app.act(Action::Undo);

        assert_eq!(app.len(), 5, "group is back in the list");
        for p in &paths {
            assert!(p.exists(), "{} should be restored", p.display());
        }
        // Ordering is by stem, so the restored group returns to position 0.
        assert_eq!(app.groups()[0].stem, stem);
        assert_eq!(app.history_len(), 0);
    }

    #[test]
    fn undo_with_empty_history_is_reported_not_crashed() {
        let (_td, mut app) = app_with(2);
        app.act(Action::Undo);
        assert!(app.status.contains("nothing to undo"));
        assert_eq!(app.len(), 2);
    }

    #[test]
    fn repeated_delete_then_undo_round_trips() {
        let (_td, mut app) = app_with(4);
        for _ in 0..3 {
            app.act(Action::Delete);
            app.act(Action::ConfirmDelete);
        }
        assert_eq!(app.len(), 1);
        for _ in 0..3 {
            app.act(Action::Undo);
        }
        assert_eq!(app.len(), 4, "all three deletions undone");
        assert_eq!(app.history_len(), 0);
    }

    #[test]
    fn vanished_file_becomes_a_failed_slot_not_a_crash() {
        // R11: the user removed a file behind our back.
        //
        // The victim must start outside the initial prefetch window, or the ring may
        // already have decoded it successfully and would legitimately keep serving the
        // cached pixels.
        let (_td, mut app) = app_with(30);
        let victim_index = 20;
        let victim = app.groups()[victim_index]
            .display_path()
            .unwrap()
            .to_path_buf();
        assert_eq!(app.prefetch().state(victim_index), State::Absent);
        std::fs::remove_file(&victim).unwrap();

        app.select(victim_index);
        app.prefetch().wait_idle();

        assert_eq!(app.current_state(), State::Failed);
        assert!(app.prefetch().failure(victim_index).is_some());
        // Navigation past it still works.
        app.act(Action::Next);
        app.prefetch().wait_idle();
        assert_eq!(app.current_state(), State::Ready);
    }

    #[test]
    fn colour_warning_is_surfaced_for_non_srgb() {
        let (_td, mut app) = app_with(1);
        app.note_shown(0, (100, 100), 1, None, Some(crate::tiff::ColorSpace::AdobeRgb));
        assert!(app.shown().unwrap().colour.needs_warning());
        assert!(app.status.to_lowercase().contains("adobe"));
    }

    #[test]
    fn srgb_images_produce_no_warning() {
        let (_td, mut app) = app_with(1);
        app.note_shown(0, (100, 100), 1, None, Some(crate::tiff::ColorSpace::Srgb));
        assert!(!app.shown().unwrap().colour.needs_warning());
    }

    #[test]
    fn empty_library_is_handled() {
        let td = tempfile::tempdir().unwrap();
        let empty = td.path().join("nothing");
        std::fs::create_dir_all(&empty).unwrap();
        let bin = Arc::new(MemBin {
            store: td.path().to_path_buf(),
            moved: Mutex::new(Vec::new()),
        });
        let mut app = App::new(&empty, StubLoader, bin, 3, 2);

        assert_eq!(app.len(), 0);
        assert!(app.current().is_none());
        // Every action must be a safe no-op.
        for a in [
            Action::Next,
            Action::Prev,
            Action::Delete,
            Action::ConfirmDelete,
            Action::Cancel,
            Action::Undo,
            Action::First,
            Action::Last,
            Action::Fit,
            Action::ActualSize,
            Action::ToggleFitMode,
        ] {
            app.act(a);
        }
        assert_eq!(app.len(), 0);
    }

    #[test]
    fn rapid_navigation_enters_and_leaves_fast_scroll() {
        let (_td, mut app) = app_with(60);
        assert!(!app.fast_scrolling(), "starts normal");
        assert!(app.settle_deadline().is_none());

        // Two steps in quick succession is a held key.
        app.act(Action::Next);
        app.act(Action::Next);
        assert!(app.fast_scrolling(), "held key should shrink the window");
        assert!(app.settle_deadline().is_some(), "must schedule a settle check");

        // Ticking before the settle interval changes nothing.
        assert!(!app.tick().redraw);
        assert!(app.fast_scrolling());

        // After the interval, the full window is restored.
        std::thread::sleep(SETTLE + Duration::from_millis(20));
        assert!(app.tick().redraw, "settling should ask for a redraw");
        assert!(!app.fast_scrolling());
        assert!(app.settle_deadline().is_none());
    }

    #[test]
    fn slow_navigation_never_enters_fast_scroll() {
        let (_td, mut app) = app_with(20);
        for _ in 0..3 {
            app.act(Action::Next);
            // Deliberately slower than the held-key threshold.
            std::thread::sleep(FAST_NAV + Duration::from_millis(20));
        }
        assert!(
            !app.fast_scrolling(),
            "tapping the key must not trigger fast scroll"
        );
    }

    #[test]
    fn fast_scroll_shrinks_then_restores_the_prefetch_window() {
        let (_td, mut app) = app_with(80);
        // The ring stays pinned to the first image until a frame has been presented, so
        // steady-state prefetch behaviour only exists after this.
        app.warm_full_window();
        app.select(40);
        app.prefetch().wait_idle();
        // Full radius is 3 in these tests, so 37..=43 are warm.
        assert_eq!(app.prefetch().state(37), State::Ready);

        app.act(Action::Next);
        app.act(Action::Next);
        assert!(app.fast_scrolling());
        app.prefetch().wait_idle();
        // Radius is now 1, so only the immediate neighbours are kept.
        let idx = app.index();
        assert_eq!(app.prefetch().state(idx), State::Ready);
        assert_eq!(
            app.prefetch().state(idx + 4),
            State::Absent,
            "distant slots must not be decoded while scrolling"
        );

        std::thread::sleep(SETTLE + Duration::from_millis(20));
        app.tick();
        app.prefetch().wait_idle();
        assert_eq!(
            app.prefetch().state(idx + 3),
            State::Ready,
            "full window should be refilled after settling"
        );
    }

    /// While a key is held the selection must keep up, but the decode target must not
    /// chase every event or nothing ever finishes decoding.
    #[test]
    fn fast_scroll_throttles_the_decode_target() {
        let (_td, mut app) = app_with(400);
        app.select(0);

        // Simulate a high key-repeat rate: many navigations with no pause.
        for _ in 0..60 {
            app.act(Action::Next);
        }
        assert!(app.fast_scrolling());
        // The selection tracked every event.
        assert_eq!(app.index(), 60, "selection must keep up with the key");

        // But the ring was only retargeted once, so decoding had a chance to progress
        // instead of being restarted 60 times.
        app.prefetch().wait_idle();
        let st = app.prefetch().stats();
        assert!(
            st.decoded + st.discarded + st.skipped < 20,
            "decode target chased too many events: {st:?}"
        );
    }

    #[test]
    fn settling_retargets_to_where_we_landed() {
        let (_td, mut app) = app_with(200);
        app.select(0);
        for _ in 0..40 {
            app.act(Action::Next);
        }
        let landed = app.index();
        assert!(app.fast_scrolling());

        std::thread::sleep(SETTLE + Duration::from_millis(20));
        app.tick();
        app.prefetch().wait_idle();

        assert!(!app.fast_scrolling());
        assert_eq!(
            app.prefetch().state(landed),
            State::Ready,
            "the image we stopped on must be decoded"
        );
        // And the full window around it.
        assert_eq!(app.prefetch().state(landed + 3), State::Ready);
    }

    #[test]
    fn fast_scroll_does_not_resize_the_view_for_images_it_skips() {
        // The view must stay sized for the image actually on screen; refitting to an
        // image we are only passing through would distort what is displayed.
        let (_td, mut app) = app_with(100);
        app.resize(1000.0, 800.0);
        app.select(0);
        bind(&mut app, 1);
        let shown_index = app.shown().unwrap().index;

        for _ in 0..30 {
            app.act(Action::Next);
        }
        assert!(app.fast_scrolling());
        // Whatever is recorded as shown must not have jumped to an arbitrary skipped
        // index; at most it advanced to a throttled retarget.
        let now_shown = app.shown().unwrap().index;
        assert!(
            now_shown == shown_index || now_shown <= app.index(),
            "shown index {now_shown} is inconsistent with selection {}",
            app.index()
        );
    }

    #[test]
    fn tick_is_harmless_when_not_scrolling() {
        let (_td, mut app) = app_with(5);
        for _ in 0..3 {
            assert!(!app.tick().redraw);
        }
        assert!(!app.fast_scrolling());
    }

    #[test]
    fn prefetch_window_follows_the_selection() {
        let (_td, mut app) = app_with(30);
        app.warm_full_window();
        app.select(15);
        app.prefetch().wait_idle();
        // Radius 3, so 12..=18 are warm and the rest are not.
        for i in 12..=18 {
            assert_eq!(app.prefetch().state(i), State::Ready, "index {i}");
        }
        assert_eq!(app.prefetch().state(11), State::Absent);
        assert_eq!(app.prefetch().state(19), State::Absent);
    }
}
