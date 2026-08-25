//! Prefetch ring: keeps a window of decoded images warm around the cursor.
//!
//! This is what makes navigation instant (REQUIREMENTS.md R1). Switching to an index
//! already in the window costs a texture bind; the decode happened while the user was
//! looking at a previous image.
//!
//! Deliberately simple, because the R1 benchmark said it can afford to be: full-res
//! decode runs at 40-90 img/s on the target machine against a human key-repeat rate, so
//! there is no need for directional bias or priority classes. What the ring *does* need
//! is correct cancellation and hard memory bounds.
//!
//! ## Cancellation
//!
//! A JPEG decode cannot be interrupted partway through, so "cancel" means two things:
//! never *start* a job whose index has left the window, and *discard* a finished image
//! whose index has left the window. Both checks happen under the lock, so a fast-moving
//! cursor cannot waste the pool on stale work for more than one decode per worker.
//!
//! ## Memory
//!
//! Pixels live in the ring only until the renderer collects them via [`take_ready`],
//! after which the slot holds no data. With radius 10 and 25.6 MP images the worst case
//! is bounded by in-flight decodes plus uncollected results, not by the window size.

use std::collections::{BinaryHeap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use crate::decode::{self, Image};

/// Produces pixels for a path. Abstracted so the ring can be tested without touching
/// the filesystem or spending 172 ms per fixture.
pub trait Loader: Send + Sync + 'static {
    fn load(&self, path: &Path) -> Result<Image, String>;
}

/// The real loader: maps the file and decodes at full resolution (see R1).
///
/// Holds the pool the decoded pixels are drawn from; the renderer returns each buffer
/// once it has finished uploading it.
pub struct FileLoader {
    pool: Arc<decode::BufferPool>,
}

impl FileLoader {
    pub fn new(pool: Arc<decode::BufferPool>) -> Self {
        Self { pool }
    }
}

impl Default for FileLoader {
    /// A loader with a pool of its own that retains nothing, so every decode allocates.
    /// Reuse only pays off when something hands the buffers back, which only the
    /// renderer does; tests and the benchmark bins have no such loop.
    fn default() -> Self {
        Self::new(Arc::new(decode::BufferPool::new(0)))
    }
}

impl Loader for FileLoader {
    fn load(&self, path: &Path) -> Result<Image, String> {
        // R11: a vanished file is an ordinary outcome, reported as a normal error.
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        // Mapped rather than read, because only part of the file is ever decoded: a
        // 32 MB ARW is displayed from a ~5 MB embedded preview (R9), so `fs::read`
        // would pull in ~27 MB of raw sensor data that is then thrown away, plus a
        // full-size heap copy. Only the pages the decoder actually touches are faulted
        // in. `decode::probe` already works this way.
        //
        // Safety: the mapping is read-only and dropped before returning. A file that is
        // *truncated* while mapped would fault; deletion is safe, and truncation of a
        // photo mid-session is not a case R11 describes.
        let map = unsafe { memmap2::Mmap::map(&file) }.map_err(|e| e.to_string())?;
        // Sized against the header inside the decoder; an empty buffer from a cold pool
        // simply causes a fresh allocation.
        decode::decode_reusing(&map, None, decode::Decoder::Zune, self.pool.take())
            .map_err(|e| e.to_string())
    }
}

/// Observable state of a slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// Not in the window, or evicted.
    Absent,
    /// In the window, waiting for a worker.
    Queued,
    /// A worker is decoding it now.
    Decoding,
    /// Decoded and waiting for the renderer to collect it.
    Ready,
    /// Collected by the renderer; the ring no longer holds pixels.
    Collected,
    /// Load or decode failed. Remembered so it is not retried in a loop (R11).
    Failed,
}

enum Slot {
    Queued,
    Decoding,
    Ready(Box<Image>),
    Collected,
    Failed(String),
}

impl Slot {
    fn state(&self) -> State {
        match self {
            Slot::Queued => State::Queued,
            Slot::Decoding => State::Decoding,
            Slot::Ready(_) => State::Ready,
            Slot::Collected => State::Collected,
            Slot::Failed(_) => State::Failed,
        }
    }
}

/// A queued decode, ordered so the nearest-to-cursor job is popped first.
#[derive(PartialEq, Eq)]
struct Job {
    /// Distance from the window centre at enqueue time.
    distance: usize,
    index: usize,
}

impl Ord for Job {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // BinaryHeap is a max-heap; invert so the smallest distance is greatest.
        other
            .distance
            .cmp(&self.distance)
            .then_with(|| other.index.cmp(&self.index))
    }
}

impl PartialOrd for Job {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

struct Shared {
    paths: Vec<PathBuf>,
    slots: HashMap<usize, Slot>,
    queue: BinaryHeap<Job>,
    centre: usize,
    radius: usize,
    /// One extra index kept warm outside the window, set when the user hovers a row in
    /// the sidebar. They are likely to click it, so decoding early makes the click
    /// instant. Bounded to a single slot so it cannot grow memory.
    hint: Option<usize>,
    stats: Stats,
}

/// Counters for tests and diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub decoded: usize,
    /// Finished decodes thrown away because the window had moved on.
    pub discarded: usize,
    /// Jobs skipped at dequeue because the window had already moved on.
    pub skipped: usize,
    pub failed: usize,
}

impl Shared {
    /// Inclusive window bounds, or `None` when there is nothing to show.
    ///
    /// Returns an `Option` rather than a clamped pair on purpose: for an empty
    /// collection any numeric answer is wrong, and an earlier version that returned
    /// `(0, 0)` here made index 0 look simultaneously in-range (to `set_centre`) and
    /// out-of-range (to `in_window`), which wedged a slot in `Queued` forever.
    fn window(&self) -> Option<(usize, usize)> {
        let last = self.paths.len().checked_sub(1)?;
        // Clamp the centre: after a rescan shrinks the list an index may be stale (R11).
        let centre = self.centre.min(last);
        Some((centre.saturating_sub(self.radius), (centre + self.radius).min(last)))
    }

    fn in_window(&self, index: usize) -> bool {
        // `hi <= last` already implies `index < paths.len()`.
        self.window()
            .is_some_and(|(lo, hi)| index >= lo && index <= hi)
    }

    /// Wanted means "inside the window, or the current hover hint".
    fn wanted(&self, index: usize) -> bool {
        self.in_window(index) || (self.hint == Some(index) && index < self.paths.len())
    }
}

/// Called from a worker thread whenever a decode finishes, so the event loop can wake
/// and collect it. Without this the UI has to poll, which spins the CPU and contends on
/// the ring's mutex with the very workers it is waiting for.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

pub struct Prefetcher {
    shared: Arc<(Mutex<Shared>, Condvar)>,
    shutdown: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
    waker: Arc<Mutex<Option<Waker>>>,
}

impl Prefetcher {
    /// Spawn a ring over `paths` with the given window radius.
    ///
    /// `threads` of zero means "use available parallelism".
    pub fn new<L: Loader>(paths: Vec<PathBuf>, radius: usize, threads: usize, loader: L) -> Self {
        let threads = if threads == 0 {
            thread::available_parallelism().map_or(4, |n| n.get())
        } else {
            threads
        };

        let shared = Arc::new((
            Mutex::new(Shared {
                paths,
                slots: HashMap::new(),
                queue: BinaryHeap::new(),
                centre: 0,
                radius,
                hint: None,
                stats: Stats::default(),
            }),
            Condvar::new(),
        ));
        let shutdown = Arc::new(AtomicBool::new(false));
        let loader = Arc::new(loader);
        let waker: Arc<Mutex<Option<Waker>>> = Arc::new(Mutex::new(None));

        let workers = (0..threads)
            .map(|_| {
                let shared = Arc::clone(&shared);
                let shutdown = Arc::clone(&shutdown);
                let loader = Arc::clone(&loader);
                let waker = Arc::clone(&waker);
                thread::spawn(move || worker(shared, shutdown, loader, waker))
            })
            .collect();

        Self {
            shared,
            shutdown,
            workers,
            waker,
        }
    }

    /// Install the callback invoked when a decode completes.
    pub fn set_waker(&self, waker: Waker) {
        *self.waker.lock().unwrap() = Some(waker);
    }

    /// Ask for `index` to be decoded soon without moving the window.
    ///
    /// Used when the pointer hovers a sidebar row: the user is likely to click it, so the
    /// image should already be resident by the time they do. Only one hint is kept, so
    /// sweeping the pointer down a long list does not queue unbounded work.
    pub fn hint(&self, index: usize) {
        let (lock, cvar) = &*self.shared;
        let mut s = lock.lock().unwrap();
        if s.hint == Some(index) || index >= s.paths.len() {
            return;
        }
        // Drop the previous hint unless the window wants it anyway.
        if let Some(old) = s.hint.take() {
            if !s.in_window(old) && !matches!(s.slots.get(&old), Some(Slot::Decoding)) {
                s.slots.remove(&old);
            }
        }
        s.hint = Some(index);
        if s.slots.contains_key(&index) {
            // Already decoded, decoding, or failed: nothing to schedule.
            return;
        }
        s.slots.insert(index, Slot::Queued);
        // Distance 0 so it outranks the rest of the window.
        s.queue.push(Job { distance: 0, index });
        drop(s);
        cvar.notify_all();
    }

    /// Clear the hover hint. The slot becomes evictable on the next window move.
    pub fn clear_hint(&self) {
        let (lock, _) = &*self.shared;
        lock.lock().unwrap().hint = None;
    }

    /// Change the window radius, re-centring on `centre`.
    ///
    /// Used to shrink the window while the user holds a navigation key: decoding the full
    /// window for every intermediate image is work that is thrown away before it can be
    /// seen, and it saturates the CPU that the UI needs.
    pub fn set_radius(&self, radius: usize, centre: usize) {
        {
            let (lock, _) = &*self.shared;
            let mut s = lock.lock().unwrap();
            if s.radius == radius {
                return;
            }
            s.radius = radius;
        }
        self.set_centre(centre);
    }

    /// Move the window. Enqueues newly covered indices and evicts those left behind.
    ///
    /// Cheap enough to call on every navigation keystroke.
    pub fn set_centre(&self, centre: usize) {
        let (lock, cvar) = &*self.shared;
        let mut s = lock.lock().unwrap();
        s.centre = centre;

        let Some((lo, hi)) = s.window() else {
            // Nothing to show. Clear everything so no slot can linger as `Queued`
            // with no job able to claim it.
            s.slots.clear();
            s.queue.clear();
            drop(s);
            cvar.notify_all();
            return;
        };

        // Evict anything outside the window so a long session cannot accumulate
        // entries without bound -- except slots being decoded right now. Keeping those
        // means the window leaving and returning cannot cause two workers to decode the
        // same index, which previously let the second finisher delete the first one's
        // result. An in-flight slot is cleaned up by its own worker.
        let hint = s.hint;
        s.slots.retain(|&i, slot| {
            (i >= lo && i <= hi) || matches!(slot, Slot::Decoding) || hint == Some(i)
        });

        // Rebuild the queue so ordering reflects the new distances. Stale heap entries
        // are filtered again at dequeue.
        s.queue.clear();
        for i in lo..=hi {
            let slot = s.slots.entry(i).or_insert(Slot::Queued);
            if matches!(slot, Slot::Queued) {
                s.queue.push(Job {
                    distance: i.abs_diff(centre),
                    index: i,
                });
            }
        }

        drop(s);
        cvar.notify_all();
    }

    /// Replace the path list and re-centre.
    ///
    /// Every slot is dropped, because indices refer to positions in the list and a
    /// deletion shifts everything after it: keeping old slots would show the wrong
    /// image. In-flight decodes are left to finish and discard themselves, since they
    /// can no longer own a slot.
    pub fn reset(&self, paths: Vec<PathBuf>, centre: usize) {
        {
            let (lock, _) = &*self.shared;
            let mut s = lock.lock().unwrap();
            s.paths = paths;
            s.slots.clear();
            s.queue.clear();
        }
        // Re-uses the normal path so enqueueing and notification stay in one place.
        self.set_centre(centre);
    }

    /// Collect every decoded image that the renderer has not yet taken.
    ///
    /// Slots move to [`State::Collected`] and release their pixels.
    pub fn take_ready(&self) -> Vec<(usize, Box<Image>)> {
        let (lock, _) = &*self.shared;
        let mut s = lock.lock().unwrap();

        let ready: Vec<usize> = s
            .slots
            .iter()
            .filter(|(_, slot)| matches!(slot, Slot::Ready(_)))
            .map(|(&i, _)| i)
            .collect();

        ready
            .into_iter()
            .filter_map(|i| match s.slots.insert(i, Slot::Collected) {
                Some(Slot::Ready(img)) => Some((i, img)),
                // Cannot happen: we just observed Ready under the same lock.
                other => {
                    if let Some(o) = other {
                        s.slots.insert(i, o);
                    }
                    None
                }
            })
            .collect()
    }

    /// Return an image collected by [`Prefetcher::take_ready`] but not yet uploaded.
    ///
    /// Lets the renderer bound how much it uploads per frame without discarding the
    /// pixels it chose to defer. Ignored if the slot has since left the window, because
    /// the image is then no longer wanted.
    pub fn put_back(&self, index: usize, image: Box<Image>) {
        let (lock, _) = &*self.shared;
        let mut s = lock.lock().unwrap();
        if s.wanted(index) && matches!(s.slots.get(&index), Some(Slot::Collected)) {
            s.slots.insert(index, Slot::Ready(image));
        }
    }

    /// Indices whose load failed, gathered under a single lock.
    ///
    /// The sidebar needs this per row, and a directory can hold hundreds of rows. Calling
    /// `state()` for each one took the ring's mutex hundreds of times per frame and
    /// starved the decode workers, so the UI takes one snapshot instead.
    pub fn failed_indices(&self) -> Vec<usize> {
        let (lock, _) = &*self.shared;
        let s = lock.lock().unwrap();
        s.slots
            .iter()
            .filter(|(_, slot)| matches!(slot, Slot::Failed(_)))
            .map(|(&i, _)| i)
            .collect()
    }

    /// Ensure `index` will be decoded again, even if it was already collected.
    ///
    /// The renderer takes ownership of pixels when it uploads them, so the ring keeps no
    /// copy. If the texture is later evicted the image would be unreachable: the ring
    /// thinks it is done and will not redo the work. This is the repair path, used when
    /// the renderer finds it has no texture for the current image.
    ///
    /// Queued at distance 0, since it is needed right now.
    pub fn require(&self, index: usize) {
        let (lock, cvar) = &*self.shared;
        let mut s = lock.lock().unwrap();
        if index >= s.paths.len() {
            return;
        }
        // Only redo work that is genuinely gone; never disturb an in-flight or ready slot.
        match s.slots.get(&index) {
            Some(Slot::Ready(_)) | Some(Slot::Queued) | Some(Slot::Decoding) => return,
            // A previous failure should be retried on an explicit request, since the
            // file may have reappeared.
            _ => {}
        }
        s.slots.insert(index, Slot::Queued);
        s.queue.push(Job { distance: 0, index });
        drop(s);
        cvar.notify_all();
    }

    pub fn state(&self, index: usize) -> State {
        let (lock, _) = &*self.shared;
        let s = lock.lock().unwrap();
        s.slots.get(&index).map_or(State::Absent, Slot::state)
    }

    /// Failure message for a slot, if it failed.
    pub fn failure(&self, index: usize) -> Option<String> {
        let (lock, _) = &*self.shared;
        let s = lock.lock().unwrap();
        match s.slots.get(&index) {
            Some(Slot::Failed(m)) => Some(m.clone()),
            _ => None,
        }
    }

    pub fn stats(&self) -> Stats {
        let (lock, _) = &*self.shared;
        lock.lock().unwrap().stats
    }

    /// Number of slots currently holding pixels. Used by tests to assert bounds.
    pub fn resident(&self) -> usize {
        let (lock, _) = &*self.shared;
        lock.lock()
            .unwrap()
            .slots
            .values()
            .filter(|s| matches!(s, Slot::Ready(_)))
            .count()
    }

    /// Block until nothing is queued or decoding. Test helper.
    pub fn wait_idle(&self) {
        let (lock, cvar) = &*self.shared;
        let mut s = lock.lock().unwrap();
        loop {
            let busy = s
                .slots
                .values()
                .any(|sl| matches!(sl, Slot::Queued | Slot::Decoding));
            if !busy {
                return;
            }
            s = cvar.wait(s).unwrap();
        }
    }
}

impl Drop for Prefetcher {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.shared.1.notify_all();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

fn worker<L: Loader>(
    shared: Arc<(Mutex<Shared>, Condvar)>,
    shutdown: Arc<AtomicBool>,
    loader: Arc<L>,
    waker: Arc<Mutex<Option<Waker>>>,
) {
    let (lock, cvar) = &*shared;

    loop {
        // --- Claim a job under the lock.
        let (index, path) = {
            let mut s = lock.lock().unwrap();
            loop {
                if shutdown.load(Ordering::SeqCst) {
                    return;
                }
                match s.queue.pop() {
                    Some(job) => {
                        // Stale: the window moved after this job was queued.
                        if !s.wanted(job.index) {
                            s.stats.skipped += 1;
                            continue;
                        }
                        // Another worker already took it, or it is already done.
                        if !matches!(s.slots.get(&job.index), Some(Slot::Queued)) {
                            continue;
                        }
                        let path = s.paths[job.index].clone();
                        s.slots.insert(job.index, Slot::Decoding);
                        break (job.index, path);
                    }
                    None => {
                        // Nothing to do; wake on new work or shutdown.
                        s = cvar.wait(s).unwrap();
                    }
                }
            }
        };

        // --- Decode without holding the lock, so navigation stays responsive.
        let result = loader.load(&path);

        // --- Publish, unless the window moved on meanwhile.
        {
            let mut s = lock.lock().unwrap();
            // Only touch the slot if it still holds *our* `Decoding` marker. If it has
            // become anything else, another worker has superseded us and owns it now;
            // overwriting or removing it would destroy their result.
            let owns = matches!(s.slots.get(&index), Some(Slot::Decoding));
            let wanted = owns && s.wanted(index);

            match result {
                Ok(img) if wanted => {
                    s.stats.decoded += 1;
                    s.slots.insert(index, Slot::Ready(Box::new(img)));
                }
                Err(msg) if wanted => {
                    // Remembered as failed so it is never retried in a loop (R11).
                    s.stats.failed += 1;
                    s.slots.insert(index, Slot::Failed(msg));
                }
                // Unwanted: the window moved on. Dropping the result here frees the
                // pixels immediately.
                other => {
                    if other.is_err() {
                        s.stats.failed += 1;
                    }
                    s.stats.discarded += 1;
                    if owns {
                        s.slots.remove(&index);
                    }
                }
            }
        }
        // Wake `wait_idle`, and any worker blocked on an empty queue.
        cvar.notify_all();
        // Wake the event loop so it collects the result promptly. Cloned out of the lock
        // first so the callback never runs while holding it.
        let cb = waker.lock().unwrap().clone();
        if let Some(cb) = cb {
            cb();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    fn tiny_image() -> Image {
        Image {
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 255],
            orientation: 1,
            icc: None,
            color_space: None,
            scale_denom: 1,
            native_width: 1,
            native_height: 1,
        }
    }

    fn paths(n: usize) -> Vec<PathBuf> {
        (0..n).map(|i| PathBuf::from(format!("/img/{i}.jpg"))).collect()
    }

    /// Succeeds for every path, counting calls.
    struct CountingLoader {
        calls: Arc<AtomicUsize>,
        delay: Duration,
    }

    impl Loader for CountingLoader {
        fn load(&self, _p: &Path) -> Result<Image, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                thread::sleep(self.delay);
            }
            Ok(tiny_image())
        }
    }

    /// Fails for paths whose index is in `missing`, mimicking R11 vanishing files.
    struct FlakyLoader {
        missing: Vec<usize>,
    }

    impl Loader for FlakyLoader {
        fn load(&self, p: &Path) -> Result<Image, String> {
            let stem = p.file_stem().unwrap().to_str().unwrap();
            let idx: usize = stem.parse().unwrap();
            if self.missing.contains(&idx) {
                Err("No such file or directory".into())
            } else {
                Ok(tiny_image())
            }
        }
    }

    /// A real JPEG on disk, small enough to keep in the tree. Exists so the *mapped*
    /// read path in `FileLoader` has coverage: the real-library suite skips wherever the
    /// photos are absent, which is everywhere except the user's machine.
    const TINY_JPEG: &[u8] = include_bytes!("../tests/fixtures/tiny.jpg");

    #[test]
    fn file_loader_decodes_a_mapped_file() {
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("tiny.jpg");
        std::fs::write(&p, TINY_JPEG).unwrap();

        let img = FileLoader::default().load(&p).expect("a valid JPEG must decode");
        assert_eq!((img.width, img.height), (17, 9));
        // RGBA8 straight out of the decoder, tightly packed (R1).
        assert_eq!(img.rgba.len(), 17 * 9 * 4);
        assert!(img.rgba.chunks_exact(4).all(|p| p[3] == 0xFF), "opaque alpha");
    }

    #[test]
    fn file_loader_reports_a_missing_file_rather_than_panicking() {
        // R11: the tree changes underneath us, so this is an ordinary outcome.
        let td = tempfile::tempdir().unwrap();
        let Err(err) = FileLoader::default().load(&td.path().join("gone.jpg")) else {
            panic!("a missing file must not load");
        };
        assert!(!err.is_empty(), "the failure must carry a message");
    }

    #[test]
    fn file_loader_rejects_an_empty_file() {
        // Zero-length files cannot be mapped on Linux; that must be an error, not a
        // panic, since a file can be created before it is written.
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("empty.jpg");
        std::fs::write(&p, b"").unwrap();
        assert!(FileLoader::default().load(&p).is_err(), "an empty file must not load");
    }

    #[test]
    fn fills_window_around_centre() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            paths(100),
            3,
            2,
            CountingLoader { calls: Arc::clone(&calls), delay: Duration::ZERO },
        );
        pf.set_centre(50);
        pf.wait_idle();

        // Window is 47..=53 inclusive: 7 slots.
        for i in 47..=53 {
            assert_eq!(pf.state(i), State::Ready, "index {i} should be ready");
        }
        assert_eq!(pf.state(46), State::Absent);
        assert_eq!(pf.state(54), State::Absent);
        assert_eq!(calls.load(Ordering::SeqCst), 7);
    }

    #[test]
    fn window_clamps_at_collection_edges() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            paths(5),
            10,
            2,
            CountingLoader { calls: Arc::clone(&calls), delay: Duration::ZERO },
        );
        pf.set_centre(0);
        pf.wait_idle();
        // Only 5 paths exist, so only 5 decodes despite radius 10.
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        for i in 0..5 {
            assert_eq!(pf.state(i), State::Ready);
        }
    }

    #[test]
    fn take_ready_delivers_once_then_reports_collected() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            paths(20),
            2,
            2,
            CountingLoader { calls: Arc::clone(&calls), delay: Duration::ZERO },
        );
        pf.set_centre(10);
        pf.wait_idle();

        let first = pf.take_ready();
        assert_eq!(first.len(), 5, "8..=12");
        for i in 8..=12 {
            assert_eq!(pf.state(i), State::Collected);
        }
        // Second call yields nothing: pixels are handed over exactly once.
        assert!(pf.take_ready().is_empty());
        assert_eq!(pf.resident(), 0, "no pixels retained after collection");
    }

    #[test]
    fn eviction_bounds_resident_pixels() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            paths(1000),
            2,
            2,
            CountingLoader { calls: Arc::clone(&calls), delay: Duration::ZERO },
        );
        // Walk a long way without ever collecting.
        for c in 0..200 {
            pf.set_centre(c);
            pf.wait_idle();
            assert!(
                pf.resident() <= 5,
                "resident {} exceeded window of 5 at centre {c}",
                pf.resident()
            );
        }
    }

    #[test]
    fn moving_window_leaves_only_the_final_window_warm() {
        // A fast-moving cursor must not leave slots behind anywhere it passed through.
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            paths(500),
            4,
            2,
            CountingLoader { calls: Arc::clone(&calls), delay: Duration::from_millis(5) },
        );
        for c in (0..200).step_by(7) {
            pf.set_centre(c);
        }
        pf.set_centre(400);
        pf.wait_idle();

        for i in 396..=404 {
            assert_eq!(pf.state(i), State::Ready, "final window must be warm at {i}");
        }
        // Nothing outside the final window survives.
        assert_eq!(pf.state(100), State::Absent);
        assert_eq!(pf.state(395), State::Absent);
        assert_eq!(pf.state(405), State::Absent);
    }

    #[test]
    fn in_flight_decode_is_discarded_when_the_window_moves_away() {
        // Timed so staleness is deterministic rather than racy: the per-image delay is
        // far longer than the pause before moving, so both workers are certainly
        // mid-decode and certainly unfinished when the window jumps.
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            paths(500),
            4,
            2,
            CountingLoader {
                calls: Arc::clone(&calls),
                delay: Duration::from_millis(120),
            },
        );
        pf.set_centre(50);
        thread::sleep(Duration::from_millis(20));
        assert_eq!(
            pf.stats().decoded,
            0,
            "test setup: no decode should have finished yet"
        );

        pf.set_centre(400);
        pf.wait_idle();

        let st = pf.stats();
        assert!(
            st.discarded >= 2,
            "both in-flight decodes should have been thrown away, got {st:?}"
        );
        // And the destination is still correctly warm afterwards.
        for i in 396..=404 {
            assert_eq!(pf.state(i), State::Ready, "index {i}");
        }
    }

    #[test]
    fn failures_are_remembered_and_not_retried() {
        let pf = Prefetcher::new(paths(20), 2, 2, FlakyLoader { missing: vec![10] });
        pf.set_centre(10);
        pf.wait_idle();

        assert_eq!(pf.state(10), State::Failed);
        assert!(pf.failure(10).unwrap().contains("No such file"));
        // Neighbours still load.
        assert_eq!(pf.state(9), State::Ready);
        assert_eq!(pf.state(11), State::Ready);

        // Re-centring on the same spot must not re-attempt the failed slot.
        let before = pf.stats().failed;
        pf.set_centre(10);
        pf.wait_idle();
        assert_eq!(pf.stats().failed, before, "failed slot must not be retried");
        assert_eq!(pf.state(10), State::Failed);
    }

    /// Regression: `window()` used to return `(0, 0)` for an empty collection, so
    /// `set_centre` queued index 0 while `in_window` rejected it. The slot stayed
    /// `Queued` forever and `wait_idle` blocked for good.
    #[test]
    fn empty_collection_does_not_wedge_a_slot() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            Vec::new(),
            5,
            2,
            CountingLoader { calls: Arc::clone(&calls), delay: Duration::ZERO },
        );
        pf.set_centre(0);
        // Would hang before the fix.
        pf.wait_idle();
        assert_eq!(pf.state(0), State::Absent, "no slot may exist");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    /// Regression: leaving and re-entering an index mid-decode let two workers decode
    /// it; the second finisher saw `Ready` rather than `Decoding`, judged the result
    /// unwanted and removed the image the first had published.
    #[test]
    fn window_leaving_and_returning_mid_decode_keeps_the_image() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            paths(200),
            1,
            2,
            CountingLoader {
                calls: Arc::clone(&calls),
                delay: Duration::from_millis(40),
            },
        );
        // Start decoding around 50, then flee and come straight back while in flight.
        pf.set_centre(50);
        thread::sleep(Duration::from_millis(5));
        pf.set_centre(150);
        pf.set_centre(50);
        pf.wait_idle();

        for i in 49..=51 {
            assert_eq!(
                pf.state(i),
                State::Ready,
                "index {i} must be ready, not silently dropped"
            );
        }
    }

    /// A centre past the end (a stale index after the tree shrank, R11) must clamp
    /// rather than produce an empty or panicking window.
    #[test]
    fn centre_beyond_end_clamps() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            paths(10),
            2,
            2,
            CountingLoader { calls: Arc::clone(&calls), delay: Duration::ZERO },
        );
        pf.set_centre(9_999);
        pf.wait_idle();
        // Clamped to index 9, so 7..=9 are warm.
        for i in 7..=9 {
            assert_eq!(pf.state(i), State::Ready, "index {i} should be ready");
        }
        assert_eq!(pf.state(6), State::Absent);
    }

    #[test]
    fn empty_collection_is_harmless() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            Vec::new(),
            5,
            2,
            CountingLoader { calls: Arc::clone(&calls), delay: Duration::ZERO },
        );
        pf.set_centre(0);
        pf.wait_idle();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(pf.state(0), State::Absent);
    }

    /// Hovering a distant row must decode it without disturbing the window (R12).
    #[test]
    fn hint_decodes_outside_the_window_and_survives() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            paths(200),
            2,
            2,
            CountingLoader { calls: Arc::clone(&calls), delay: Duration::ZERO },
        );
        pf.set_centre(10);
        pf.wait_idle();
        assert_eq!(pf.state(150), State::Absent);

        pf.hint(150);
        pf.wait_idle();
        assert_eq!(pf.state(150), State::Ready, "hovered row should be decoded");
        // The window itself is untouched.
        for i in 8..=12 {
            assert_eq!(pf.state(i), State::Ready, "index {i}");
        }

        // Moving the window nearby must not evict the hint.
        pf.set_centre(11);
        pf.wait_idle();
        assert_eq!(pf.state(150), State::Ready, "hint must survive a window move");
    }

    #[test]
    fn only_one_hint_is_retained() {
        // Sweeping the pointer down a list must not accumulate slots without bound.
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            paths(500),
            1,
            2,
            CountingLoader { calls: Arc::clone(&calls), delay: Duration::ZERO },
        );
        pf.set_centre(0);
        pf.wait_idle();

        for i in 100..140 {
            pf.hint(i);
            pf.wait_idle();
        }
        // Window is 0..=1 plus at most one hint.
        assert!(
            pf.resident() <= 3,
            "resident {} suggests hints are accumulating",
            pf.resident()
        );
        assert_eq!(pf.state(139), State::Ready, "the latest hint is warm");
        assert_eq!(pf.state(120), State::Absent, "older hints are dropped");
    }

    #[test]
    fn clear_hint_lets_the_slot_be_evicted() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            paths(100),
            1,
            2,
            CountingLoader { calls: Arc::clone(&calls), delay: Duration::ZERO },
        );
        pf.set_centre(5);
        pf.hint(50);
        pf.wait_idle();
        assert_eq!(pf.state(50), State::Ready);

        pf.clear_hint();
        pf.set_centre(6);
        pf.wait_idle();
        assert_eq!(pf.state(50), State::Absent, "cleared hint should be evicted");
    }

    #[test]
    fn hint_on_out_of_range_index_is_ignored() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            paths(10),
            1,
            2,
            CountingLoader { calls: Arc::clone(&calls), delay: Duration::ZERO },
        );
        pf.set_centre(0);
        pf.hint(9_999);
        pf.wait_idle();
        assert_eq!(pf.state(9_999), State::Absent);
    }

    /// The renderer defers uploads to bound per-frame work, so deferred pixels must be
    /// returnable rather than lost.
    #[test]
    fn put_back_makes_an_image_collectable_again() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            paths(50),
            2,
            2,
            CountingLoader { calls: Arc::clone(&calls), delay: Duration::ZERO },
        );
        pf.set_centre(10);
        pf.wait_idle();

        let mut ready = pf.take_ready();
        assert!(ready.len() >= 3);
        let (idx, img) = ready.pop().unwrap();
        pf.put_back(idx, img);

        assert_eq!(pf.state(idx), State::Ready, "returned image is available again");
        let again = pf.take_ready();
        assert!(again.iter().any(|(i, _)| *i == idx), "must be collectable");
    }

    #[test]
    fn put_back_of_an_evicted_index_is_dropped() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            paths(200),
            2,
            2,
            CountingLoader { calls: Arc::clone(&calls), delay: Duration::ZERO },
        );
        pf.set_centre(10);
        pf.wait_idle();
        let (idx, img) = pf.take_ready().pop().unwrap();

        // Move far away, then try to return the stale image.
        pf.set_centre(150);
        pf.wait_idle();
        pf.put_back(idx, img);
        assert_eq!(pf.state(idx), State::Absent, "stale image must not be resurrected");
    }

    /// The waker is how the event loop learns a decode finished, replacing polling.
    #[test]
    fn waker_fires_for_each_completed_decode() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            paths(50),
            2,
            2,
            CountingLoader { calls: Arc::clone(&calls), delay: Duration::ZERO },
        );
        let wakes = Arc::new(AtomicUsize::new(0));
        let w = Arc::clone(&wakes);
        pf.set_waker(Arc::new(move || {
            w.fetch_add(1, Ordering::SeqCst);
        }));

        pf.set_centre(10);
        pf.wait_idle();

        // `wait_idle` returns as soon as the last slot is published, which happens just
        // *before* that worker invokes the waker -- so poll rather than asserting
        // immediately, or the check races the final callback.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while wakes.load(Ordering::SeqCst) < 5 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        // Five slots in a radius-2 window, so one wake per completion.
        assert!(
            wakes.load(Ordering::SeqCst) >= 5,
            "expected at least 5 wakes, got {}",
            wakes.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn shutdown_joins_workers_promptly() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            paths(1000),
            10,
            4,
            CountingLoader { calls: Arc::clone(&calls), delay: Duration::from_millis(2) },
        );
        pf.set_centre(500);
        // Drop while work is in flight; Drop must not hang.
        let t = std::time::Instant::now();
        drop(pf);
        assert!(t.elapsed() < Duration::from_secs(5), "drop should not hang");
    }

    #[test]
    fn repeated_recentre_on_same_index_is_idempotent() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pf = Prefetcher::new(
            paths(50),
            3,
            2,
            CountingLoader { calls: Arc::clone(&calls), delay: Duration::ZERO },
        );
        pf.set_centre(20);
        pf.wait_idle();
        let after_first = calls.load(Ordering::SeqCst);

        for _ in 0..10 {
            pf.set_centre(20);
            pf.wait_idle();
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            after_first,
            "already-decoded slots must not be decoded again"
        );
    }
}
