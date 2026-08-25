//! R21 driven through a real egui context.
//!
//! egui is pure CPU, so the sidebar can be laid out here even though the sandbox has no
//! display and no GPU. This is the only way to check the tree actually follows the
//! selection: the arithmetic can be right in isolation and still disagree with what egui
//! lays out, which is exactly how the bug survived a unit test of the arithmetic alone.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

use cull::app::App;
use cull::decode::Image;
use cull::prefetch::Loader;
use cull::trash::Bin;

const TINY_JPEG: &[u8] = include_bytes!("fixtures/tiny.jpg");

struct StubLoader;
impl Loader for StubLoader {
    fn load(&self, _p: &Path) -> Result<Image, String> {
        Err("not needed for layout".into())
    }
}

struct NoBin;
impl Bin for NoBin {
    fn send(&self, _p: &Path) -> Result<(), String> {
        panic!("layout tests must not delete");
    }
    fn restore(&self, _p: &Path) -> Result<(), String> {
        panic!("layout tests must not restore");
    }
}

fn app_with(n: usize) -> (tempfile::TempDir, App) {
    let td = tempfile::tempdir().unwrap();
    let photos = td.path().join("photos");
    std::fs::create_dir_all(&photos).unwrap();
    for i in 0..n {
        std::fs::write(photos.join(format!("IMG{i:05}.JPG")), TINY_JPEG).unwrap();
    }
    let app = App::new(&photos, StubLoader, Arc::new(NoBin), 2, 2);
    (td, app)
}

/// Panel size in points, roughly the real window.
const SCREEN: (f32, f32) = (1600.0, 1000.0);

/// Mirrors the shell's own bookkeeping, including that the tree follows the selection
/// only when it has moved.
struct Harness {
    ctx: egui::Context,
    offset: f32,
    followed: Option<usize>,
    collapsed: HashSet<std::path::PathBuf>,
}

impl Harness {
    fn new() -> Self {
        Self {
            ctx: egui::Context::default(),
            offset: 0.0,
            followed: None,
            collapsed: HashSet::new(),
        }
    }

    /// Lay the sidebar out once, feeding last frame's offset back in as the shell does.
    fn frame(&mut self, app: &App) -> cull::ui::UiOutcome {
        self.frame_with(app, Vec::new())
    }

    /// Scroll the list the way a user would: pointer over the sidebar, wheel events.
    ///
    /// Assigning the offset directly would prove nothing -- egui keeps its own retained
    /// scroll state per `ScrollArea`, and only actually moves for real input or an
    /// explicit override.
    fn scroll_by(&mut self, app: &App, points: f32) {
        self.frame_with(
            app,
            vec![
                egui::Event::PointerMoved(egui::pos2(200.0, 500.0)),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -points),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::default(),
                },
            ],
        );
    }

    fn frame_with(&mut self, app: &App, events: Vec<egui::Event>) -> cull::ui::UiOutcome {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(SCREEN.0, SCREEN.1),
            )),
            events,
            ..Default::default()
        };
        let follow = self.followed != Some(app.index());
        let mut captured = None;
        // The tessellated output is irrelevant here; only the layout matters.
        let _ = self.ctx.clone().run_ui(input, |root| {
            captured = Some(cull::ui::draw(root, app, &self.collapsed, self.offset, follow));
        });
        let out = captured.expect("ui::draw must run");
        self.offset = out.scroll_offset;
        if follow {
            self.followed = Some(app.index());
        }
        out
    }

    /// Two frames: one to apply the correction, one to observe the settled layout.
    fn settle(&mut self, app: &App) -> cull::ui::UiOutcome {
        self.frame(app);
        self.frame(app)
    }

    /// Run frames until the offset stops moving.
    ///
    /// egui eases wheel scrolling over several frames, so the offset keeps changing
    /// after the last event; asserting before it has come to rest measures the
    /// animation rather than anything this code does.
    fn quiesce(&mut self, app: &App) -> cull::ui::UiOutcome {
        let mut out = self.frame(app);
        for _ in 0..200 {
            let before = self.offset;
            out = self.frame(app);
            if (self.offset - before).abs() < 0.01 {
                break;
            }
        }
        out
    }
}

/// The selected row must actually be on screen, with egui doing the layout.
#[test]
fn selection_stays_visible_while_stepping_down() {
    let (_td, mut app) = app_with(600);
    let mut h = Harness::new();
    h.settle(&app);

    for i in 0..600 {
        app.select(i);
        let out = h.settle(&app);
        let (rect, view) = (
            out.selected_row_rect.expect("selected row must be laid out"),
            out.list_viewport,
        );
        assert!(
            rect.top() >= view.top() - 0.5 && rect.bottom() <= view.bottom() + 0.5,
            "row {i} at {:?} is outside the visible list {:?} (offset {})",
            rect,
            view,
            h.offset
        );
    }
}

#[test]
fn selection_stays_visible_while_stepping_up() {
    let (_td, mut app) = app_with(600);
    let mut h = Harness::new();
    app.select(599);
    h.settle(&app);

    for i in (0..600).rev() {
        app.select(i);
        let out = h.settle(&app);
        let rect = out.selected_row_rect.expect("selected row must be laid out");
        let view = out.list_viewport;
        assert!(
            rect.top() >= view.top() - 0.5 && rect.bottom() <= view.bottom() + 0.5,
            "row {i} at {:?} is outside the visible list {:?}",
            rect,
            view
        );
    }
}

/// Big jumps (Home/End/PageDown, or a click in the tree) must land visible too.
#[test]
fn selection_stays_visible_after_jumps() {
    let (_td, mut app) = app_with(2000);
    let mut h = Harness::new();
    h.settle(&app);

    for i in [0, 1999, 1000, 3, 1998, 500, 0, 1500] {
        app.select(i);
        let out = h.settle(&app);
        let rect = out.selected_row_rect.expect("selected row must be laid out");
        let view = out.list_viewport;
        assert!(
            rect.top() >= view.top() - 0.5 && rect.bottom() <= view.bottom() + 0.5,
            "row {i} at {:?} is outside the visible list {:?}",
            rect,
            view
        );
    }
}

/// Reported as "I can only scroll in the vicinity of the current selected image".
///
/// Once the selection has been followed, the user must be able to scroll anywhere and
/// stay there. Re-asserting the correction every frame dragged the list straight back.
#[test]
fn the_user_can_scroll_away_from_the_selection_and_stay_there() {
    let (_td, mut app) = app_with(2000);
    let mut h = Harness::new();
    app.select(50);
    h.settle(&app);
    let started_at = h.offset;

    // Wheel a long way down, as the user would, and let the easing finish.
    for _ in 0..40 {
        h.scroll_by(&app, 400.0);
    }
    h.quiesce(&app);
    let scrolled_to = h.offset;
    assert!(
        scrolled_to > started_at + 1000.0,
        "the wheel did not move the list: {started_at} -> {scrolled_to}"
    );

    // And it must stay there, with the selection still on row 50, far above.
    for _ in 0..5 {
        h.frame(&app);
    }
    assert!(
        (h.offset - scrolled_to).abs() < 1.0,
        "the list snapped back from {scrolled_to} to {} -- the selection is dragging it",
        h.offset
    );
}

/// But a *move* must still pull the list back to the selection.
#[test]
fn moving_the_selection_still_scrolls_to_it() {
    let (_td, mut app) = app_with(2000);
    let mut h = Harness::new();
    app.select(50);
    h.settle(&app);

    // Wander off, then move the selection: the tree must follow again.
    for _ in 0..40 {
        h.scroll_by(&app, 400.0);
    }
    h.quiesce(&app);
    app.select(51);
    let out = h.settle(&app);

    let rect = out
        .selected_row_rect
        .expect("moving the selection must bring it back on screen");
    assert!(
        rect.top() >= out.list_viewport.top() - 0.5
            && rect.bottom() <= out.list_viewport.bottom() + 0.5,
        "row 51 at {rect:?} is outside {:?}",
        out.list_viewport
    );
}

/// The scroll offset must reach a fixed point, or the correction is being re-applied
/// every frame and the user cannot scroll the list by hand at all.
#[test]
fn scrolling_settles_and_does_not_pin_the_list() {
    let (_td, mut app) = app_with(2000);
    let mut h = Harness::new();

    for i in [0, 999, 1999, 1997, 1000] {
        app.select(i);
        h.settle(&app);
        let settled = h.offset;
        for _ in 0..3 {
            h.frame(&app);
            assert!(
                (h.offset - settled).abs() < 0.5,
                "offset keeps moving at row {i}: {settled} -> {}",
                h.offset
            );
        }
    }
}
