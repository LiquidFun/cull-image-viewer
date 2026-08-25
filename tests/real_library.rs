//! End-to-end tests against the real photo library.
//!
//! These exercise scan, grouping, RAW preview extraction, real JPEG decoding, the
//! prefetch ring and the view maths together. Everything except the GPU.
//!
//! Skipped automatically when the library is absent, so the suite still runs anywhere.
//! Note the library is mutable (R11): assertions are about invariants, never counts.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cull::app::{Action, App};
use cull::decode::{self, Decoder};
use cull::prefetch::{FileLoader, Loader, Prefetcher, State};
use cull::scan;
use cull::trash::Bin;
use cull::view::{FitMode, Orientation, View, Viewport};

const LIBRARY: &str = "/workspace/2026-08_Norwegen_Lofoten";

fn library() -> Option<PathBuf> {
    let p = PathBuf::from(LIBRARY);
    p.is_dir().then_some(p)
}

/// A bin that refuses everything, so tests can never delete the user's photos.
struct ReadOnlyBin;

impl Bin for ReadOnlyBin {
    fn send(&self, _p: &Path) -> Result<(), String> {
        panic!("tests must never delete files from the real library");
    }
    fn restore(&self, _p: &Path) -> Result<(), String> {
        panic!("tests must never restore into the real library");
    }
}

#[test]
fn scan_produces_consistent_groups() {
    let Some(root) = library() else {
        eprintln!("skipping: {LIBRARY} not present");
        return;
    };
    let dirs = scan::scan(&root);
    let groups = scan::flatten(&dirs);
    assert!(!groups.is_empty(), "library should contain images");

    // Flattened order must match the per-directory order the sidebar walks.
    let mut expected = 0usize;
    for d in &dirs {
        assert!(!d.groups.is_empty(), "empty directories must be dropped");
        expected += d.groups.len();
    }
    assert_eq!(expected, groups.len(), "flatten must preserve every group");

    for g in &groups {
        assert!(
            g.display_path().is_some(),
            "group {} has no displayable file",
            g.stem
        );
        assert!(!g.stem.is_empty());
        // Every member must share the group's stem prefix.
        for m in &g.members {
            let name = m.path.file_name().unwrap().to_string_lossy();
            assert!(
                name.starts_with(&g.stem),
                "member {name} does not belong to group {}",
                g.stem
            );
        }
    }
}

#[test]
fn every_group_has_a_decodable_display_file() {
    let Some(root) = library() else {
        return;
    };
    let groups = scan::flatten(&scan::scan(&root));

    // Sample across the library rather than decoding 2000 files.
    let step = (groups.len() / 12).max(1);
    let mut checked = 0;
    for g in groups.iter().step_by(step) {
        let path = g.display_path().expect("has a display file");
        let Ok(bytes) = std::fs::read(path) else {
            // R11: may have been deleted since the scan. Not a failure.
            continue;
        };
        let img = decode::decode(&bytes, None, Decoder::Zune)
            .unwrap_or_else(|e| panic!("failed to decode {}: {e}", path.display()));

        assert!(img.width > 0 && img.height > 0);
        assert_eq!(
            img.rgba.len(),
            (img.width * img.height * 4) as usize,
            "RGBA buffer must be tightly packed for the texture upload"
        );
        assert!((1..=8).contains(&img.orientation));
        checked += 1;
    }
    assert!(checked > 0, "should have decoded at least one image");
}

#[test]
fn raw_previews_match_their_sidecar_jpegs() {
    let Some(root) = library() else {
        return;
    };
    let groups = scan::flatten(&scan::scan(&root));

    // Find groups holding both an ARW and a JPG, and check the ARW's embedded preview
    // agrees with the sidecar. This is the R9 claim that RAW needs no demosaicing.
    let mut checked = 0;
    for g in groups.iter().step_by((groups.len() / 6).max(1)) {
        let arw = g.members.iter().find(|m| m.ext == "arw");
        let jpg = g.members.iter().find(|m| m.ext == "jpg");
        let (Some(arw), Some(jpg)) = (arw, jpg) else {
            continue;
        };
        let (Ok(ab), Ok(jb)) = (std::fs::read(&arw.path), std::fs::read(&jpg.path)) else {
            continue;
        };

        let a = decode::locate(&ab).expect("ARW should yield a preview");
        let j = decode::locate(&jb).expect("JPG should locate itself");
        assert!(a.from_raw, "ARW must be recognised as a RAW container");
        assert!(!j.from_raw);

        let ad = decode::jpeg_dimensions(a.jpeg).expect("preview has dimensions");
        let jd = decode::jpeg_dimensions(j.jpeg).expect("jpeg has dimensions");
        assert_eq!(
            ad, jd,
            "{}: embedded preview {ad:?} should match sidecar {jd:?}",
            g.stem
        );
        assert_eq!(
            a.orientation, j.orientation,
            "{}: orientation must agree between RAW and JPEG",
            g.stem
        );
        // The whole point: the preview is smaller to decode than the sidecar.
        assert!(
            a.jpeg.len() < jb.len(),
            "{}: preview ({} B) should be cheaper than the sidecar ({} B)",
            g.stem,
            a.jpeg.len(),
            jb.len()
        );
        checked += 1;
    }
    assert!(checked > 0, "should have compared at least one RAW/JPEG pair");
}

#[test]
fn orientation_and_fit_agree_with_decoded_pixels() {
    let Some(root) = library() else {
        return;
    };
    let groups = scan::flatten(&scan::scan(&root));
    let vp = Viewport::new(3840.0, 2160.0);

    let mut portrait_seen = false;
    let mut landscape_seen = false;

    for g in groups.iter().step_by((groups.len() / 40).max(1)) {
        let Ok(bytes) = std::fs::read(g.display_path().unwrap()) else {
            continue;
        };
        let Ok(img) = decode::decode(&bytes, None, Decoder::Zune) else {
            continue;
        };
        let o = Orientation::new(img.orientation);
        let displayed = o.displayed_size(img.width, img.height);

        if o.swaps_axes() {
            assert_eq!(displayed, (img.height, img.width));
            portrait_seen = true;
        } else {
            assert_eq!(displayed, (img.width, img.height));
            landscape_seen = true;
        }

        // A fitted image must never exceed the viewport on either axis.
        let v = View::fitted(displayed, vp);
        let (w, h) = v.scaled_size(displayed);
        assert!(
            w <= vp.width + 1e-6 && h <= vp.height + 1e-6,
            "{}: fitted size {w}x{h} exceeds viewport",
            g.stem
        );
        assert!(v.zoom > 0.0 && v.zoom.is_finite());
    }

    assert!(landscape_seen, "expected some landscape frames");
    // The library is known to contain orientation-8 frames, but it is mutable, so this
    // is informational rather than an assertion.
    if !portrait_seen {
        eprintln!("note: no rotated frames in the sampled subset");
    }
}

#[test]
fn prefetch_ring_warms_the_window_on_real_files() {
    let Some(root) = library() else {
        return;
    };
    let groups = scan::flatten(&scan::scan(&root));
    let paths: Vec<PathBuf> = groups
        .iter()
        .filter_map(|g| g.display_path().map(Path::to_path_buf))
        .collect();
    if paths.len() < 20 {
        return;
    }

    let radius = 3;
    let pf = Prefetcher::new(paths, radius, 2, FileLoader::default());
    pf.set_centre(10);
    pf.wait_idle();

    for i in 7..=13 {
        let s = pf.state(i);
        assert!(
            matches!(s, State::Ready | State::Failed),
            "index {i} settled as {s:?}, expected Ready or Failed"
        );
    }
    // Outside the window nothing is resident.
    assert_eq!(pf.state(6), State::Absent);
    assert_eq!(pf.state(14), State::Absent);

    // Collecting yields real pixels of the right size.
    let ready = pf.take_ready();
    assert!(!ready.is_empty());
    for (_, img) in &ready {
        assert_eq!(img.rgba.len(), (img.width * img.height * 4) as usize);
    }
    // Pixels are handed over exactly once.
    assert_eq!(pf.resident(), 0);
}

#[test]
fn app_navigates_the_real_library() {
    let Some(root) = library() else {
        return;
    };
    let mut app = App::new(&root, FileLoader::default(), Arc::new(ReadOnlyBin), 3, 2);
    assert!(app.len() > 50, "expected a sizeable library");

    app.resize(3840.0, 2160.0);

    // Walk forward, binding each image as the renderer would.
    for _ in 0..12 {
        app.prefetch().wait_idle();
        let idx = app.index();
        if let Some((i, img)) = app
            .prefetch()
            .take_ready()
            .into_iter()
            .find(|(i, _)| *i == idx)
        {
            app.note_shown(
                i,
                (img.width, img.height),
                img.orientation,
                img.icc.as_deref(),
                img.color_space,
            );
            let shown = app.shown().expect("just recorded");
            assert_eq!(shown.index, idx);
            // Fit mode is the default, so the image must be fitted and centred.
            assert_eq!(app.view.pan, (0.0, 0.0));
            let (w, h) = app.view.scaled_size(shown.displayed);
            assert!(w <= app.viewport.width + 1e-6 && h <= app.viewport.height + 1e-6);
            // Real files are sRGB, so nothing should be flagged (R10).
            assert!(
                !shown.colour.needs_warning(),
                "{} unexpectedly flagged: {}",
                app.current().unwrap().stem,
                shown.colour.describe()
            );
        }
        app.act(Action::Next);
    }
    assert_eq!(app.index(), 12);

    // Zoom and pan must stay bounded on a real image.
    app.act(Action::ActualSize);
    app.pan((100_000.0, 100_000.0));
    let displayed = app.shown().map_or((1, 1), |s| s.displayed);
    let (sw, sh) = app.view.scaled_size(displayed);
    let max_x = ((sw - app.viewport.width) / 2.0).max(0.0);
    let max_y = ((sh - app.viewport.height) / 2.0).max(0.0);
    assert!(app.view.pan.0 <= max_x + 1e-6, "pan escaped horizontally");
    assert!(app.view.pan.1 <= max_y + 1e-6, "pan escaped vertically");

    // Zooming and panning already handed control to the user, so the mode is now
    // Preserve without any toggle -- that is what makes a single `X` recentre.
    assert_eq!(
        app.fit_mode,
        FitMode::Preserve,
        "manual zoom/pan should leave refit mode"
    );
    let z = app.view.zoom;
    app.act(Action::Next);
    app.prefetch().wait_idle();
    assert!((app.view.zoom - z).abs() < 1e-9, "preserve mode lost the zoom");

    // And one toggle returns to a fitted view.
    app.act(Action::ToggleFitMode);
    assert_eq!(app.fit_mode, FitMode::Refit);
    let displayed = app.shown().map_or((1, 1), |s| s.displayed);
    let (w, h) = app.view.scaled_size(displayed);
    assert!(
        w <= app.viewport.width + 1e-6 && h <= app.viewport.height + 1e-6,
        "one toggle should refit, got {w}x{h}"
    );
    assert_eq!(app.view.pan, (0.0, 0.0), "refit should also recentre");
}

/// The view must be laid out from the header before pixels exist, or the image visibly
/// resizes when it finishes loading.
#[test]
fn probe_reports_dimensions_without_decoding() {
    let Some(root) = library() else {
        return;
    };
    let groups = scan::flatten(&scan::scan(&root));

    let mut checked = 0;
    for g in groups.iter().step_by((groups.len() / 8).max(1)) {
        let path = g.display_path().unwrap();
        let Some((pw, ph, po)) = decode::probe(path) else {
            continue;
        };
        // Must agree exactly with a full decode, or the view would jump anyway.
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let Ok(img) = decode::decode(&bytes, None, Decoder::Zune) else {
            continue;
        };
        assert_eq!(
            (pw, ph),
            (img.width, img.height),
            "{}: probe disagreed with decode",
            g.stem
        );
        assert_eq!(po, img.orientation, "{}: orientation mismatch", g.stem);
        checked += 1;
    }
    assert!(checked > 0, "should have probed at least one image");
}

#[test]
fn probe_is_much_cheaper_than_decoding() {
    let Some(root) = library() else {
        return;
    };
    let groups = scan::flatten(&scan::scan(&root));
    let Some(path) = groups.first().and_then(|g| g.display_path()) else {
        return;
    };

    // Warm the page cache so this measures work, not I/O.
    let _ = decode::probe(path);

    let t = std::time::Instant::now();
    for _ in 0..20 {
        assert!(decode::probe(path).is_some());
    }
    let probe_ms = t.elapsed().as_secs_f64() * 1000.0 / 20.0;

    // Probing is only useful if it is effectively free on the UI thread.
    assert!(
        probe_ms < 20.0,
        "probe took {probe_ms:.2} ms/image, too slow to do on selection"
    );
}

#[test]
fn probe_on_missing_file_is_none() {
    assert!(decode::probe(Path::new("/definitely/not/here.jpg")).is_none());
}

#[test]
fn decoding_is_robust_against_truncated_and_corrupt_files() {
    let Some(root) = library() else {
        return;
    };
    let groups = scan::flatten(&scan::scan(&root));
    let Some(path) = groups.first().and_then(|g| g.display_path()) else {
        return;
    };
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };

    // Truncation at many points must never panic.
    for frac in [0.0, 0.001, 0.01, 0.1, 0.5, 0.9, 0.999] {
        let n = (bytes.len() as f64 * frac) as usize;
        let _ = decode::decode(&bytes[..n], None, Decoder::Zune);
        let _ = decode::locate(&bytes[..n]);
        let _ = decode::jpeg_dimensions(&bytes[..n]);
    }

    // Byte corruption in the header region must never panic.
    for offset in [0, 2, 4, 8, 20, 100, 1000] {
        let mut corrupt = bytes.clone();
        if offset < corrupt.len() {
            corrupt[offset] ^= 0xFF;
            let _ = decode::decode(&corrupt, None, Decoder::Zune);
        }
    }
}

/// The loader used by the real app must report a missing file rather than panicking.
#[test]
fn file_loader_reports_missing_paths() {
    match FileLoader::default().load(Path::new("/definitely/not/here.jpg")) {
        Ok(_) => panic!("loading a nonexistent path should fail"),
        Err(e) => assert!(!e.is_empty(), "error message should be informative"),
    }
}
