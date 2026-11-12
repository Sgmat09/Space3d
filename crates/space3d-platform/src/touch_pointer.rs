//! Fingers as a mouse ([`WindowPlugin::touch_as_mouse`](crate::WindowPlugin::touch_as_mouse)):
//! what a game whose controls and UI read the mouse gets from a touch screen.
//!
//! | gesture | the mouse |
//! |---|---|
//! | finger down | the pointer is there (a hover); the left button goes down on the **next frame** |
//! | tap | left press and release, a frame each, then the pointer leaves |
//! | drag | a left drag |
//! | long press (still for [`TouchPointerConfig::long_press`]) | a right click where the finger is; the left button stays down |
//! | two fingers, pinch | the wheel: one line per [`TouchPointerConfig::pinch_step`] of spread (apart is up), the pointer between the fingers |
//!
//! The press waits a frame because a mouse hovers before it clicks and a finger does not: a UI
//! that decides whose click it is from where the pointer was at its last pass (egui's "is the
//! pointer over a window") would hand a tap on a button to the world behind it. For the same
//! reason a tap's release comes a frame after its press, and the pointer leaves a frame after
//! that, so nothing stays hovered where the finger was.
//!
//! A second finger ends the first one's press as a cancel (no click), and when one finger of a
//! pinch lifts the other does nothing until it lifts too.
//!
//! [`TouchPointer`] is the state machine alone (no window, no clock): the runner feeds it the
//! touches and a call per frame and applies the [`PointerAction`]s to [`Input`](crate::Input).

use crate::input::{MouseButton, TouchPhase};
use glam::Vec2;

/// The thresholds of [`TouchPointer`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TouchPointerConfig {
    /// Physical pixels a finger may wander and still be a tap or a long press. The runner sets
    /// it to [`Self::SLOP_POINTS`] of the window's scale.
    pub slop: f32,
    /// Seconds a still finger is down before the right click. Default 0.5.
    pub long_press: f64,
    /// The ratio of the fingers' distance that is one wheel line. Default 1.05 (twice as far
    /// apart: about 14 lines).
    pub pinch_step: f32,
}

impl TouchPointerConfig {
    /// The slop in the window's own points (multiply by the scale factor for pixels).
    pub const SLOP_POINTS: f32 = 10.0;
}

impl Default for TouchPointerConfig {
    fn default() -> Self {
        Self {
            slop: Self::SLOP_POINTS,
            long_press: 0.5,
            pinch_step: 1.05,
        }
    }
}

/// What the mouse does ([`TouchPointer`]), in order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointerAction {
    /// The pointer appears at a position (no movement from where it was).
    Place(Vec2),
    /// The pointer moves to a position.
    Move(Vec2),
    /// The pointer is gone.
    Leave,
    /// A button goes down.
    Down(MouseButton),
    /// A button comes up.
    Up(MouseButton),
    /// A button comes up without a click (a second finger, a cancelled touch).
    Cancel(MouseButton),
    /// The wheel, in lines (up is positive).
    Wheel(f32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    /// No finger that counts.
    Idle,
    /// One finger is the pointer.
    One {
        id: u64,
        start: Vec2,
        since: f64,
        /// It left the slop: a drag, no long press any more.
        moved: bool,
        /// The long press's right click was sent.
        long: bool,
    },
    /// Two fingers pinch.
    Pinch { a: (u64, Vec2), b: (u64, Vec2) },
    /// A gesture is over but fingers are still down: nothing until all are lifted.
    Lifting,
}

/// The state machine of fingers as a mouse (see the module).
#[derive(Clone, Debug)]
pub struct TouchPointer {
    /// The thresholds.
    pub config: TouchPointerConfig,
    state: State,
    /// Fingers down, whatever they do.
    fingers: Vec<u64>,
    /// Where the one finger that is the pointer was last.
    last: Option<Vec2>,
    /// The left button is down for the game.
    left: bool,
    /// The left press waits for the next frame.
    press_due: bool,
    /// The left release waits for the frame after the press (`true`: a cancel).
    release_due: Option<bool>,
    /// The right button of a long press comes up on the next frame.
    right_due: bool,
    /// The pointer leaves on the next frame with nothing else to do.
    leave_due: bool,
}

impl Default for TouchPointer {
    fn default() -> Self {
        Self::new(TouchPointerConfig::default())
    }
}

impl TouchPointer {
    /// A machine with `config`.
    pub fn new(config: TouchPointerConfig) -> Self {
        Self {
            config,
            state: State::Idle,
            fingers: Vec::new(),
            last: None,
            left: false,
            press_due: false,
            release_due: None,
            right_due: false,
            leave_due: false,
        }
    }

    /// A touch event at `now` (seconds on any clock that [`Self::frame`] shares).
    pub fn touch(&mut self, id: u64, phase: TouchPhase, p: Vec2, now: f64, out: &mut Vec<PointerAction>) {
        match phase {
            TouchPhase::Started => {
                if !self.fingers.contains(&id) {
                    self.fingers.push(id);
                }
                match self.state {
                    State::Idle => {
                        // A tap right after a tap: the first one's press and release are not lost.
                        self.flush(out);
                        self.leave_due = false;
                        out.push(PointerAction::Place(p));
                        self.press_due = true;
                        self.state = State::One {
                            id,
                            start: p,
                            since: now,
                            moved: false,
                            long: false,
                        };
                    }
                    State::One { id: first, start, .. } if first != id => {
                        // The first finger was no click.
                        self.press_due = false;
                        self.release_due = None;
                        if self.left {
                            self.left = false;
                            out.push(PointerAction::Cancel(MouseButton::Left));
                        }
                        let a = (first, self.last.unwrap_or(start));
                        self.state = State::Pinch { a, b: (id, p) };
                        out.push(PointerAction::Place((a.1 + p) * 0.5));
                    }
                    _ => {}
                }
            }
            TouchPhase::Moved => match &mut self.state {
                State::One {
                    id: first,
                    start,
                    moved,
                    ..
                } if *first == id => {
                    if start.distance(p) > self.config.slop {
                        *moved = true;
                    }
                    self.last = Some(p);
                    out.push(PointerAction::Move(p));
                }
                State::Pinch { a, b } if a.0 == id || b.0 == id => {
                    let before = a.1.distance(b.1);
                    if a.0 == id {
                        a.1 = p;
                    } else {
                        b.1 = p;
                    }
                    let after = a.1.distance(b.1);
                    out.push(PointerAction::Place((a.1 + b.1) * 0.5));
                    if before > 1.0 && after > 1.0 && self.config.pinch_step > 1.0 {
                        let lines = (after / before).ln() / self.config.pinch_step.ln();
                        if lines != 0.0 {
                            out.push(PointerAction::Wheel(lines));
                        }
                    }
                }
                _ => {}
            },
            TouchPhase::Ended | TouchPhase::Cancelled => {
                self.fingers.retain(|f| *f != id);
                match self.state {
                    State::One { id: first, .. } if first == id => {
                        self.last = None;
                        out.push(PointerAction::Move(p));
                        if phase == TouchPhase::Cancelled {
                            // Not a click: a press that was still waiting never happens.
                            if self.press_due {
                                self.press_due = false;
                            } else {
                                self.release_due = Some(true);
                            }
                        } else {
                            self.release_due = Some(false);
                        }
                        self.state = State::Idle;
                        self.leave_due = true;
                    }
                    State::Pinch { a, b } if a.0 == id || b.0 == id => {
                        self.last = None;
                        self.state = if self.fingers.is_empty() {
                            State::Idle
                        } else {
                            State::Lifting
                        };
                        self.leave_due = true;
                    }
                    State::Lifting if self.fingers.is_empty() => self.state = State::Idle,
                    _ => {}
                }
            }
        }
    }

    /// Once a frame, before the game runs: what waited for this frame, and the long press.
    pub fn frame(&mut self, now: f64, out: &mut Vec<PointerAction>) {
        if self.right_due {
            self.right_due = false;
            out.push(PointerAction::Up(MouseButton::Right));
        }
        if self.press_due {
            self.press_due = false;
            self.left = true;
            out.push(PointerAction::Down(MouseButton::Left));
            // Its release, if the finger is up already, is the next frame's.
            return;
        }
        if let Some(cancel) = self.release_due.take() {
            self.release(cancel, out);
            return;
        }
        if let State::One { since, moved, long, .. } = &mut self.state
            && self.left
            && !*moved
            && !*long
            && now - *since >= self.config.long_press
        {
            *long = true;
            self.right_due = true;
            out.push(PointerAction::Down(MouseButton::Right));
            return;
        }
        if self.leave_due && self.state == State::Idle {
            self.leave_due = false;
            out.push(PointerAction::Leave);
        }
    }

    /// Whether a finger is the pointer or a gesture is going on.
    pub fn active(&self) -> bool {
        self.state != State::Idle || self.press_due || self.release_due.is_some() || self.right_due || self.leave_due
    }

    fn release(&mut self, cancel: bool, out: &mut Vec<PointerAction>) {
        if self.left {
            self.left = false;
            out.push(if cancel {
                PointerAction::Cancel(MouseButton::Left)
            } else {
                PointerAction::Up(MouseButton::Left)
            });
        }
    }

    /// Everything that waited for a frame, now (a new finger is down before that frame came).
    fn flush(&mut self, out: &mut Vec<PointerAction>) {
        if self.right_due {
            self.right_due = false;
            out.push(PointerAction::Up(MouseButton::Right));
        }
        if self.press_due {
            self.press_due = false;
            self.left = true;
            out.push(PointerAction::Down(MouseButton::Left));
        }
        if let Some(cancel) = self.release_due.take() {
            self.release(cancel, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PointerAction::*;
    use super::*;
    use crate::input::MouseButton::{Left, Right};
    use TouchPhase::{Cancelled, Ended, Moved, Started};

    const P: Vec2 = Vec2::new(100.0, 200.0);

    /// The actions of `steps` (a touch, or `None` for a frame at that time), one list per step.
    fn run(t: &mut TouchPointer, steps: &[(f64, Option<(u64, TouchPhase, Vec2)>)]) -> Vec<Vec<PointerAction>> {
        steps
            .iter()
            .map(|(now, touch)| {
                let mut out = Vec::new();
                match touch {
                    Some((id, phase, p)) => t.touch(*id, *phase, *p, *now, &mut out),
                    None => t.frame(*now, &mut out),
                }
                out
            })
            .collect()
    }

    /// A tap whose down and up arrive between two frames (`adb shell input tap`, a quick finger):
    /// the pointer is placed, then the press, the release and the leave take a frame each.
    #[test]
    fn a_tap_is_a_hover_then_a_click_over_three_frames() {
        let mut t = TouchPointer::default();
        let got = run(
            &mut t,
            &[
                (0.0, Some((1, Started, P))),
                (0.001, Some((1, Ended, P))),
                (0.016, None),
                (0.033, None),
                (0.05, None),
                (0.066, None),
            ],
        );
        assert_eq!(
            got,
            vec![
                vec![Place(P)],
                vec![Move(P)],
                vec![Down(Left)],
                vec![Up(Left)],
                vec![Leave],
                vec![]
            ]
        );
        assert!(!t.active());
    }

    /// The press of a finger that stays down comes on the first frame after it, not with it.
    #[test]
    fn the_press_waits_for_the_next_frame_and_a_drag_is_a_left_drag() {
        let mut t = TouchPointer::default();
        let to = P + Vec2::new(80.0, 0.0);
        let got = run(
            &mut t,
            &[
                (0.0, Some((7, Started, P))),
                (0.016, None),
                (0.02, Some((7, Moved, to))),
                (0.033, None),
                // Long after: a drag is no long press.
                (2.0, None),
                (2.1, Some((7, Ended, to))),
                (2.116, None),
                (2.133, None),
            ],
        );
        assert_eq!(
            got,
            vec![
                vec![Place(P)],
                vec![Down(Left)],
                vec![Move(to)],
                vec![],
                vec![],
                vec![Move(to)],
                vec![Up(Left)],
                vec![Leave]
            ]
        );
    }

    /// A still finger: one right click after the long-press time, the left button still down.
    #[test]
    fn a_long_press_is_one_right_click() {
        let mut t = TouchPointer::default();
        let near = P + Vec2::new(3.0, -4.0);
        let got = run(
            &mut t,
            &[
                (0.0, Some((1, Started, P))),
                (0.016, None),
                (0.2, Some((1, Moved, near))),
                (0.4, None),
                (0.52, None),
                (0.54, None),
                (1.5, None),
                (1.6, Some((1, Ended, near))),
                (1.62, None),
            ],
        );
        assert_eq!(
            got,
            vec![
                vec![Place(P)],
                vec![Down(Left)],
                vec![Move(near)],
                vec![],
                vec![Down(Right)],
                vec![Up(Right)],
                vec![],
                vec![Move(near)],
                vec![Up(Left)]
            ]
        );
    }

    /// Spreading two fingers turns the wheel up, closing them down; the first finger's press is
    /// cancelled (no click), and the finger left after the pinch does nothing.
    #[test]
    fn a_pinch_is_the_wheel_and_never_a_click() {
        let mut t = TouchPointer::default();
        let (a, b) = (Vec2::new(400.0, 300.0), Vec2::new(600.0, 300.0));
        let wide = Vec2::new(820.5, 300.0);
        let mut out = Vec::new();
        t.touch(1, Started, a, 0.0, &mut out);
        t.frame(0.016, &mut out);
        t.touch(2, Started, b, 0.02, &mut out);
        assert_eq!(out, vec![Place(a), Down(Left), Cancel(Left), Place((a + b) * 0.5)]);
        out.clear();
        // 200 px apart to 420.5: 1.05^15.2.
        t.touch(2, Moved, wide, 0.05, &mut out);
        let lines: f32 = out
            .iter()
            .filter_map(|a| match a {
                Wheel(l) => Some(*l),
                _ => None,
            })
            .sum();
        assert!((lines - 15.23).abs() < 0.05, "{lines}");
        assert!(out.contains(&Place((a + wide) * 0.5)));
        out.clear();
        t.touch(2, Moved, b, 0.08, &mut out);
        assert!(matches!(out[1], Wheel(l) if (l + 15.23).abs() < 0.05), "{out:?}");
        out.clear();
        // One finger lifts; the other moves about and lifts: no button at all.
        t.touch(2, Ended, b, 0.1, &mut out);
        t.frame(0.116, &mut out);
        t.touch(1, Moved, a + Vec2::splat(90.0), 0.2, &mut out);
        t.frame(1.0, &mut out);
        t.touch(1, Ended, a, 1.1, &mut out);
        t.frame(1.116, &mut out);
        t.frame(1.133, &mut out);
        assert_eq!(out, vec![Leave]);
        assert!(!t.active());
    }

    /// A second finger before the first frame: the press that waited never happens.
    #[test]
    fn a_second_finger_drops_a_press_that_was_still_waiting() {
        let mut t = TouchPointer::default();
        let mut out = Vec::new();
        t.touch(1, Started, P, 0.0, &mut out);
        t.touch(2, Started, P + Vec2::X * 100.0, 0.001, &mut out);
        t.frame(0.016, &mut out);
        assert!(!out.iter().any(|a| matches!(a, Down(_) | Up(_) | Cancel(_))), "{out:?}");
    }

    /// Two taps faster than the frames: both clicks arrive, each press before its release.
    #[test]
    fn a_double_tap_keeps_both_clicks_in_order() {
        let mut t = TouchPointer::default();
        let mut out = Vec::new();
        t.touch(1, Started, P, 0.0, &mut out);
        t.touch(1, Ended, P, 0.001, &mut out);
        t.touch(2, Started, P, 0.002, &mut out);
        t.touch(2, Ended, P, 0.003, &mut out);
        for frame in 1..=4 {
            t.frame(frame as f64 * 0.016, &mut out);
        }
        let buttons: Vec<_> = out.iter().filter(|a| matches!(a, Down(_) | Up(_))).copied().collect();
        assert_eq!(buttons, vec![Down(Left), Up(Left), Down(Left), Up(Left)]);
        assert_eq!(out.last(), Some(&Leave));
    }

    /// A touch the system takes away (a gesture of its own) lets go without a click.
    #[test]
    fn a_cancelled_touch_is_no_click() {
        let mut t = TouchPointer::default();
        let got = run(
            &mut t,
            &[
                (0.0, Some((1, Started, P))),
                (0.016, None),
                (0.1, Some((1, Cancelled, P))),
                (0.116, None),
                (0.133, None),
            ],
        );
        assert_eq!(got[3], vec![Cancel(Left)]);
        assert_eq!(got[4], vec![Leave]);
        // Cancelled before its press: nothing but the pointer coming and going.
        let got = run(
            &mut t,
            &[
                (1.0, Some((2, Started, P))),
                (1.001, Some((2, Cancelled, P))),
                (1.016, None),
            ],
        );
        assert_eq!(got, vec![vec![Place(P)], vec![Move(P)], vec![Leave]]);
    }
}
