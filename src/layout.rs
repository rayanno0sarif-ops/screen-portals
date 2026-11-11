//! Pure geometry: which screen edge leads to which computer, and where the
//! cursor is while it is "through the portal" on the other desktop.

use serde::{Deserialize, Serialize};

/// Where a neighbouring computer sits relative to this one.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
    Above,
    Below,
}

impl Side {
    /// The edge facing this one: what the other computer sees.
    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Above => Side::Below,
            Side::Below => Side::Above,
        }
    }

    /// Read a side typed on the command line.
    pub fn parse(text: &str) -> Option<Side> {
        match text.to_ascii_lowercase().as_str() {
            "left" => Some(Side::Left),
            "right" => Some(Side::Right),
            "above" | "top" | "up" => Some(Side::Above),
            "below" | "bottom" | "down" => Some(Side::Below),
            _ => None,
        }
    }

    /// For sentences: "laptop is <phrase> this computer".
    pub fn phrase(self) -> &'static str {
        match self {
            Side::Left => "to the left of",
            Side::Right => "to the right of",
            Side::Above => "above",
            Side::Below => "below",
        }
    }

    /// The side as one lowercase word.
    pub fn name(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
            Side::Above => "above",
            Side::Below => "below",
        }
    }
}

/// A rectangle in desktop pixels. `x` and `y` may be negative with several monitors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// How far inside the edge the cursor lands after crossing, so that a
/// one-pixel wobble does not bounce it straight back.
pub const INSET: i32 = 2;

/// Is the pointer on (or pushing past) an outer edge of the desktop?
/// Returns the edge and whether the point is strictly beyond it.
pub fn edge_hit(s: Rect, px: i32, py: i32) -> Option<(Side, bool)> {
    let right = s.x + s.w - 1;
    let bottom = s.y + s.h - 1;
    if px >= right {
        Some((Side::Right, px > right))
    } else if px <= s.x {
        Some((Side::Left, px < s.x))
    } else if py <= s.y {
        Some((Side::Above, py < s.y))
    } else if py >= bottom {
        Some((Side::Below, py > bottom))
    } else {
        None
    }
}

fn fraction(value: i32, start: i32, len: i32) -> f64 {
    if len <= 1 {
        return 0.5;
    }
    ((value - start) as f64 / (len - 1) as f64).clamp(0.0, 1.0)
}

fn along(t: f64, len: i32) -> i32 {
    (t * (len - 1).max(0) as f64).round() as i32
}

/// Position along an edge as 0.0..=1.0, so screens of different sizes line up.
pub fn edge_fraction(s: Rect, side: Side, px: i32, py: i32) -> f64 {
    match side {
        Side::Left | Side::Right => fraction(py, s.y, s.h),
        Side::Above | Side::Below => fraction(px, s.x, s.w),
    }
}

/// Where the local cursor reappears when it comes back through `side`.
pub fn return_point(s: Rect, side: Side, t: f64) -> (i32, i32) {
    match side {
        Side::Right => (s.x + s.w - 1 - INSET, s.y + along(t, s.h)),
        Side::Left => (s.x + INSET, s.y + along(t, s.h)),
        Side::Above => (s.x + along(t, s.w), s.y + INSET),
        Side::Below => (s.x + along(t, s.w), s.y + s.h - 1 - INSET),
    }
}

/// Which stretch of the shared edge lets the cursor through.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PortalMode {
    /// The whole edge of this screen leads to the whole edge of the other,
    /// scaled if the screens differ in size.
    #[default]
    Edge,
    /// Only the stretch where the two screens touch in the arrangement.
    Touching,
    /// Only the corner at the top (side edges) or left (top/bottom edges).
    Start,
    /// Only the corner at the bottom (side edges) or right (top/bottom edges).
    End,
}

/// How a neighbouring computer is arranged against this one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub side: Side,
    /// Where the neighbour's edge starts relative to ours, in pixels along
    /// the shared edge (positive = further down or right).
    pub offset: i32,
    pub mode: PortalMode,
}

impl Placement {
    /// The same arrangement as seen from the other computer.
    pub fn mirrored(self) -> Placement {
        Placement { side: self.side.opposite(), offset: -self.offset, mode: self.mode }
    }
}

/// A corner portal is this share of the shorter of the two edges.
pub const CORNER: f64 = 0.25;
/// Screens must keep at least this many pixels in contact when arranged.
pub const MIN_TOUCH: i32 = 80;

fn edge_len(side: Side, size: (i32, i32)) -> i32 {
    match side {
        Side::Left | Side::Right => size.1,
        Side::Above | Side::Below => size.0,
    }
    .max(2)
}

/// Keep an arrangement sensible: the two screens always stay in contact.
pub fn clamp_offset(side: Side, offset: i32, mine: (i32, i32), theirs: (i32, i32)) -> i32 {
    let (lm, lp) = (edge_len(side, mine), edge_len(side, theirs));
    let need = MIN_TOUCH.min(lm).min(lp);
    offset.clamp(need - lp, lm - need)
}

/// The doorway between two desktops: a stretch of our edge (`local`) joined
/// to a stretch of the neighbour's facing edge (`remote`), both as fractions
/// 0.0..=1.0 along the edge.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Portal {
    pub side: Side,
    pub local: (f64, f64),
    pub remote: (f64, f64),
}

/// Work out the doorway for an arrangement. `None` if the screens do not touch.
pub fn portal(placement: Placement, mine: (i32, i32), theirs: (i32, i32)) -> Option<Portal> {
    let lm = edge_len(placement.side, mine) as f64;
    let lp = edge_len(placement.side, theirs) as f64;
    let corner = CORNER * lm.min(lp);
    let (local, remote) = match placement.mode {
        PortalMode::Edge => ((0.0, 1.0), (0.0, 1.0)),
        PortalMode::Touching => {
            let o = placement.offset as f64;
            let (a, b) = (o.max(0.0), (o + lp).min(lm));
            if b - a < 1.0 {
                return None;
            }
            ((a / lm, b / lm), ((a - o) / lp, (b - o) / lp))
        }
        PortalMode::Start => ((0.0, corner / lm), (0.0, corner / lp)),
        PortalMode::End => ((1.0 - corner / lm, 1.0), (1.0 - corner / lp, 1.0)),
    };
    Some(Portal { side: placement.side, local, remote })
}

fn within(range: (f64, f64), t: f64) -> bool {
    t >= range.0 - 1e-9 && t <= range.1 + 1e-9
}

fn remap(from: (f64, f64), to: (f64, f64), t: f64) -> f64 {
    let span = from.1 - from.0;
    if span <= 0.0 {
        return (to.0 + to.1) / 2.0;
    }
    (to.0 + (t - from.0) / span * (to.1 - to.0)).clamp(to.0.min(to.1), to.0.max(to.1))
}

impl Portal {
    /// Does this point on our edge lie inside the doorway?
    pub fn admits_local(&self, t: f64) -> bool {
        within(self.local, t)
    }

    /// A position along our edge (0..1) as the matching position on theirs.
    pub fn to_remote(&self, t: f64) -> f64 {
        remap(self.local, self.remote, t)
    }

    /// A position along their edge (0..1) as the matching position on ours.
    pub fn to_local(&self, t: f64) -> f64 {
        remap(self.remote, self.local, t)
    }
}

/// The cursor's virtual position on a neighbour's desktop.
#[derive(Debug, Clone, PartialEq)]
pub struct Remote {
    /// Side of *our* desktop the neighbour is attached to.
    pub side: Side,
    pub w: i32,
    pub h: i32,
    pub x: i32,
    pub y: i32,
    /// The stretch of the neighbour's edge that leads back to us.
    pub door: (f64, f64),
}

impl Remote {
    /// Arrive on the neighbour's desktop at fraction `t` along its facing edge.
    pub fn enter(side: Side, w: i32, h: i32, t: f64, door: (f64, f64)) -> Remote {
        let (w, h) = (w.max(2), h.max(2));
        let (x, y) = match side {
            Side::Right => (INSET.min(w - 1), along(t, h)),
            Side::Left => ((w - 1 - INSET).max(0), along(t, h)),
            Side::Above => (along(t, w), (h - 1 - INSET).max(0)),
            Side::Below => (along(t, w), INSET.min(h - 1)),
        };
        Remote { side, w, h, x, y, door }
    }

    /// Move by a relative amount. Returns `Some(fraction along the
    /// neighbour's edge)` when the movement goes back out through the
    /// doorway. Outside the doorway the edge is a wall.
    pub fn apply(&mut self, dx: i32, dy: i32) -> Option<f64> {
        let nx = self.x.saturating_add(dx);
        let ny = self.y.saturating_add(dy);
        let back = match self.side {
            Side::Right => nx < 0,
            Side::Left => nx > self.w - 1,
            Side::Above => ny > self.h - 1,
            Side::Below => ny < 0,
        };
        self.x = nx.clamp(0, self.w - 1);
        self.y = ny.clamp(0, self.h - 1);
        if !back {
            return None;
        }
        let t = match self.side {
            Side::Left | Side::Right => fraction(self.y, 0, self.h),
            Side::Above | Side::Below => fraction(self.x, 0, self.w),
        };
        within(self.door, t).then_some(t)
    }

    /// Position as 0..=65535 on each axis.
    pub fn normalized(&self) -> (u16, u16) {
        let n = |v: i32, len: i32| ((v as i64 * 65535) / (len - 1).max(1) as i64) as u16;
        (n(self.x, self.w), n(self.y, self.h))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Rect = Rect { x: 0, y: 0, w: 1000, h: 500 };

    #[test]
    fn edges_are_detected() {
        assert_eq!(edge_hit(SCREEN, 500, 250), None);
        assert_eq!(edge_hit(SCREEN, 999, 250), Some((Side::Right, false)));
        assert_eq!(edge_hit(SCREEN, 1004, 250), Some((Side::Right, true)));
        assert_eq!(edge_hit(SCREEN, -1, 250), Some((Side::Left, true)));
        assert_eq!(edge_hit(SCREEN, 500, 0), Some((Side::Above, false)));
        assert_eq!(edge_hit(SCREEN, 500, 520), Some((Side::Below, true)));
    }

    #[test]
    fn negative_origin_desktops_work() {
        let s = Rect { x: -1920, y: 0, w: 3840, h: 1080 };
        assert_eq!(edge_hit(s, -1921, 5), Some((Side::Left, true)));
        assert_eq!(edge_hit(s, 0, 5), None);
        assert_eq!(return_point(s, Side::Left, 0.0), (-1918, 0));
    }

    const WHOLE: (f64, f64) = (0.0, 1.0);

    #[test]
    fn crossing_scales_between_screen_sizes() {
        let t = edge_fraction(SCREEN, Side::Right, 1003, 250);
        let r = Remote::enter(Side::Right, 2000, 1000, t, WHOLE);
        assert_eq!(r.x, INSET);
        assert!((r.y - 500).abs() <= 1);
    }

    #[test]
    fn whole_edge_portal_scales() {
        let p = portal(Placement { side: Side::Right, offset: 300, mode: PortalMode::Edge }, (1000, 500), (2000, 1000)).unwrap();
        assert_eq!((p.local, p.remote), (WHOLE, WHOLE));
        assert!(p.admits_local(0.0) && p.admits_local(1.0));
        assert!((p.to_remote(0.25) - 0.25).abs() < 1e-9);
    }

    #[test]
    fn touching_portal_is_only_the_overlap() {
        // A 1000-tall neighbour pushed 300px down against a 500-tall screen:
        // they share our 300..500, which is its 0..200.
        let place = Placement { side: Side::Right, offset: 300, mode: PortalMode::Touching };
        let p = portal(place, (1000, 500), (2000, 1000)).unwrap();
        assert!((p.local.0 - 0.6).abs() < 1e-9 && (p.local.1 - 1.0).abs() < 1e-9);
        assert!((p.remote.0 - 0.0).abs() < 1e-9 && (p.remote.1 - 0.2).abs() < 1e-9);
        assert!(!p.admits_local(0.5), "above the overlap is a wall");
        assert!(p.admits_local(0.8));
        assert!((p.to_remote(0.8) - 0.1).abs() < 1e-9, "pixels map one to one");
        assert!((p.to_local(0.1) - 0.8).abs() < 1e-9);

        // Seen from the other computer it is the same doorway, swapped.
        let q = portal(place.mirrored(), (2000, 1000), (1000, 500)).unwrap();
        assert_eq!(q.side, Side::Left);
        assert!((q.local.0 - p.remote.0).abs() < 1e-9 && (q.local.1 - p.remote.1).abs() < 1e-9);
        assert!((q.remote.0 - p.local.0).abs() < 1e-9 && (q.remote.1 - p.local.1).abs() < 1e-9);

        let apart = Placement { side: Side::Right, offset: 5000, mode: PortalMode::Touching };
        assert_eq!(portal(apart, (1000, 500), (2000, 1000)), None);
    }

    #[test]
    fn corner_portals_are_the_same_size_on_both_screens() {
        let mine = (1000, 400);
        let theirs = (2000, 1000);
        let start = portal(Placement { side: Side::Right, offset: 0, mode: PortalMode::Start }, mine, theirs).unwrap();
        // A quarter of the shorter edge: 100px of our 400 and of its 1000.
        assert!((start.local.1 - 0.25).abs() < 1e-9 && (start.remote.1 - 0.1).abs() < 1e-9);
        assert!(start.admits_local(0.1) && !start.admits_local(0.5));
        let end = portal(Placement { side: Side::Right, offset: 0, mode: PortalMode::End }, mine, theirs).unwrap();
        assert!((end.local.0 - 0.75).abs() < 1e-9 && (end.remote.0 - 0.9).abs() < 1e-9);
        assert!(end.admits_local(0.9) && !end.admits_local(0.5));
        // Top/bottom edges use widths.
        let top = portal(Placement { side: Side::Above, offset: 0, mode: PortalMode::Start }, mine, theirs).unwrap();
        assert!((top.local.1 - 0.25).abs() < 1e-9 && (top.remote.1 - 0.125).abs() < 1e-9);
    }

    #[test]
    fn offsets_are_clamped_so_screens_stay_in_contact() {
        let (mine, theirs) = ((1000, 500), (2000, 1000));
        assert_eq!(clamp_offset(Side::Right, 9999, mine, theirs), 500 - MIN_TOUCH);
        assert_eq!(clamp_offset(Side::Right, -9999, mine, theirs), MIN_TOUCH - 1000);
        assert_eq!(clamp_offset(Side::Right, 10, mine, theirs), 10);
        assert_eq!(clamp_offset(Side::Above, 9999, mine, theirs), 1000 - MIN_TOUCH);
    }

    #[test]
    fn leaving_outside_the_doorway_is_blocked() {
        let mut r = Remote::enter(Side::Right, 2000, 1000, 0.1, (0.0, 0.2));
        assert!(r.apply(500, 600).is_none());
        assert!(r.apply(-5000, 0).is_none(), "the edge is a wall away from the doorway");
        assert_eq!(r.x, 0);
        assert!(r.apply(0, -600).is_none());
        assert!(r.apply(-10, 0).is_some(), "back inside the doorway it opens");
    }

    #[test]
    fn remote_cursor_clamps_and_returns() {
        let mut r = Remote::enter(Side::Right, 2000, 1000, 0.5, WHOLE);
        assert_eq!(r.apply(100, 0), None);
        assert_eq!(r.x, 102);
        assert_eq!(r.apply(5000, -5000), None);
        assert_eq!((r.x, r.y), (1999, 0));
        assert_eq!(r.normalized(), (65535, 0));
        let t = r.apply(-3000, 999).expect("should come back");
        assert!((t - 1.0).abs() < 1e-9);
        assert_eq!(return_point(SCREEN, Side::Right, t), (997, 499));
    }

    #[test]
    fn each_side_returns_through_its_own_edge() {
        let mut left = Remote::enter(Side::Left, 800, 600, 0.5, WHOLE);
        assert_eq!(left.x, 797);
        assert!(left.apply(-10, 0).is_none());
        assert!(left.apply(50, 0).is_some());

        let mut above = Remote::enter(Side::Above, 800, 600, 0.5, WHOLE);
        assert!(above.apply(0, -10).is_none());
        assert!(above.apply(0, 50).is_some());

        let mut below = Remote::enter(Side::Below, 800, 600, 0.5, WHOLE);
        assert!(below.apply(0, 10).is_none());
        assert!(below.apply(0, -50).is_some());
    }

    #[test]
    fn side_parsing_round_trips() {
        for side in [Side::Left, Side::Right, Side::Above, Side::Below] {
            assert_eq!(Side::parse(side.name()), Some(side));
            assert_eq!(side.opposite().opposite(), side);
        }
        assert_eq!(Side::parse("sideways"), None);
    }
}
