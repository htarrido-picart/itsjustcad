use egui::{Pos2, Rect};

/// How the central 3D area is split into viewport panes. Camera slots are
/// fixed per pane role so layouts share cameras: 0 = Persp, 1 = Top,
/// 2 = Front, 3 = Right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewportLayout {
    Single,
    /// Persp | Top, side by side.
    Two,
    /// Rhino-style quadrants: Top | Persp over Front | Right.
    Four,
}

impl ViewportLayout {
    pub fn pane_count(self) -> usize {
        match self {
            Self::Single => 1,
            Self::Two => 2,
            Self::Four => 4,
        }
    }

    /// Camera slot for a pane index (see enum docs for the slot roles).
    pub fn camera_index(self, pane: usize) -> usize {
        match self {
            Self::Single => 0,
            Self::Two => [0, 1][pane],
            Self::Four => [1, 0, 2, 3][pane], // TL Top, TR Persp, BL Front, BR Right
        }
    }

    /// Split the full viewport rect into pane rects, in pane-index order.
    pub fn split(self, rect: Rect) -> Vec<Rect> {
        let c = rect.center();
        match self {
            Self::Single => vec![rect],
            Self::Two => vec![
                Rect::from_min_max(rect.min, Pos2::new(c.x, rect.max.y)),
                Rect::from_min_max(Pos2::new(c.x, rect.min.y), rect.max),
            ],
            Self::Four => vec![
                Rect::from_min_max(rect.min, c),
                Rect::from_min_max(Pos2::new(c.x, rect.min.y), Pos2::new(rect.max.x, c.y)),
                Rect::from_min_max(Pos2::new(rect.min.x, c.y), Pos2::new(c.x, rect.max.y)),
                Rect::from_min_max(c, rect.max),
            ],
        }
    }

    /// Which pane is under `pos`, if any (drives the active-viewport rule:
    /// last hovered pane receives view commands and tool input).
    pub fn pane_at(self, rect: Rect, pos: Pos2) -> Option<usize> {
        self.split(rect).iter().position(|r| r.contains(pos))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAYOUTS: [ViewportLayout; 3] =
        [ViewportLayout::Single, ViewportLayout::Two, ViewportLayout::Four];

    fn full() -> Rect {
        Rect::from_min_max(Pos2::new(10.0, 20.0), Pos2::new(810.0, 620.0))
    }

    #[test]
    fn split_matches_pane_count_and_stays_inside() {
        for layout in LAYOUTS {
            let panes = layout.split(full());
            assert_eq!(panes.len(), layout.pane_count());
            for r in &panes {
                assert!(full().contains_rect(*r), "{layout:?}: {r:?} escapes");
            }
        }
    }

    #[test]
    fn split_tiles_the_full_rect_without_overlap() {
        for layout in LAYOUTS {
            let panes = layout.split(full());
            let area: f32 = panes.iter().map(|r| r.area()).sum();
            assert!((area - full().area()).abs() < 1.0, "{layout:?}: area {area}");
            for (i, a) in panes.iter().enumerate() {
                for b in &panes[i + 1..] {
                    let overlap = a.intersect(*b);
                    assert!(
                        overlap.area().max(0.0) < 1.0,
                        "{layout:?}: {a:?} overlaps {b:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn single_split_is_identity() {
        assert_eq!(ViewportLayout::Single.split(full()), vec![full()]);
    }

    #[test]
    fn pane_at_resolves_pane_centers_and_rejects_outside() {
        for layout in LAYOUTS {
            for (i, r) in layout.split(full()).iter().enumerate() {
                assert_eq!(layout.pane_at(full(), r.center()), Some(i), "{layout:?}");
            }
            assert_eq!(layout.pane_at(full(), Pos2::new(-5.0, -5.0)), None);
        }
    }

    #[test]
    fn camera_slots_are_distinct_within_a_layout() {
        for layout in LAYOUTS {
            let slots: Vec<usize> =
                (0..layout.pane_count()).map(|p| layout.camera_index(p)).collect();
            let mut dedup = slots.clone();
            dedup.sort_unstable();
            dedup.dedup();
            assert_eq!(dedup.len(), slots.len(), "{layout:?}: duplicate camera slot");
            assert!(slots.iter().all(|&s| s < 4));
        }
    }

    #[test]
    fn two_up_is_persp_then_top_and_four_up_is_rhino_order() {
        assert_eq!(ViewportLayout::Two.camera_index(0), 0); // Persp left
        assert_eq!(ViewportLayout::Two.camera_index(1), 1); // Top right
        let four: Vec<usize> = (0..4).map(|p| ViewportLayout::Four.camera_index(p)).collect();
        assert_eq!(four, vec![1, 0, 2, 3]); // Top, Persp, Front, Right
    }
}
