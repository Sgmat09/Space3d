//! Where the docked tools go in the dock area: the debug overlay top-left, the profiler
//! top-right and the console bottom-left ("stacked": the console under the other two), or, in an
//! area too short for that (a band a game's HUD leaves free), side by side.
//!
//! The console moves over when a tool above it would reach into it as drawn (a whole number of
//! log lines tall, up to a line shorter than its default height):
//!
//! * right of the debug overlay when the overlay reaches down into it;
//! * left of the profiler (which then takes the area's height) when the profiler's smallest
//!   layout with [`MIN_PROFILER_ROWS`] table rows (as drawn: without frames, its whole window)
//!   does not fit above it and the console reaches it (closer than a gap). The profiler narrows
//!   (to [`crate::profiler::MIN_WIDTH`]) before the console gets under [`CONSOLE_ROOM`] pt.
//!
//! With neither tool above it, the console is as tall as the area (up to [`MAX_CONSOLE_HEIGHT`])
//! instead of 40 % of it. When the console would get narrower than [`MIN_CONSOLE_WIDTH`] beside
//! the profiler it stays under it (the profiler shows what rows fit above it); when not even the
//! move right of the overlay leaves it that wide, it keeps its corner and the overlay covers part
//! of it.
//!
//! Tools the user resized ([`crate::tool::user_size`]) dock by the same rules at their own
//! sizes, each up to the other: beside the profiler the console stays left of the profiler as
//! drawn (at the width a resized profiler keeps), the profiler right of the console. A profiler
//! that does not fit above the console but is clear of it (a wide area, or a console the user
//! narrowed) keeps its place, and neither is resized into the other. At its limit the console
//! is a gap from the profiler in either layout, so the limit holds (it no longer lifts every
//! other frame) while the console's edge is dragged toward the profiler.
//!
//! The first tool system to run in a frame computes the plan (from the tools' open and docked
//! state, where the overlay is drawn, the profiler's measured minimum height, the console's size
//! and drawn height) and the others take it, so they agree within a frame; what they measure
//! while drawing counts from the next frame. The edges the console docks against beside the
//! others (the area's own left edge too) snap to the pixel grid, so the gaps stay [`EDGE`] wide
//! as drawn.

use crate::tool::EDGE;
use space3d_core::Resources;

/// Table rows the profiler keeps above the console before the console moves beside it.
pub(crate) const MIN_PROFILER_ROWS: usize = 3;

/// Narrowest console beside the other tools (a tool window's minimum width).
pub(crate) const MIN_CONSOLE_WIDTH: f32 = crate::tool::MIN_SIZE.x;

/// Width the console keeps beside the profiler before the profiler gives up any of its 440 pt.
pub(crate) const CONSOLE_ROOM: f32 = 320.0;

/// Tallest console (also its largest default height when stacked).
pub(crate) const MAX_CONSOLE_HEIGHT: f32 = 360.0;

/// What the arrangement needs to know about the open, docked tools.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Tools {
    /// The dock area (the tools stay [`EDGE`] in from its edges).
    pub dock: egui::Rect,
    /// Physical pixels per point (the pixel grid egui draws windows on).
    pub pixels_per_point: f32,
    /// Where the debug overlay is drawn (its top-left corner [`EDGE`] in from the area's, snapped
    /// to the pixel grid), when it is open.
    pub overlay: Option<egui::Rect>,
    /// The profiler, when it is open and docked.
    pub profiler: Option<ProfilerNeeds>,
    /// The console's size in the stacked layout (its default, or the size the user gave it),
    /// when it is open and docked.
    pub console: Option<ConsoleNeeds>,
}

/// The profiler's width and the smallest height it needs above the console.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ProfilerNeeds {
    /// Its width (the default 440 pt, or the width the user gave it).
    pub width: f32,
    /// The user resized it: it keeps its width wherever it docks, up to what is in the way.
    pub user_sized: bool,
    /// Its height with [`MIN_PROFILER_ROWS`] table rows in its compact layout.
    pub min_height: f32,
}

/// The console's size in the stacked layout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ConsoleNeeds {
    /// Its default size (640 pt × 40 % of the area), or the size the user gave it
    /// ([`crate::tool::user_size`]).
    pub size: egui::Vec2,
    /// Its height as drawn at `size` (capped to the area): a whole number of log lines, so up to
    /// a line shorter (at least 3 lines, so taller in a tiny area).
    pub height: f32,
    /// The user resized it: it keeps that size wherever it docks, up to what is in the way.
    pub user_sized: bool,
}

/// Where the console and the profiler dock this frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DockPlan {
    /// The dock area, [`EDGE`] in.
    pub bounds: egui::Rect,
    /// Where the console's bottom-left corner docks.
    pub console_pos: egui::Pos2,
    /// The console's default size (it takes it while the user has not resized it).
    pub console_size: egui::Vec2,
    /// Width the console must stay within (a tool beside it), or infinity.
    pub console_max_width: f32,
    /// The profiler is beside the console, as tall as the area, at this width (instead of above
    /// it).
    pub profiler_beside: Option<f32>,
    /// Width the profiler must stay within (the console beside it), or infinity.
    pub profiler_max_width: f32,
}

#[cfg(test)]
impl DockPlan {
    /// The console docks beside another tool (not in the stacked layout).
    fn console_moved(&self) -> bool {
        self.console_pos != self.bounds.left_bottom() || self.profiler_beside.is_some()
    }
}

/// Where the tools dock in `t.dock` (see the module docs).
pub(crate) fn arrange(t: &Tools) -> DockPlan {
    let b = t.dock.shrink(EDGE);
    let stacked = DockPlan {
        bounds: b,
        console_pos: b.left_bottom(),
        console_size: crate::console::window_rect(t.dock).size(),
        console_max_width: f32::INFINITY,
        profiler_beside: None,
        profiler_max_width: f32::INFINITY,
    };
    let Some(console) = t.console else {
        return stacked;
    };
    let ppp = t.pixels_per_point;
    let cs = console.size.min(b.size());
    // (Where it is drawn: its log is a whole number of lines.)
    let console_top = b.bottom() - console.height;
    let overlay = t.overlay;
    // Right of the overlay when the overlay reaches down into it. Beside another tool its left
    // edge is on the pixel grid (as egui draws a window's corner), so the gaps stay whole as
    // drawn.
    let beside_overlay = overlay.filter(|o| o.bottom() + EDGE > console_top);
    let left = ceil_to_pixels(beside_overlay.map_or(b.left(), |o| o.right() + EDGE), ppp);
    let room = b.right() - left;

    // The profiler where the stacked layout docks it (top-right, left of the overlay, or under
    // it on a narrow area): its left edge, and whether its smallest layout fits above the
    // console.
    let profiler_above = t.profiler.map(|p| {
        let width = p.width.min(b.width());
        let (mut x, mut top) = (b.right() - width, b.top());
        if let Some(o) = overlay
            && x < o.right() + EDGE
        {
            if b.right() - o.right() - EDGE >= crate::profiler::MIN_WIDTH {
                x = o.right() + EDGE;
            } else {
                top = o.bottom() + EDGE;
            }
        }
        (p, x, console_top - EDGE - top >= p.min_height)
    });
    // Whether a console `width` wide whose left edge is at `console_left` (on the pixel grid)
    // reaches a profiler whose left edge is at `x`: they are closer than a gap, as drawn. (One
    // the user resized up to the other is a gap from it either way: see below.)
    let reaches =
        |x: f32, console_left: f32, width: f32| floor_to_pixels(x, ppp) - (console_left + width) < EDGE - 0.01;
    let height = |nothing_above: bool| {
        if nothing_above && !console.user_sized {
            b.height().min(MAX_CONSOLE_HEIGHT)
        } else {
            cs.y
        }
    };

    // Left of the profiler when its smallest layout does not fit above the console and the
    // console reaches it. (The profiler, then as tall as the area, stays right of an overlay the
    // console is under.)
    if let Some((p, x, false)) = profiler_above
        && reaches(x, left, cs.x.min(room))
    {
        let width = p.width.min(b.width());
        let clear_of_overlay = match overlay {
            Some(o) if beside_overlay.is_none() => b.right() - o.right() - EDGE,
            _ => f32::INFINITY,
        };
        let widest = width.min(clear_of_overlay);
        let narrowest = crate::profiler::MIN_WIDTH.min(width);
        let profiler = (room - EDGE - CONSOLE_ROOM).min(widest).max(narrowest);
        // Up to the profiler's left edge as egui draws it (its corner rounded to the pixel grid).
        let console_room = floor_to_pixels(b.right() - profiler, ppp) - EDGE - left;
        let console_width = cs.x.min(console_room);
        if console_width >= MIN_CONSOLE_WIDTH && widest >= narrowest {
            let profiler_max = (b.right() - (left + console_width) - EDGE).min(clear_of_overlay);
            // A profiler the user resized keeps its width up to the console, and the console
            // stays left of it as drawn.
            let drawn = if p.user_sized {
                width.min(profiler_max).max(profiler)
            } else {
                profiler
            };
            let nothing_above = beside_overlay.is_some() || overlay.is_none();
            return DockPlan {
                console_pos: egui::pos2(left, b.bottom()),
                console_size: egui::vec2(console_width, height(nothing_above)),
                console_max_width: floor_to_pixels(b.right() - drawn, ppp) - EDGE - left,
                profiler_beside: Some(profiler),
                profiler_max_width: profiler_max,
                ..stacked
            };
        }
    }
    let mut plan = stacked;
    let mut console_left = ceil_to_pixels(b.left(), ppp);
    let mut console_width = cs.x;
    if beside_overlay.is_some() && cs.x.min(room) >= MIN_CONSOLE_WIDTH {
        console_left = left;
        console_width = cs.x.min(room);
        let nothing_above = profiler_above.is_none_or(|(_, x, _)| !reaches(x, left, console_width));
        plan = DockPlan {
            console_pos: egui::pos2(left, b.bottom()),
            console_size: egui::vec2(console_width, height(nothing_above)),
            console_max_width: room,
            ..stacked
        };
    }
    // A profiler that does not fit above the console, clear of it (a wide area, or a console
    // the user narrowed), is beside it all the same: neither is resized into the other.
    if let Some((_, x, false)) = profiler_above
        && !reaches(x, console_left, console_width)
    {
        plan.console_max_width = plan
            .console_max_width
            .min(floor_to_pixels(x, ppp) - EDGE - console_left);
        plan.profiler_max_width = b.right() - (console_left + console_width) - EDGE;
    }
    plan
}

/// `x` rounded up to the pixel grid.
fn ceil_to_pixels(x: f32, pixels_per_point: f32) -> f32 {
    // (A hair of tolerance: a value on the grid stays there.)
    (x * pixels_per_point - 1e-3).ceil() / pixels_per_point
}

/// `x` rounded down to the pixel grid.
fn floor_to_pixels(x: f32, pixels_per_point: f32) -> f32 {
    (x * pixels_per_point + 1e-3).floor() / pixels_per_point
}

/// The [`arrange`]ment of the tools in the dock area `dock` this frame: computed by the first
/// tool system that asks in a frame (pass), the same for the others.
pub(crate) fn plan(resources: &Resources, ctx: &egui::Context, dock: egui::Rect) -> DockPlan {
    let id = egui::Id::new("space3d_ui::dock_plan");
    let pass = ctx.cumulative_pass_nr();
    if let Some((p, d, plan)) = ctx.data(|d| d.get_temp::<(u64, egui::Rect, DockPlan)>(id))
        && p == pass
        && d == dock
    {
        return plan;
    }
    let plan = arrange(&tools(resources, ctx, dock));
    ctx.data_mut(|d| d.insert_temp(id, (pass, dock, plan)));
    plan
}

/// What [`arrange`] needs to know about the tools this frame.
fn tools(resources: &Resources, ctx: &egui::Context, dock: egui::Rect) -> Tools {
    use crate::tool::{is_floating, user_size};
    let overlay = resources
        .get::<crate::DebugOverlay>()
        .filter(|o| o.visible)
        .and_then(|_| ctx.memory(|m| m.area_rect(egui::Id::new(crate::debug::OVERLAY_ID))))
        .map(|r| {
            // Where it is docked, as egui places an area (its corner rounded to the pixel grid).
            // (Not where egui last put it: on its first frame egui sizes it somewhere else.)
            use egui::emath::GuiRounding as _;
            let corner = (dock.min + egui::Vec2::splat(EDGE)).round_to_pixels(ctx.pixels_per_point());
            egui::Rect::from_min_size(corner.round_ui(), r.size())
        });
    let profiler = resources
        .get::<crate::Profiler>()
        .filter(|p| p.visible && !is_floating(ctx, crate::profiler::PROFILER_ID))
        .map(|p| {
            let user = user_size(ctx, crate::profiler::PROFILER_ID);
            ProfilerNeeds {
                width: user.map_or(crate::profiler::WIDTH, |s| s.x),
                user_sized: user.is_some(),
                min_height: p.min_height(ctx, MIN_PROFILER_ROWS),
            }
        });
    let console = resources
        .get::<crate::Console>()
        .filter(|c| c.visible && !is_floating(ctx, crate::console::CONSOLE_ID))
        .map(|c| {
            // (A window gets at most the area's height, at least a tool window's.)
            let fit = |h: f32| h.min(dock.height() - 2.0 * EDGE).max(crate::tool::MIN_SIZE.y);
            let user = user_size(ctx, crate::console::CONSOLE_ID);
            // (As the user left it, capped to the area.)
            let size = user.unwrap_or_else(|| crate::console::window_rect(dock).size());
            ConsoleNeeds {
                size,
                height: c.drawn_height(fit(size.y)),
                user_sized: user.is_some(),
            }
        });
    Tools {
        dock,
        pixels_per_point: ctx.pixels_per_point(),
        overlay,
        profiler,
        console,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OVERLAY: egui::Vec2 = egui::vec2(213.0, 144.0);
    const PROFILER: ProfilerNeeds = ProfilerNeeds {
        width: 440.0,
        user_sized: false,
        min_height: 150.0,
    };
    /// The console's log line, the height of its window around the log and what the rest of it
    /// reserves, at 1x (`Console::drawn_height`).
    const LINE: f32 = 13.0;
    const AROUND: f32 = 86.0;
    const RESERVED: f32 = 92.0;

    /// The console's outer height when given `h`: a whole number of lines, at least 3.
    fn drawn(h: f32) -> f32 {
        AROUND + ((h - RESERVED) / LINE + 0.01).floor().max(3.0) * LINE
    }

    /// The overlay (`size`) where it is drawn in `dock` at 1x.
    fn overlay_in(dock: egui::Rect, size: egui::Vec2) -> egui::Rect {
        egui::Rect::from_min_size((dock.min + egui::Vec2::splat(EDGE)).round(), size)
    }

    fn console(dock: egui::Rect) -> ConsoleNeeds {
        let size = crate::console::window_rect(dock).size();
        ConsoleNeeds {
            size,
            height: drawn(size.y),
            user_sized: false,
        }
    }

    fn tools(dock: egui::Rect) -> Tools {
        Tools {
            dock,
            pixels_per_point: 1.0,
            overlay: Some(overlay_in(dock, OVERLAY)),
            profiler: Some(PROFILER),
            console: Some(console(dock)),
        }
    }

    fn area(x: f32, y: f32, w: f32, h: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h))
    }

    /// Normal and tall areas keep the stacked layout, and so do short ones where it fits as the
    /// tools are drawn: the 1100 × 300 pt band (the console is drawn 125 pt tall, not its default
    /// 140 pt: the overlay ends 19 pt above it) and a 640 × 330 pt screen with a profiler that
    /// needs 180 pt (187 pt are free above the drawn console).
    #[test]
    fn roomy_areas_stay_stacked() {
        for dock in [
            area(0.0, 0.0, 1280.0, 720.0),
            area(0.0, 0.0, 640.0, 400.0),
            area(210.0, 23.0, 790.0, 697.0),
            area(0.0, 0.0, 390.0, 844.0),
            area(0.0, 300.0, 1100.0, 300.0),
        ] {
            let plan = arrange(&tools(dock));
            assert!(!plan.console_moved(), "{dock:?}: {plan:?}");
        }
        let dock = area(0.0, 0.0, 640.0, 330.0);
        let mut t = tools(dock);
        t.profiler = Some(ProfilerNeeds {
            min_height: 180.0,
            ..PROFILER
        });
        assert!(!arrange(&t).console_moved(), "{:?}", arrange(&t));
    }

    /// The plan goes by the console's drawn height: taller (its default height, or a user's), the
    /// overlay and the profiler above reach into it.
    #[test]
    fn the_plan_uses_the_consoles_drawn_height() {
        for (dock, min_height) in [
            (area(0.0, 300.0, 1100.0, 300.0), 150.0),
            (area(0.0, 0.0, 640.0, 330.0), 180.0),
        ] {
            let mut t = tools(dock);
            t.profiler = Some(ProfilerNeeds { min_height, ..PROFILER });
            let c = t.console.unwrap();
            assert!(c.height < c.size.y, "{c:?}");
            assert!(!arrange(&t).console_moved(), "{dock:?}");
            t.console = Some(ConsoleNeeds { height: c.size.y, ..c });
            assert!(arrange(&t).console_moved(), "{dock:?}");
        }
    }

    /// The reported band (1024 × 233 pt): overlay | console | profiler.
    #[test]
    fn a_short_band_puts_the_tools_side_by_side() {
        let dock = area(0.0, 300.0, 1024.0, 233.0);
        let plan = arrange(&tools(dock));
        let b = dock.shrink(EDGE);
        assert_eq!(plan.console_pos, egui::pos2(6.0 + OVERLAY.x + EDGE, b.bottom()));
        let profiler = plan.profiler_beside.expect("the profiler beside the console");
        assert_eq!(profiler, 440.0);
        assert_eq!(plan.console_size.y, b.height(), "nothing above it: the area's height");
        let console_right = plan.console_pos.x + plan.console_size.x;
        assert!(console_right + EDGE <= b.right() - profiler + 0.01, "{plan:?}");
        assert!(plan.console_size.x >= CONSOLE_ROOM, "{plan:?}");
    }

    /// A profiler without frames yet is only its message (about 76 pt): it stays above the
    /// console, which moves right of the overlay only.
    #[test]
    fn a_profiler_without_frames_stays_above_the_console() {
        let dock = area(0.0, 300.0, 1024.0, 233.0);
        let mut t = tools(dock);
        t.profiler = Some(ProfilerNeeds {
            min_height: 76.0,
            ..PROFILER
        });
        let plan = arrange(&t);
        assert!(plan.profiler_beside.is_none(), "{plan:?}");
        assert_eq!(plan.console_pos.x, 6.0 + OVERLAY.x + EDGE);
    }

    /// Only the overlay in the way: right of it, under the profiler when that fits.
    #[test]
    fn the_console_moves_right_of_the_overlay() {
        // 640 × 330 pt with the overlay's details (200 pt): the profiler still fits above.
        let dock = area(0.0, 0.0, 640.0, 330.0);
        let mut t = tools(dock);
        t.overlay = Some(overlay_in(dock, egui::vec2(213.0, 200.0)));
        let plan = arrange(&t);
        assert_eq!(plan.console_pos.x, 6.0 + 213.0 + EDGE);
        assert!(plan.profiler_beside.is_none(), "{plan:?}");
        assert_eq!(
            plan.console_size.y, 140.0,
            "the profiler is above it: its default height"
        );
        // Without the profiler it takes the area's height.
        t.profiler = None;
        assert_eq!(arrange(&t).console_size.y, 318.0);
    }

    /// Whatever the area, a plan that moves the console keeps the tools apart: the console right
    /// of the overlay it moved beside and at least 160 pt wide, the profiler beside the console
    /// right of both and at least 240 pt wide.
    #[test]
    fn moved_tools_never_overlap() {
        let mut moved = 0;
        for w in (300..2000).step_by(20) {
            for h in (120..900).step_by(20) {
                for (overlay, details) in [
                    (Some(OVERLAY), false),
                    (Some(egui::vec2(213.0, 200.0)), true),
                    (None, false),
                ] {
                    let dock = area(0.0, 0.0, w as f32, h as f32);
                    let mut t = tools(dock);
                    t.overlay = overlay.map(|s| overlay_in(dock, s));
                    let plan = arrange(&t);
                    if !plan.console_moved() {
                        continue;
                    }
                    moved += 1;
                    let label = format!("{w} x {h}, details {details}, overlay {overlay:?}: {plan:?}");
                    let b = dock.shrink(EDGE);
                    let console = egui::Rect::from_min_max(
                        egui::pos2(plan.console_pos.x, plan.console_pos.y - plan.console_size.y),
                        egui::pos2(plan.console_pos.x + plan.console_size.x, plan.console_pos.y),
                    );
                    assert!(b.expand(0.01).contains_rect(console), "{label}");
                    assert!(console.width() >= MIN_CONSOLE_WIDTH, "{label}");
                    if let Some(o) = t.overlay {
                        assert!(!o.expand(EDGE - 0.01).intersects(console), "{label}");
                    }
                    if let Some(width) = plan.profiler_beside {
                        let max = plan.profiler_max_width;
                        let profiler = egui::Rect::from_min_max(egui::pos2(b.right() - width, b.top()), b.max);
                        assert!(width >= crate::profiler::MIN_WIDTH && max >= width - 0.01, "{label}");
                        assert!(console.right() + EDGE <= profiler.left() + 0.01, "{label}");
                        if let Some(o) = t.overlay {
                            assert!(!o.expand(EDGE - 0.01).intersects(profiler), "{label}");
                        }
                    }
                }
            }
        }
        assert!(moved > 100, "{moved}");
    }

    /// On a dock area off the pixel grid the edges the console docks against beside the others
    /// snap to it, so the gaps stay whole as egui draws the windows (their corners rounded to
    /// pixels): the overlay drawn half a pixel right of the area's corner, the profiler's corner
    /// rounded either way, and without the overlay the console's own corner (egui rounded the
    /// area's 43.5 pt to 44 pt: 5.5 pt were left to the profiler).
    #[test]
    fn gaps_stay_whole_on_the_pixel_grid() {
        for ppp in [1.0f32, 1.25, 1.5, 2.0] {
            for (x, y, w, h) in [
                (37.5, 123.25, 1011.5, 233.5),
                (0.3, 300.7, 1024.2, 233.0),
                (10.6, 0.0, 1100.9, 250.0),
                (0.0, 0.0, 660.0, 240.0),
            ] {
                for overlay in [true, false] {
                    let dock = area(x, y, w, h);
                    let b = dock.shrink(EDGE);
                    let px = |v: f32| (v * ppp).round() / ppp;
                    let mut t = tools(dock);
                    t.pixels_per_point = ppp;
                    let corner = dock.min + egui::Vec2::splat(EDGE);
                    t.overlay = overlay.then(|| {
                        egui::Rect::from_min_size(egui::pos2(px(corner.x), px(corner.y)), egui::vec2(212.69, 144.0))
                    });
                    let plan = arrange(&t);
                    let label = format!("{ppp}x {dock:?}, overlay {overlay}: {plan:?}");
                    // (Where egui draws the console's corner: rounded to the grid.)
                    let left = px(plan.console_pos.x);
                    if plan.console_moved() {
                        assert!((px(left) - plan.console_pos.x).abs() < 1e-3, "on the grid: {label}");
                    }
                    // (egui rounds a stacked console's corner either way.)
                    let slack = if plan.console_moved() { 1e-3 } else { 0.5 / ppp };
                    assert!(left >= b.left() - slack, "{label}");
                    if let Some(o) = t.overlay {
                        assert!(left - o.right() >= EDGE - 1e-3, "{label}");
                    }
                    // The profiler as egui draws it, beside the console or top-right above it.
                    let profiler_left = px(b.right() - plan.profiler_beside.unwrap_or(PROFILER.width));
                    if plan.console_max_width.is_finite() {
                        assert!(left + plan.console_max_width + EDGE <= profiler_left + 1e-3, "{label}");
                    } else {
                        assert!(
                            overlay && x == 10.6,
                            "only the 1100 pt band fits the stacked layout: {label}"
                        );
                    }
                    if plan.profiler_beside.is_some() {
                        assert!(profiler_left - (left + plan.console_size.x) >= EDGE - 1e-3, "{label}");
                    }
                }
            }
        }
    }

    /// A console the user sized docks by the same limit whatever its width: widened up to the
    /// profiler, the plan at the width it is then drawn at keeps the limit that stopped it (at
    /// the limit the profiler no longer overlapped it, which lifted the limit every other frame
    /// of the drag: the console was drawn over the profiler). Its limit keeps it a gap from the
    /// profiler as drawn, beside it or top-right above it.
    #[test]
    fn the_consoles_limit_holds_as_it_reaches_the_profiler() {
        for ppp in [1.0f32, 1.5, 2.0] {
            for dock in [
                area(0.0, 300.0, 1024.0, 233.0),
                area(0.0, 0.0, 800.0, 200.0),
                area(0.0, 0.0, 1920.0, 260.0),
                area(37.5, 123.25, 1011.5, 233.5),
            ] {
                for overlay in [true, false] {
                    let b = dock.shrink(EDGE);
                    let mut t = tools(dock);
                    t.pixels_per_point = ppp;
                    if !overlay {
                        t.overlay = None;
                    }
                    let with_width = |w: f32| {
                        let console = t.console.map(|c| ConsoleNeeds {
                            size: egui::vec2(w, c.size.y),
                            user_sized: true,
                            ..c
                        });
                        arrange(&Tools { console, ..t })
                    };
                    for w in (160..1900).step_by(3) {
                        let plan = with_width(w as f32);
                        let label = format!("{ppp}x {dock:?}, overlay {overlay}, width {w}: {plan:?}");
                        let left = ceil_to_pixels(plan.console_pos.x, ppp);
                        let drawn = (w as f32).min(plan.console_max_width).min(b.right() - left);
                        let again = with_width(drawn);
                        let profiler_left = |plan: &DockPlan| {
                            floor_to_pixels(b.right() - plan.profiler_beside.unwrap_or(PROFILER.width), ppp)
                        };
                        assert_eq!(again.console_max_width, plan.console_max_width, "{label}");
                        assert_eq!(profiler_left(&again), profiler_left(&plan), "{label}");
                        let profiler_left = profiler_left(&plan);
                        assert!(left + plan.console_max_width + EDGE <= profiler_left + 1e-3, "{label}");
                        assert!(
                            b.right() - plan.profiler_max_width >= left + drawn + EDGE - 1e-3,
                            "the profiler stays clear of it: {label}"
                        );
                    }
                }
            }
        }
    }

    /// A profiler the user resized keeps its width beside the console, up to the console (or
    /// the overlay the console is under); the console keeps a gap from the profiler at the width
    /// the profiler keeps, and a wide area's stacked tools stay clear of each other too.
    #[test]
    fn a_resized_profiler_and_the_console_stay_apart() {
        for ppp in [1.0f32, 2.0] {
            for dock in [
                area(0.0, 300.0, 1024.0, 233.0),
                area(0.0, 0.0, 1920.0, 260.0),
                area(0.0, 0.0, 1100.0, 280.0),
            ] {
                for overlay in [Some(OVERLAY), Some(egui::vec2(213.0, 60.0)), None] {
                    for console_width in [None, Some(200.0), Some(300.0), Some(900.0)] {
                        for profiler_width in (240..1000).step_by(9) {
                            let b = dock.shrink(EDGE);
                            let mut t = tools(dock);
                            t.pixels_per_point = ppp;
                            t.overlay = overlay.map(|s| overlay_in(dock, s));
                            t.profiler = Some(ProfilerNeeds {
                                width: profiler_width as f32,
                                user_sized: true,
                                ..PROFILER
                            });
                            if let (Some(w), Some(c)) = (console_width, t.console.as_mut()) {
                                c.size.x = w;
                                c.user_sized = true;
                            }
                            let plan = arrange(&t);
                            let label = format!(
                                "{ppp}x {dock:?}, overlay {overlay:?}, console {console_width:?}, profiler {profiler_width}: {plan:?}"
                            );
                            let left = ceil_to_pixels(plan.console_pos.x, ppp);
                            let cs = t.console.unwrap().size.x.min(b.width());
                            let console_right = left + cs.min(plan.console_max_width).min(b.right() - left);
                            // The profiler at the width it keeps (and at the plan's width, for
                            // one the user did not resize).
                            let mut profiler = (profiler_width as f32).min(plan.profiler_max_width).min(b.width());
                            if let Some(planned) = plan.profiler_beside {
                                profiler = profiler.max(planned);
                                assert!(plan.profiler_max_width >= planned - 0.01, "{label}");
                            } else if let Some(o) = t.overlay
                                && b.right() - profiler < o.right() + EDGE
                            {
                                // (Top-right, it stays right of the overlay above the console.)
                                profiler = profiler.min(b.right() - o.right() - EDGE);
                            }
                            let profiler_left = floor_to_pixels(b.right() - profiler, ppp);
                            let profiler_in_rows =
                                plan.profiler_beside.is_some() || plan.profiler_max_width.is_finite();
                            if profiler_in_rows {
                                assert!(console_right + EDGE <= profiler_left + 1e-3, "{label}");
                                assert!(left + plan.console_max_width + EDGE <= profiler_left + 1e-3, "{label}");
                            }
                            if let (Some(o), Some(_)) = (t.overlay, plan.profiler_beside) {
                                let p = egui::Rect::from_min_max(egui::pos2(b.right() - profiler, b.top()), b.max);
                                assert!(!o.expand(EDGE - 0.01).intersects(p), "clear of the overlay: {label}");
                            }
                        }
                    }
                }
            }
        }
    }

    /// Too narrow for the console beside the tools: it keeps its corner.
    #[test]
    fn a_tiny_area_keeps_the_stacked_corners() {
        let plan = arrange(&tools(area(0.0, 0.0, 360.0, 200.0)));
        assert!(!plan.console_moved(), "{plan:?}");
    }
}
