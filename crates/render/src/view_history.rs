// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Previous / next view (#368): a per-viewport history of settled cameras.
//!
//! The history watches the camera rather than being told about each gesture:
//! [`ViewHistory::observe`] is called once per frame (or after each batch of
//! camera changes) and records a view once the camera has held still for
//! [`SETTLE_SECS`] with no navigation button down. A drag release, a scroll
//! burst, `ze`, a standard or named view, a recentre or a zoom window each
//! therefore make one entry, and the frames in between make none. Camera state
//! is view state: the history is never logged in the op log nor saved.

use crate::OrbitCamera;

/// Seconds the camera must hold still before its view is recorded, so a scroll
/// burst (one wheel step per frame) makes a single entry.
pub const SETTLE_SECS: f64 = 0.35;
/// Most views kept behind the current one; older ones drop off.
pub const HISTORY_CAP: usize = 50;

/// Settled views of one camera: `back` (oldest first), the current view and
/// `forward` (nearest last).
#[derive(Clone, Debug, Default)]
pub struct ViewHistory {
    back: Vec<OrbitCamera>,
    current: Option<OrbitCamera>,
    forward: Vec<OrbitCamera>,
    /// The camera last observed and when it last changed.
    seen: Option<(OrbitCamera, f64)>,
}

impl ViewHistory {
    /// Watch `cam` at time `now` (seconds, monotonic). `busy` is true while a
    /// navigation gesture holds the camera (a button is down), which defers
    /// recording however long the camera holds still. The first call records
    /// the starting view. Returns true while a change waits to settle, so the
    /// caller can schedule another call.
    pub fn observe(&mut self, cam: &OrbitCamera, now: f64, busy: bool) -> bool {
        let Some(current) = self.current else {
            self.current = Some(*cam);
            self.seen = Some((*cam, now));
            return false;
        };
        match self.seen {
            Some((seen, since)) if seen == *cam => {
                if seen != current && !busy && now - since >= SETTLE_SECS {
                    self.record(seen);
                }
            }
            prev => {
                // A change that had already settled, unobserved, is recorded
                // before the new one starts.
                if let Some((seen, since)) = prev
                    && seen != current
                    && now - since >= SETTLE_SECS
                {
                    self.record(seen);
                }
                self.seen = Some((*cam, now));
            }
        }
        self.current != Some(*cam)
    }

    /// Record `cam` as the current view, after the one it replaces. New
    /// navigation drops the views ahead.
    fn record(&mut self, cam: OrbitCamera) {
        if let Some(current) = self.current.replace(cam) {
            self.back.push(current);
            if self.back.len() > HISTORY_CAP {
                self.back.remove(0);
            }
        }
        self.forward.clear();
    }

    /// Settle `cam` now: record it if it differs from the current view, as if
    /// it had held still. Called before stepping, so a view still settling is
    /// not lost.
    fn settle(&mut self, cam: &OrbitCamera, now: f64) {
        match self.current {
            None => self.current = Some(*cam),
            Some(current) if current != *cam => self.record(*cam),
            Some(_) => {}
        }
        self.seen = Some((*cam, now));
    }

    /// Step back from `cam`, the camera as it is now: the previous view, which
    /// becomes current, or `None` at the oldest view.
    pub fn back(&mut self, cam: &OrbitCamera, now: f64) -> Option<OrbitCamera> {
        self.settle(cam, now);
        let prev = self.back.pop()?;
        self.forward.extend(self.current.replace(prev));
        self.seen = Some((prev, now));
        Some(prev)
    }

    /// Step forward from `cam`: the next view, or `None` at the newest one.
    pub fn forward(&mut self, cam: &OrbitCamera, now: f64) -> Option<OrbitCamera> {
        self.settle(cam, now);
        let next = self.forward.pop()?;
        self.back.extend(self.current.replace(next));
        self.seen = Some((next, now));
        Some(next)
    }

    /// Views behind and ahead of the current one.
    pub fn counts(&self) -> (usize, usize) {
        (self.back.len(), self.forward.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec3;

    fn at(x: f32) -> OrbitCamera {
        OrbitCamera { target: Vec3::new(x, 0.0, 0.0), ..OrbitCamera::default() }
    }

    /// Hold `cam` long enough to settle.
    fn settle(h: &mut ViewHistory, cam: &OrbitCamera, t: &mut f64) {
        h.observe(cam, *t, false);
        *t += SETTLE_SECS + 0.01;
        h.observe(cam, *t, false);
    }

    #[test]
    fn back_and_forward_walk_the_settled_views() {
        let (mut h, mut t) = (ViewHistory::default(), 0.0);
        for x in [0.0, 1.0, 2.0] {
            settle(&mut h, &at(x), &mut t);
        }
        assert_eq!(h.counts(), (2, 0));
        assert_eq!(h.back(&at(2.0), t), Some(at(1.0)));
        assert_eq!(h.back(&at(1.0), t), Some(at(0.0)));
        assert_eq!(h.back(&at(0.0), t), None, "oldest view");
        assert_eq!(h.forward(&at(0.0), t), Some(at(1.0)));
        assert_eq!(h.forward(&at(1.0), t), Some(at(2.0)));
        assert_eq!(h.forward(&at(2.0), t), None, "newest view");
        // Restoring a view is not itself new navigation.
        settle(&mut h, &at(2.0), &mut t);
        assert_eq!(h.counts(), (2, 0));
    }

    #[test]
    fn new_navigation_after_going_back_drops_the_views_ahead() {
        let (mut h, mut t) = (ViewHistory::default(), 0.0);
        for x in [0.0, 1.0, 2.0] {
            settle(&mut h, &at(x), &mut t);
        }
        let prev = h.back(&at(2.0), t).unwrap();
        settle(&mut h, &prev, &mut t);
        assert_eq!(h.counts(), (1, 1));
        settle(&mut h, &at(5.0), &mut t);
        assert_eq!(h.counts(), (2, 0), "forward truncated");
        assert_eq!(h.back(&at(5.0), t), Some(at(1.0)));
    }

    #[test]
    fn a_scroll_burst_or_a_held_drag_makes_one_entry() {
        let (mut h, mut t) = (ViewHistory::default(), 0.0);
        settle(&mut h, &at(0.0), &mut t);
        // One wheel step per 16 ms frame.
        for i in 1..=40 {
            t += 0.016;
            assert!(h.observe(&at(i as f32 * 0.1), t, false));
        }
        // A drag that pauses mid-way, button still down.
        for i in 1..=10 {
            t += 0.1;
            h.observe(&at(10.0 + i as f32), t, true);
        }
        t += 1.0;
        h.observe(&at(20.0), t, true);
        assert_eq!(h.counts(), (0, 0), "nothing recorded while moving or held");
        t += 0.016;
        assert!(!h.observe(&at(20.0), t, false), "released after holding still: recorded");
        assert_eq!(h.counts(), (1, 0));
        assert_eq!(h.back(&at(20.0), t), Some(at(0.0)));
    }

    #[test]
    fn a_settled_change_is_kept_when_no_frame_observed_the_pause() {
        // A host that only calls `observe` on camera changes (iOS) still gets
        // two entries for two gestures.
        let (mut h, mut t) = (ViewHistory::default(), 0.0);
        h.observe(&at(0.0), t, false);
        t += 0.1;
        h.observe(&at(1.0), t, false);
        t += 5.0;
        h.observe(&at(2.0), t, false);
        assert_eq!(h.counts(), (1, 0));
        // And a view still settling is recorded before stepping back.
        assert_eq!(h.back(&at(2.0), t), Some(at(1.0)));
        assert_eq!(h.forward(&at(1.0), t), Some(at(2.0)));
    }

    #[test]
    fn the_history_is_capped() {
        let (mut h, mut t) = (ViewHistory::default(), 0.0);
        for x in 0..(HISTORY_CAP + 20) {
            settle(&mut h, &at(x as f32), &mut t);
        }
        assert_eq!(h.counts(), (HISTORY_CAP, 0));
        let mut cam = at((HISTORY_CAP + 19) as f32);
        while let Some(prev) = h.back(&cam, t) {
            cam = prev;
        }
        assert_eq!(cam, at(19.0), "oldest views dropped first");
    }
}
