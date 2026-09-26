//! Where the docked tools go in the dock area: the debug overlay top-left, the profiler
//! top-right and the console bottom-left ("stacked": the console under the other two), or, in an
//! area too short for that (a band a game's HUD leaves free), side by side.
//!
//! The console moves over when a tool above it would reach into it:
//!
//! * right of the debug overlay when the overlay reaches down into it;
//! * left of the profiler (which then takes the area's height) when the profiler's smallest
//!   layout with [`MIN_PROFILER_ROWS`] table rows does not fit above it. The profiler narrows (to
//!   [`crate::profiler::MIN_WIDTH`]) before the console gets under [`CONSOLE_ROOM`] pt.
//!
//! With neither tool above it, the console is as tall as the area (up to [`MAX_CONSOLE_HEIGHT`])
//! instead of 40 % of it. When the console would get narrower than [`MIN_CONSOLE_WIDTH`] beside
//! the profiler it stays under it (the profiler shows what rows fit above it); when not even the
//! move right of the overlay leaves it that wide, it keeps its corner and the overlay covers part
//! of it. Every tool system computes the same plan from the same inputs (the tools' open and
//! docked state, the overlay's size, the profiler's measured minimum height, a resized console's
//! size), so the three agree within a frame.

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
    /// The debug overlay's size, when it is open.
    pub overlay: Option<egui::Vec2>,
    /// The profiler, when it is open and docked.
    pub profiler: Option<ProfilerNeeds>,
    /// The console's size in the stacked layout (its default, or the size the user gave it),
    /// when it is open and docked.
    pub console: Option<ConsoleNeeds>,
}

/// The profiler's width and the smallest height it needs above the console.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ProfilerNeeds {
    /// Its width (the default 440 pt, or what the user resized it to).
    pub width: f32,
    /// Its height with [`MIN_PROFILER_ROWS`] table rows in its compact layout.
    pub min_height: f32,
}

/// The console's size in the stacked layout.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ConsoleNeeds {
    /// Its default size (640 pt × 40 % of the area), or the size the user resized it to.
    pub size: egui::Vec2,
    /// The user resized it: it keeps that height wherever it docks.
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
    /// The profiler is beside the console, not above it: its width and the width it must stay
    /// within.
    pub profiler_beside: Option<(f32, f32)>,
}

#[cfg(test)]
impl DockPlan {
    /// The console docks beside another tool (not in the stacked layout).
    fn console_moved(&self) -> bool {
        self.console_pos != self.bounds.left_bottom()
            || self.console_max_width.is_finite()
            || self.profiler_beside.is_some()
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
    };
    let Some(console) = t.console else {
        return stacked;
    };
    let cs = console.size.min(b.size());
    let console_top = b.bottom() - cs.y;
    let overlay = t.overlay.map(|size| egui::Rect::from_min_size(b.left_top(), size));
    // Right of the overlay when the overlay reaches down into it.
    let beside_overlay = overlay.filter(|o| o.bottom() + EDGE > console_top);
    let left = beside_overlay.map_or(b.left(), |o| o.right() + EDGE);
    let room = b.right() - left;

    // The profiler where the stacked layout docks it: top-right, left of the overlay (or under
    // it, on a narrow area).
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
        let over_console = x < left + cs.x.min(room);
        (p, over_console, console_top - EDGE - top >= p.min_height)
    });
    let height = |nothing_above: bool| {
        if nothing_above && !console.user_sized {
            b.height().min(MAX_CONSOLE_HEIGHT)
        } else {
            cs.y
        }
    };

    // Left of the profiler when its smallest layout does not fit above the console. (The
    // profiler, then as tall as the area, stays right of an overlay the console is under.)
    if let Some((p, true, false)) = profiler_above {
        let width = p.width.min(b.width());
        let widest = match overlay {
            Some(o) if beside_overlay.is_none() => width.min(b.right() - o.right() - EDGE),
            _ => width,
        };
        let narrowest = crate::profiler::MIN_WIDTH.min(width);
        let profiler = (room - EDGE - CONSOLE_ROOM).min(widest).max(narrowest);
        let console_width = cs.x.min(room - EDGE - profiler);
        if console_width >= MIN_CONSOLE_WIDTH && widest >= narrowest {
            let nothing_above = beside_overlay.is_some() || overlay.is_none();
            return DockPlan {
                console_pos: egui::pos2(left, b.bottom()),
                console_size: egui::vec2(console_width, height(nothing_above)),
                console_max_width: room - EDGE - profiler,
                profiler_beside: Some((profiler, b.right() - (left + console_width) - EDGE)),
                ..stacked
            };
        }
    }
    if beside_overlay.is_some() && cs.x.min(room) >= MIN_CONSOLE_WIDTH {
        let nothing_above = profiler_above.is_none_or(|(_, over, _)| !over);
        return DockPlan {
            console_pos: egui::pos2(left, b.bottom()),
            console_size: egui::vec2(cs.x.min(room), height(nothing_above)),
            console_max_width: room,
            ..stacked
        };
    }
    stacked
}

/// The [`arrange`]ment of the tools in the dock area `dock` this frame.
pub(crate) fn plan(resources: &Resources, ctx: &egui::Context, dock: egui::Rect) -> DockPlan {
    use crate::tool::{is_floating, is_user_sized};
    let last = |id: &str| ctx.memory(|m| m.area_rect(egui::Id::new(id)));
    let overlay = resources
        .get::<crate::DebugOverlay>()
        .filter(|o| o.visible)
        .and_then(|_| last(crate::debug::OVERLAY_ID))
        .map(|r| r.size());
    let profiler = resources
        .get::<crate::Profiler>()
        .filter(|p| p.visible && !is_floating(ctx, crate::profiler::PROFILER_ID))
        .map(|p| ProfilerNeeds {
            width: match last(crate::profiler::PROFILER_ID) {
                Some(r) if is_user_sized(ctx, crate::profiler::PROFILER_ID) => r.width(),
                _ => crate::profiler::WIDTH,
            },
            min_height: p.min_height(ctx, MIN_PROFILER_ROWS),
        });
    let console = resources
        .get::<crate::Console>()
        .filter(|c| c.visible && !is_floating(ctx, crate::console::CONSOLE_ID))
        .map(|_| match last(crate::console::CONSOLE_ID) {
            Some(r) if is_user_sized(ctx, crate::console::CONSOLE_ID) => ConsoleNeeds {
                size: r.size(),
                user_sized: true,
            },
            _ => ConsoleNeeds {
                size: crate::console::window_rect(dock).size(),
                user_sized: false,
            },
        });
    arrange(&Tools {
        dock,
        overlay,
        profiler,
        console,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const OVERLAY: egui::Vec2 = egui::vec2(213.0, 144.0);
    const PROFILER: ProfilerNeeds = ProfilerNeeds {
        width: 440.0,
        min_height: 150.0,
    };

    fn tools(dock: egui::Rect) -> Tools {
        Tools {
            dock,
            overlay: Some(OVERLAY),
            profiler: Some(PROFILER),
            console: Some(ConsoleNeeds {
                size: crate::console::window_rect(dock).size(),
                user_sized: false,
            }),
        }
    }

    fn area(x: f32, y: f32, w: f32, h: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h))
    }

    /// Normal and tall areas keep the stacked layout.
    #[test]
    fn roomy_areas_stay_stacked() {
        for dock in [
            area(0.0, 0.0, 1280.0, 720.0),
            area(0.0, 0.0, 640.0, 400.0),
            area(210.0, 23.0, 790.0, 697.0),
            area(0.0, 0.0, 390.0, 844.0),
        ] {
            let plan = arrange(&tools(dock));
            assert!(!plan.console_moved(), "{dock:?}: {plan:?}");
        }
    }

    /// The reported band (1024 × 233 pt): overlay | console | profiler.
    #[test]
    fn a_short_band_puts_the_tools_side_by_side() {
        let dock = area(0.0, 300.0, 1024.0, 233.0);
        let plan = arrange(&tools(dock));
        let b = dock.shrink(EDGE);
        assert_eq!(plan.console_pos, egui::pos2(6.0 + OVERLAY.x + EDGE, b.bottom()));
        let (profiler, _) = plan.profiler_beside.expect("the profiler beside the console");
        assert_eq!(profiler, 440.0);
        assert_eq!(plan.console_size.y, b.height(), "nothing above it: the area's height");
        let console_right = plan.console_pos.x + plan.console_size.x;
        assert!(console_right + EDGE <= b.right() - profiler + 0.01, "{plan:?}");
        assert!(plan.console_size.x >= CONSOLE_ROOM, "{plan:?}");
    }

    /// Only the overlay in the way: right of it, under the profiler when that fits.
    #[test]
    fn the_console_moves_right_of_the_overlay() {
        // 640 × 330 pt with the overlay's details (200 pt): the profiler still fits above.
        let mut t = tools(area(0.0, 0.0, 640.0, 330.0));
        t.overlay = Some(egui::vec2(213.0, 200.0));
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
                    t.overlay = overlay;
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
                    let o = overlay.map(|s| egui::Rect::from_min_size(b.left_top(), s));
                    if let Some(o) = o {
                        assert!(!o.expand(EDGE - 0.01).intersects(console), "{label}");
                    }
                    if let Some((width, max)) = plan.profiler_beside {
                        let profiler = egui::Rect::from_min_max(egui::pos2(b.right() - width, b.top()), b.max);
                        assert!(width >= crate::profiler::MIN_WIDTH && max >= width - 0.01, "{label}");
                        assert!(console.right() + EDGE <= profiler.left() + 0.01, "{label}");
                        if let Some(o) = o {
                            assert!(!o.expand(EDGE - 0.01).intersects(profiler), "{label}");
                        }
                    }
                }
            }
        }
        assert!(moved > 100, "{moved}");
    }

    /// Too narrow for the console beside the tools: it keeps its corner.
    #[test]
    fn a_tiny_area_keeps_the_stacked_corners() {
        let plan = arrange(&tools(area(0.0, 0.0, 360.0, 200.0)));
        assert!(!plan.console_moved(), "{plan:?}");
    }
}
