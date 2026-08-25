//! egui overlay: directory tree sidebar and status bar (REQUIREMENTS.md R3).
//!
//! Presentation only. Every decision is delegated to [`crate::app::App`]; the sidebar
//! reports selections, hovers and its own leftover space back to the caller.
//!
//! The tree is **virtualised**: only the rows actually on screen are laid out. An
//! `egui::CollapsingHeader` body instantiates a widget for every child, so an expanded
//! directory of 538 groups cost 538 widget layouts every frame regardless of how few were
//! visible. That is why the list is flattened into rows and drawn with
//! `ScrollArea::show_rows`, which needs a uniform row height and its own collapse state.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::app::{Action, App};
use crate::prefetch::State;
use crate::scan::{format_size, format_time, DirNode, Group, MetaStore};

/// What the user did in the UI this frame, plus the space left for the image.
pub struct UiOutcome {
    pub selected: Option<usize>,
    /// Row under the pointer. The caller preloads it, since a click is likely next.
    pub hovered: Option<usize>,
    pub action: Option<Action>,
    /// Directory whose collapsed state should be toggled.
    pub toggled_dir: Option<PathBuf>,
    /// Scroll offset of the tree after this frame, fed back in next frame so the
    /// selection can be kept in view with a minimal correction.
    pub scroll_offset: f32,
    /// Rect not claimed by any panel, in egui points. This is the image area, reported
    /// rather than assumed because the sidebar is resizable.
    pub image_rect: egui::Rect,
    /// Where the selected row was drawn, if it was on screen at all. Reported so R21 can
    /// be tested against egui's own layout rather than against a second copy of the
    /// arithmetic -- the two disagreeing is precisely how this went wrong before.
    pub selected_row_rect: Option<egui::Rect>,
    /// Visible rect of the scrolling list, the thing `selected_row_rect` must sit inside.
    pub list_viewport: egui::Rect,
}

/// Default sidebar width.
///
/// Sized to actually fit a row: the four columns come to 56 monospace characters, which
/// egui lays out at ~446 px, plus the panel margins and the scrollbar. The old 380 was
/// *meant* to fit them and did not, so every row silently wrapped to two lines. Rows are
/// truncated rather than wrapped now, so dragging the panel narrower is safe -- it just
/// costs the trailing columns.
pub const SIDEBAR_WIDTH: f32 = 480.0;

/// One line in the flattened tree.
enum Row<'a> {
    Dir {
        node: &'a DirNode,
        collapsed: bool,
    },
    Group {
        group: &'a Group,
        index: usize,
    },
}

/// Flatten the directory tree into a list of rows, honouring collapse state.
///
/// Group indices are assigned from the full list regardless of visibility, so they always
/// match the ordering the app and prefetch ring use.
fn build_rows<'a>(app: &'a App, collapsed: &HashSet<PathBuf>) -> Vec<Row<'a>> {
    let mut rows = Vec::new();
    let mut base = 0usize;
    for node in app.dirs() {
        let is_collapsed = collapsed.contains(&node.path);
        rows.push(Row::Dir {
            node,
            collapsed: is_collapsed,
        });
        if !is_collapsed {
            for (offset, group) in node.groups.iter().enumerate() {
                rows.push(Row::Group {
                    group,
                    index: base + offset,
                });
            }
        }
        base += node.groups.len();
    }
    rows
}

/// Row height to hand to `show_rows`, and the pitch it will actually lay rows out on.
///
/// Returned together because the two must agree, and every past bug here was them
/// disagreeing. `show_rows` positions rows on a fixed pitch computed from the height it
/// is given; if the rows it then draws are a different height, the mapping from scroll
/// offset to row is wrong and the selection cannot be located on screen at all.
///
/// Measured against egui's own layout (`tests/sidebar_scroll.rs`), a row is:
///
/// * `selectable_label`, i.e. a button, so `text + 2 * button_padding.y` = 17.125,
/// * but floored at `interact_size.y` = 18, which is what actually wins,
/// * and `show_rows` adds `item_spacing.y` = 3 itself, giving a pitch of 21.
///
/// Rows must also be kept to a single line -- see `SINGLE_LINE` at the call site.
fn row_metrics(ui: &egui::Ui) -> (f32, f32) {
    // Directory rows use the body style, group rows monospace; the taller must fit.
    let text_h = ui
        .text_style_height(&egui::TextStyle::Monospace)
        .max(ui.text_style_height(&egui::TextStyle::Body));
    let spacing = ui.spacing();
    let row_h = (text_h + 2.0 * spacing.button_padding.y).max(spacing.interact_size.y);
    (row_h, row_h + spacing.item_spacing.y)
}

/// Smallest scroll offset that brings row `pos` back into view, or `None` to leave the
/// offset alone.
///
/// Pure arithmetic so the R21 behaviour can be tested without a display.
fn scroll_correction(
    pos: usize,
    total_rows: usize,
    pitch: f32,
    spacing: f32,
    viewport_h: f32,
    offset: f32,
) -> Option<f32> {
    let row_top = pos as f32 * pitch;
    let row_bottom = row_top + pitch;
    // A margin so the selection does not sit flush against the edge while scrolling,
    // which is what made it appear to run off screen.
    let margin = pitch * 2.0;
    // Mirrors how `show_rows` sizes its content, so `max_off` is the real end of travel.
    let content_h = (pitch * total_rows as f32 - spacing).max(0.0);
    let max_off = (content_h - viewport_h).max(0.0);

    let wanted = if row_top - margin < offset {
        (row_top - margin).clamp(0.0, max_off)
    } else if row_bottom + margin > offset + viewport_h {
        (row_bottom + margin - viewport_h).clamp(0.0, max_off)
    } else {
        return None;
    };

    // Only override the user's own scrolling when it would actually move. For the last
    // rows the margin asks to scroll past the bottom, which clamps to the offset we are
    // already at; re-asserting it every frame pinned the scrollbar and made the list
    // impossible to scroll by hand.
    ((wanted - offset).abs() > 0.5).then_some(wanted)
}

/// Draw the sidebar and status bar into the root `Ui`.
///
/// egui 0.35 unified `SidePanel`/`TopBottomPanel` into `Panel`, which takes a `Ui` rather
/// than a `Context`; the root `Ui` comes from `Context::run_ui`.
/// `follow` asks for the selection to be scrolled into view. It must be true only on the
/// frames just after the selection *moved*, never continuously: re-asserting the offset
/// every frame drags the list back the moment the user scrolls, so they can only ever
/// scroll within a margin of whatever is selected.
pub fn draw(
    root: &mut egui::Ui,
    app: &App,
    collapsed: &HashSet<PathBuf>,
    prev_offset: f32,
    follow: bool,
) -> UiOutcome {
    // One snapshot per frame. Querying the ring per row locked its mutex hundreds of
    // times a frame and starved the decode workers.
    let failed: HashSet<usize> = app.prefetch().failed_indices().into_iter().collect();

    let mut out = UiOutcome {
        selected: None,
        hovered: None,
        action: None,
        toggled_dir: None,
        scroll_offset: prev_offset,
        image_rect: root.available_rect_before_wrap(),
        selected_row_rect: None,
        list_viewport: egui::Rect::NOTHING,
    };

    egui::Panel::left("tree")
        .default_size(SIDEBAR_WIDTH)
        .resizable(true)
        .show(root, |ui| {
            ui.heading("cull");
            ui.label(format!("{} images", app.len()));
            ui.separator();

            let rows = build_rows(app, collapsed);
            let (row_h, pitch) = row_metrics(ui);

            let mut area = egui::ScrollArea::vertical()
                // Without this the scroll area shrinks to fit its content, leaving the
                // scrollbar floating in the middle of the panel instead of at its edge.
                .auto_shrink([false, false]);

            // Bring the selection into view, but only when it has just moved. `scroll_to_me`
            // cannot help: with a virtualised list the selected row is usually not
            // instantiated, so there is nothing to scroll to. Instead correct the offset by
            // the smallest amount that brings the row back into view, using last frame's
            // offset. Doing this on every frame instead of only on a move is what made the
            // list impossible to scroll away from the selection.
            let viewport_h = ui.available_height();
            if follow {
                if let Some(pos) = rows
                    .iter()
                    .position(|r| matches!(r, Row::Group { index, .. } if *index == app.index()))
                {
                    let spacing = ui.spacing().item_spacing.y;
                    if let Some(off) =
                        scroll_correction(pos, rows.len(), pitch, spacing, viewport_h, prev_offset)
                    {
                        area = area.vertical_scroll_offset(off);
                    }
                }
            }

            let result = area.show_rows(ui, row_h, rows.len(), |ui, range| {
                // A virtualised list is built on every row being the same height, and a
                // row that wraps is not. The full row text is 56 monospace characters,
                // ~446 px, so at any sidebar width below that it wrapped to two lines and
                // became 32 px against the 18 px `show_rows` was told to expect. The
                // offset-to-row mapping was then wrong by nearly 2x, which is what put
                // the selection off screen no matter what the arithmetic did.
                // Truncating keeps every row exactly one line at any panel width.
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                for row in &rows[range] {
                    match row {
                        Row::Dir { node, collapsed } => {
                            let arrow = if *collapsed { "\u{25b6}" } else { "\u{25bc}" };
                            let name = node
                                .path
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| node.path.display().to_string());
                            let label = egui::RichText::new(format!(
                                "{arrow} {name}  ({})",
                                node.groups.len()
                            ))
                            .strong();
                            if ui.selectable_label(false, label).clicked() {
                                out.toggled_dir = Some(node.path.clone());
                            }
                        }
                        Row::Group { group, index } => {
                            let selected = *index == app.index();
                            let row_resp =
                                row_label(ui, group, app.meta(), selected, failed.contains(index));
                            if selected {
                                out.selected_row_rect = Some(row_resp.rect);
                            }
                            if row_resp.clicked() {
                                out.selected = Some(*index);
                            }
                            if row_resp.hovered() {
                                out.hovered = Some(*index);
                            }
                        }
                    }
                }
            });
            // Record where the list actually ended up, for next frame's correction.
            out.scroll_offset = result.state.offset.y;
            out.list_viewport = result.inner_rect;
        });

    egui::Panel::bottom("status").show(root, |ui| {
        ui.horizontal(|ui| {
            if let Some(group) = app.current() {
                ui.label(format!("{}/{}", app.index() + 1, app.len()));
                ui.separator();
                ui.label(&group.stem);
            } else {
                ui.label("no image");
            }

            if let Some(shown) = app.shown() {
                ui.separator();
                ui.label(format!(
                    "{}x{} ({:.1} MP)",
                    shown.stored.0,
                    shown.stored.1,
                    shown.megapixels()
                ));
                if shown.orientation.value() != 1 {
                    ui.label(format!("rot {}", shown.orientation.value()));
                }
                if shown.colour.needs_warning() {
                    ui.colored_label(
                        egui::Color32::from_rgb(240, 190, 90),
                        shown.colour.describe(),
                    );
                }
            }

            ui.separator();
            ui.label(format!("zoom {:.0}%", app.view.zoom * 100.0));
            ui.separator();
            if ui
                .button(format!("mode: {}", app.fit_mode.label()))
                .clicked()
            {
                out.action = Some(Action::ToggleFitMode);
            }

            // Loading indicator, so a cold slot does not look like a hang.
            match app.current_state() {
                State::Queued | State::Decoding => {
                    ui.separator();
                    ui.spinner();
                }
                State::Failed => {
                    ui.separator();
                    ui.colored_label(egui::Color32::from_rgb(230, 120, 120), "load failed");
                }
                _ => {}
            }
            if app.fast_scrolling() {
                ui.separator();
                ui.weak("scrolling");
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if app.history_len() > 0
                    && ui.button(format!("undo ({})", app.history_len())).clicked()
                {
                    out.action = Some(Action::Undo);
                }
                if app.delete_pending() {
                    ui.colored_label(egui::Color32::from_rgb(240, 140, 140), &app.status);
                } else {
                    ui.label(&app.status);
                }
            });
        });
    });

    // Whatever the panels did not claim is where the image goes.
    out.image_rect = root.available_rect_before_wrap();
    out
}

/// Column widths for the monospaced row layout.
const STEM_W: usize = 20;
const KIND_W: usize = 8;
const SIZE_W: usize = 9;

/// Render one row's text. Split out so the column alignment is testable.
fn row_text(stem: &str, kind: &str, size: &str, when: &str) -> String {
    format!("{stem:<STEM_W$} {kind:<KIND_W$}{size:>SIZE_W$}  {when}")
}

/// One tree row: stem, kind, size and capture time.
///
/// Size and time come from the background stat pass, so they show `-` for the moment
/// after launch before it completes (R23). Only ~40 rows are drawn per frame, so the
/// per-row lookups are a non-issue.
fn row_label(
    ui: &mut egui::Ui,
    group: &Group,
    meta: &MetaStore,
    selected: bool,
    failed: bool,
) -> egui::Response {
    let size = meta
        .group_bytes(group)
        .map_or_else(|| "-".into(), format_size);
    let when = meta
        .group_modified(group)
        .map(format_time)
        .unwrap_or_else(|| "-".into());

    // Monospace keeps the columns aligned without a real table layout.
    let text = row_text(&group.stem, group.kind(), &size, &when);
    let mut rich = egui::RichText::new(text).monospace();
    if failed {
        rich = rich.color(egui::Color32::from_rgb(230, 120, 120));
    } else if group.is_raw_only() {
        // Shown from an embedded preview rather than a sidecar JPEG; worth marking.
        rich = rich.color(egui::Color32::from_rgb(150, 190, 230));
    }

    ui.selectable_label(selected, rich)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Character offset at which the size column ends.
    const SIZE_END: usize = STEM_W + 1 + KIND_W + SIZE_W;

    // --- R21: the tree must follow the selection ---

    /// Realistic egui defaults: 14 px text, 1 px button padding, 3 px item spacing.
    const PITCH: f32 = 19.0;
    const SPACING: f32 = 3.0;
    /// A tall panel, ~26 rows visible.
    const VIEW_H: f32 = 500.0;

    #[test]
    fn visible_row_is_left_alone() {
        // Row 10 sits at 190..209 with the list scrolled to the top; well inside.
        assert_eq!(
            scroll_correction(10, 2000, PITCH, SPACING, VIEW_H, 0.0),
            None,
            "a row already in view must not fight the user's scrolling"
        );
    }

    #[test]
    fn row_below_the_viewport_scrolls_just_far_enough() {
        // Row 40 is far below a viewport showing 0..500.
        let off = scroll_correction(40, 2000, PITCH, SPACING, VIEW_H, 0.0)
            .expect("an off-screen row must be corrected");
        // It should end up two rows clear of the bottom edge, not centred.
        let row_bottom = 41.0 * PITCH;
        assert!(
            (off - (row_bottom + 2.0 * PITCH - VIEW_H)).abs() < 0.01,
            "expected a minimal correction, got {off}"
        );
        // And once applied, the row is genuinely inside the viewport.
        assert!(40.0 * PITCH >= off && row_bottom <= off + VIEW_H);
    }

    #[test]
    fn row_above_the_viewport_scrolls_up() {
        let off = scroll_correction(10, 2000, PITCH, SPACING, VIEW_H, 1000.0)
            .expect("a row above the viewport must be corrected");
        assert!((off - (10.0 * PITCH - 2.0 * PITCH)).abs() < 0.01, "got {off}");
    }

    /// The correction is applied every frame, so it has to converge: after one
    /// correction the row must be visible and the next call must return `None`.
    /// Otherwise the offset is re-asserted forever and the user cannot scroll at all.
    #[test]
    fn correction_settles_after_one_frame() {
        for pos in [0, 1, 37, 500, 1998, 1999] {
            let first = scroll_correction(pos, 2000, PITCH, SPACING, VIEW_H, 0.0);
            let Some(off) = first else { continue };
            assert_eq!(
                scroll_correction(pos, 2000, PITCH, SPACING, VIEW_H, off),
                None,
                "row {pos} did not settle: correction re-applies at offset {off}"
            );
        }
    }

    /// Regression: for rows near the end, `row_bottom + margin` is past the end of the
    /// list, so the requested offset was clamped by egui to the maximum. The condition
    /// then still held next frame, so the correction re-applied on every frame and
    /// pinned the scrollbar -- the list could not be scrolled by hand any more.
    #[test]
    fn last_row_does_not_pin_the_scrollbar() {
        let total = 2000;
        let content_h = PITCH * total as f32 - SPACING;
        let max_off = content_h - VIEW_H;

        // Sitting at the very bottom with the last row selected: nothing to do.
        assert_eq!(
            scroll_correction(total - 1, total, PITCH, SPACING, VIEW_H, max_off),
            None,
            "at the end of travel the correction must stop asking to scroll further"
        );
        // And it never asks to scroll beyond the end.
        for pos in [total - 3, total - 2, total - 1] {
            if let Some(off) = scroll_correction(pos, total, PITCH, SPACING, VIEW_H, 0.0) {
                assert!(off <= max_off + 0.01, "row {pos} scrolled past the end: {off}");
            }
        }
    }

    #[test]
    fn correction_never_goes_negative() {
        // The first rows sit within the margin of the top, so the target is negative
        // before clamping.
        for pos in [0, 1, 2] {
            if let Some(off) = scroll_correction(pos, 2000, PITCH, SPACING, VIEW_H, 300.0) {
                assert!(off >= 0.0, "row {pos} produced a negative offset {off}");
            }
        }
    }

    #[test]
    fn a_list_shorter_than_the_viewport_never_scrolls() {
        // max_off is zero, so every correction clamps to zero and then settles.
        for pos in 0..5 {
            let off = scroll_correction(pos, 5, PITCH, SPACING, VIEW_H, 0.0);
            assert_eq!(off, None, "row {pos} tried to scroll a list that fits");
        }
    }

    /// Row flattening is pure and must keep group indices aligned with the app's
    /// ordering even when directories are collapsed -- the bug that previously made
    /// clicking a row after a collapsed folder select the wrong image.
    #[test]
    fn indices_stay_aligned_when_directories_are_collapsed() {
        use crate::scan;

        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        // Two directories, three groups each.
        for (d, n) in [("a", 3), ("b", 3)] {
            let dir = root.join(d);
            std::fs::create_dir_all(&dir).unwrap();
            for i in 0..n {
                std::fs::write(dir.join(format!("{d}{i}.JPG")), b"x").unwrap();
            }
        }
        let dirs = scan::scan(root);
        let groups = scan::flatten(&dirs);
        assert_eq!(groups.len(), 6);

        // Simulate the flattening directly against scan output, which is what `draw`
        // consumes. Collapsing the first directory must not shift the second's indices.
        let collapse_first: HashSet<PathBuf> = [dirs[0].path.clone()].into_iter().collect();

        let mut base = 0usize;
        let mut seen = Vec::new();
        for node in &dirs {
            if !collapse_first.contains(&node.path) {
                for offset in 0..node.groups.len() {
                    seen.push(base + offset);
                }
            }
            base += node.groups.len();
        }
        // Only the second directory is visible, and it still reports indices 3..6.
        assert_eq!(seen, vec![3, 4, 5]);
        for i in &seen {
            assert!(*i < groups.len());
        }
    }

    /// The columns only line up if every field is padded to a fixed width, so check that
    /// the size column lands at the same offset regardless of stem or kind length.
    #[test]
    fn row_columns_align_across_rows() {
        let rows = [
            row_text("A1", "JPG", "1.0 MB", "2026-08-09 17:44"),
            row_text("A6701135_Export", "JPG+ARW", "41.0 MB", "2026-08-10 10:49"),
            row_text("A6709216", "ARW", "31.8 MB", "-"),
        ];
        for r in &rows {
            // The size field is right-aligned, so it always ends at the same offset.
            let size_field = &r[SIZE_END - SIZE_W..SIZE_END];
            assert!(
                size_field.trim_start().ends_with("MB") || size_field.trim() == "-",
                "size column misaligned in {r:?}: got {size_field:?}"
            );
            // Two spaces separate size from the timestamp.
            assert_eq!(&r[SIZE_END..SIZE_END + 2], "  ", "separator moved in {r:?}");
        }
    }

    /// An over-long stem must not shift the later columns for that row alone; it pushes
    /// them right, which is acceptable, but must not panic or truncate mid-character.
    #[test]
    fn over_long_stem_does_not_panic() {
        let long = "A".repeat(80);
        let r = row_text(&long, "JPG+ARW", "41.0 MB", "2026-08-10 10:49");
        assert!(r.starts_with(&long));
        assert!(r.contains("41.0 MB"));
    }

    #[test]
    fn dir_path_without_file_name_still_labels() {
        // A root path has no file_name; the label must fall back rather than be blank.
        let p = Path::new("/");
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| p.display().to_string());
        assert!(!name.is_empty());
    }
}
