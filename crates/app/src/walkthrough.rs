// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The walkthrough tools (#369, SketchUp's Camera ▸ Position Camera / Look
//! Around / Walk). View state only: they move the pane's camera, never the
//! document, so nothing is logged or saved, like the other navigation.
//!
//! - `poscam [height]` (`positioncamera`): click a face or the ground to
//!   stand there, the eye `height` above it (default 1.68 m, SketchUp's
//!   5'6"), looking level the way the view faces; or press there and drag
//!   to the point to look at. Leaves parallel for perspective (the field of
//!   view is kept). Look Around follows.
//! - `lookaround [height]`: drag turns the view about the eye.
//! - `walk [height]`: drag up/down from the press point walks forward/back,
//!   left/right turns, faster the farther the drag; ↑ ↓ / W S walk, ← →
//!   turn, A D step sideways. Shift flies up/down (and drags sideways),
//!   Ctrl/Cmd runs (×3), Alt/Option walks through faces. The eye keeps its
//!   height above the floor (stairs, ramps) and stops short of faces ahead.
//!
//! A typed number (on the canvas, or on the command line) is the eye height:
//! for Position Camera the next placement's, in Look Around and Walk the
//! eye moves to that height above its floor at once. Esc ends the tool.
//! Scroll still zooms (cursor zoom, #365) in every one of them.
//!
//! This module is the state machine and the motion maths; the viewport side
//! is `app/walk_ui.rs`.

use itsjustcad_doc::walk::EYE_HEIGHT;

/// Which walkthrough tool is armed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WalkMode {
    PositionCamera,
    LookAround,
    Walk,
}

impl WalkMode {
    /// The mode a command-line verb arms.
    pub(crate) fn from_verb(verb: &str) -> Option<Self> {
        match verb {
            "poscam" | "positioncamera" => Some(Self::PositionCamera),
            "lookaround" => Some(Self::LookAround),
            "walk" => Some(Self::Walk),
            _ => None,
        }
    }

    pub(crate) fn verb(self) -> &'static str {
        match self {
            Self::PositionCamera => "poscam",
            Self::LookAround => "lookaround",
            Self::Walk => "walk",
        }
    }

    fn prompt_key(self) -> &'static str {
        match self {
            Self::PositionCamera => "walk.poscam.prompt",
            Self::LookAround => "walk.look.prompt",
            Self::Walk => "walk.walk.prompt",
        }
    }
}

/// Look Around and Walk turn rate per dragged pixel, in radians (as orbit).
pub(crate) const LOOK_RAD_PER_PX: f32 = 0.004;
/// Walking speed in m/s for a key, and for a drag of [`DRAG_REF_PX`].
pub(crate) const WALK_SPEED: f32 = 1.5;
/// Turning speed in rad/s for a key, and for a drag of [`DRAG_REF_PX`].
pub(crate) const TURN_SPEED: f32 = 1.0;
/// The drag distance (px) that walks at [`WALK_SPEED`]; speed scales with it.
pub(crate) const DRAG_REF_PX: f32 = 100.0;
/// Ctrl/Cmd: run.
pub(crate) const RUN_FACTOR: f32 = 3.0;
/// The drag dead zone around the press point, in px.
const DEAD_PX: f32 = 4.0;
/// Where the target sits ahead of a placed eye (m): the pivot a later orbit
/// turns about.
pub(crate) const LOOK_DISTANCE: f32 = 5.0;

/// One frame's walking intent, in the camera's walking frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Motion {
    /// Metres ahead (negative: back).
    pub ahead: f32,
    /// Metres to the right.
    pub right: f32,
    /// Metres up.
    pub up: f32,
    /// Radians turned left.
    pub turn_left: f32,
}

impl Motion {
    pub(crate) fn is_zero(&self) -> bool {
        *self == Self::default()
    }
}

/// Keys held this frame (see [`Motion::from_input`]).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WalkKeys {
    pub forward: bool,
    pub back: bool,
    pub turn_left: bool,
    pub turn_right: bool,
    pub strafe_left: bool,
    pub strafe_right: bool,
}

impl Motion {
    /// The motion over `dt` seconds from held keys and a walk drag `drag`
    /// (pointer minus press point, px, screen y down). Shift turns ahead/back
    /// into up/down and a drag's turn into a sideways step; `run` triples it.
    pub(crate) fn from_input(keys: WalkKeys, drag: Option<(f32, f32)>, shift: bool, run: bool, dt: f32) -> Self {
        let axis = |pos: bool, neg: bool| f32::from(u8::from(pos)) - f32::from(u8::from(neg));
        let mut fwd = axis(keys.forward, keys.back);
        let mut turn = axis(keys.turn_left, keys.turn_right);
        let mut side = axis(keys.strafe_right, keys.strafe_left);
        if let Some((dx, dy)) = drag {
            let dead = |v: f32| if v.abs() <= DEAD_PX { 0.0 } else { (v - DEAD_PX * v.signum()) / DRAG_REF_PX };
            fwd += -dead(dy);
            if shift {
                side += dead(dx);
            } else {
                turn += -dead(dx);
            }
        }
        let speed = WALK_SPEED * if run { RUN_FACTOR } else { 1.0 } * dt;
        let (ahead, up) = if shift { (0.0, fwd * speed) } else { (fwd * speed, 0.0) };
        Motion { ahead, right: side * speed, up, turn_left: turn * TURN_SPEED * dt }
    }
}

pub(crate) struct Walkthrough {
    mode: Option<WalkMode>,
    /// The eye height for the next Position Camera (m).
    pub eye_height: f64,
    /// Digits typed on the canvas.
    typed: String,
    /// The canvas should take the keyboard on the next frame (the tool was
    /// just started from the command line).
    pub take_keys: bool,
    /// Position Camera: the world point pressed, while dragging the look.
    pub press: Option<glam::DVec3>,
    /// Walk/Look: the screen point a drag started at.
    pub drag_origin: Option<egui::Pos2>,
}

impl Default for Walkthrough {
    fn default() -> Self {
        Self { mode: None, eye_height: EYE_HEIGHT, typed: String::new(), take_keys: false, press: None, drag_origin: None }
    }
}

impl Walkthrough {
    /// Arm `mode`. Returns the prompt.
    pub(crate) fn start(&mut self, mode: WalkMode) -> String {
        self.mode = Some(mode);
        self.typed.clear();
        self.press = None;
        self.drag_origin = None;
        self.take_keys = true;
        self.prompt().unwrap_or_default()
    }

    pub(crate) fn mode(&self) -> Option<WalkMode> {
        self.mode
    }

    pub(crate) fn active(&self) -> bool {
        self.mode.is_some()
    }

    pub(crate) fn cancel(&mut self) {
        self.mode = None;
        self.typed.clear();
        self.press = None;
        self.drag_origin = None;
        self.take_keys = false;
    }

    /// The command-line prompt (status hint) of the armed tool.
    pub(crate) fn prompt(&self) -> Option<String> {
        let mode = self.mode?;
        let mut s = crate::i18n::t(mode.prompt_key()).to_string();
        if !self.typed.is_empty() {
            s.push_str(&format!(" · {} m", self.typed));
        }
        Some(s)
    }

    /// A character typed on the canvas: digits, `.` and `-` build the eye
    /// height; anything else is ignored.
    pub(crate) fn push_input(&mut self, c: char) {
        if c.is_ascii_digit() || c == '.' || (c == '-' && self.typed.is_empty()) {
            self.typed.push(c);
        }
    }

    pub(crate) fn pop_input(&mut self) {
        self.typed.pop();
    }

    /// Enter: the typed eye height, if it is a usable one; the buffer empties.
    pub(crate) fn take_input(&mut self) -> Option<f64> {
        let v = parse_height(&std::mem::take(&mut self.typed));
        if let Some(h) = v {
            self.eye_height = h;
        }
        v
    }
}

/// A typed eye height: a finite number of metres (an optional `m` suffix),
/// at most 10 km either way.
pub(crate) fn parse_height(s: &str) -> Option<f64> {
    let s = s.trim();
    let s = s.strip_suffix('m').unwrap_or(s).trim();
    s.parse::<f64>().ok().filter(|v| v.is_finite() && v.abs() <= 1.0e4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbs_arm_their_modes() {
        assert_eq!(WalkMode::from_verb("poscam"), Some(WalkMode::PositionCamera));
        assert_eq!(WalkMode::from_verb("positioncamera"), Some(WalkMode::PositionCamera));
        assert_eq!(WalkMode::from_verb("lookaround"), Some(WalkMode::LookAround));
        assert_eq!(WalkMode::from_verb("walk"), Some(WalkMode::Walk));
        assert_eq!(WalkMode::from_verb("walls"), None);
        let mut w = Walkthrough::default();
        assert!(!w.active());
        w.start(WalkMode::Walk);
        assert!(w.active() && w.take_keys);
        for c in "1.7x".chars() {
            w.push_input(c);
        }
        assert!(w.prompt().unwrap().ends_with("1.7 m"));
        assert_eq!(w.take_input(), Some(1.7));
        assert_eq!(w.eye_height, 1.7);
        assert_eq!(w.take_input(), None);
        w.cancel();
        assert!(!w.active());
        assert_eq!(parse_height("1.6m"), Some(1.6));
        assert_eq!(parse_height("nan"), None);
    }

    #[test]
    fn motion_scales_with_drag_and_keys() {
        let dt = 0.5;
        let keys = WalkKeys { forward: true, turn_left: true, ..Default::default() };
        let m = Motion::from_input(keys, None, false, false, dt);
        assert_eq!(m, Motion { ahead: WALK_SPEED * dt, right: 0.0, up: 0.0, turn_left: TURN_SPEED * dt });
        let run = Motion::from_input(keys, None, false, true, dt);
        assert_eq!(run.ahead, m.ahead * RUN_FACTOR);
        // Dragging up walks forward, farther = faster; right turns right.
        let near = Motion::from_input(WalkKeys::default(), Some((0.0, -54.0)), false, false, dt);
        let far = Motion::from_input(WalkKeys::default(), Some((0.0, -204.0)), false, false, dt);
        assert!(near.ahead > 0.0 && far.ahead > 3.0 * near.ahead, "{near:?} {far:?}");
        let turn = Motion::from_input(WalkKeys::default(), Some((104.0, 0.0)), false, false, dt);
        assert!((turn.turn_left + TURN_SPEED * dt).abs() < 1e-6, "{turn:?}");
        // Shift: up/down and sideways instead.
        let fly = Motion::from_input(WalkKeys::default(), Some((104.0, -104.0)), true, false, dt);
        assert_eq!((fly.ahead, fly.turn_left), (0.0, 0.0));
        assert!(fly.up > 0.0 && fly.right > 0.0, "{fly:?}");
        // The dead zone.
        assert!(Motion::from_input(WalkKeys::default(), Some((3.0, 3.0)), false, false, dt).is_zero());
    }
}
