// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! Symbol placement: where a label or an icon goes, and whether it fits without overlapping
//! one placed before it. The design follows MapLibre GL JS's `symbol/` (`get_anchors.ts`,
//! `check_max_angle.ts`, `collision_index.ts`, `shaping.ts`), with the geometry done in screen
//! points rather than tile units and a text box per label rather than per glyph.

use day_geometry::{Point, Rect};
use std::collections::HashMap;

/// A place a symbol can go: a screen point and the angle (radians, clockwise from +x) the
/// symbol's baseline takes there. Zero for point placement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Anchor {
    pub point: Point,
    pub angle: f64,
}

/// The boxes placed so far in a frame, bucketed on a grid so a query touches a few cells.
#[derive(Debug)]
pub struct CollisionIndex {
    viewport: Rect,
    cell: f64,
    boxes: Vec<Rect>,
    grid: HashMap<(i32, i32), Vec<usize>>,
}

impl CollisionIndex {
    /// An empty index for symbols drawn into `viewport`.
    pub fn new(viewport: Rect) -> CollisionIndex {
        CollisionIndex {
            viewport,
            cell: 64.0,
            boxes: Vec::new(),
            grid: HashMap::new(),
        }
    }

    fn cells(&self, r: &Rect) -> impl Iterator<Item = (i32, i32)> + use<> {
        let x0 = (r.origin.x / self.cell).floor() as i32;
        let y0 = (r.origin.y / self.cell).floor() as i32;
        let x1 = ((r.origin.x + r.size.width) / self.cell).floor() as i32;
        let y1 = ((r.origin.y + r.size.height) / self.cell).floor() as i32;
        (y0..=y1).flat_map(move |y| (x0..=x1).map(move |x| (x, y)))
    }

    /// Whether any part of `r` is on screen.
    pub fn is_visible(&self, r: &Rect) -> bool {
        self.viewport.intersects(r)
    }

    /// Whether `r` overlaps a box placed before.
    pub fn collides(&self, r: &Rect) -> bool {
        self.cells(r).any(|cell| {
            self.grid
                .get(&cell)
                .is_some_and(|ids| ids.iter().any(|&i| self.boxes[i].intersects(r)))
        })
    }

    /// Reserve `r`.
    pub fn insert(&mut self, r: Rect) {
        let i = self.boxes.len();
        self.boxes.push(r);
        for cell in self.cells(&r) {
            self.grid.entry(cell).or_default().push(i);
        }
    }

    /// How many boxes are placed.
    pub fn len(&self) -> usize {
        self.boxes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.boxes.is_empty()
    }
}

/// The axis-aligned box of a `width` × `height` box centered on `center` and rotated by
/// `angle`, grown by `padding` on every side.
pub fn rotated_bounds(center: Point, width: f64, height: f64, angle: f64, padding: f64) -> Rect {
    let (s, c) = angle.sin_cos();
    let (hw, hh) = (width / 2.0, height / 2.0);
    let ex = (hw * c).abs() + (hh * s).abs() + padding;
    let ey = (hw * s).abs() + (hh * c).abs() + padding;
    Rect::new(center.x - ex, center.y - ey, 2.0 * ex, 2.0 * ey)
}

/// The offset from an anchor point to the center of a `width` × `height` box hung on it per a
/// style anchor token (`center`, `left`, `top-right`, …): `left` means the box's left edge is
/// at the point, so the box sits to the right.
pub fn anchor_offset(anchor: &str, width: f64, height: f64) -> Point {
    let (hw, hh) = (width / 2.0, height / 2.0);
    let x = if anchor.contains("left") {
        hw
    } else if anchor.contains("right") {
        -hw
    } else {
        0.0
    };
    let y = if anchor.contains("top") {
        hh
    } else if anchor.contains("bottom") {
        -hh
    } else {
        0.0
    };
    Point::new(x, y)
}

/// The length of a polyline.
pub fn length(line: &[Point]) -> f64 {
    line.windows(2).map(|w| distance(w[0], w[1])).sum()
}

fn distance(a: Point, b: Point) -> f64 {
    ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt()
}

/// The point `at` along `line` from its start, and the angle of the segment it falls in.
fn point_along(line: &[Point], at: f64) -> Option<Anchor> {
    let mut walked = 0.0;
    for w in line.windows(2) {
        let seg = distance(w[0], w[1]);
        if seg == 0.0 {
            continue;
        }
        if walked + seg >= at {
            let t = ((at - walked) / seg).clamp(0.0, 1.0);
            let point = Point::new(
                w[0].x + (w[1].x - w[0].x) * t,
                w[0].y + (w[1].y - w[0].y) * t,
            );
            let angle = (w[1].y - w[0].y).atan2(w[1].x - w[0].x);
            return Some(Anchor { point, angle });
        }
        walked += seg;
    }
    None
}

/// Whether the line bends by less than `max_angle` (radians) in total over the `half`
/// points on either side of the position `at` along it: MapLibre's `checkMaxAngle`, so a label
/// is not laid along a corner.
pub fn fits_angle(line: &[Point], at: f64, half: f64, max_angle: f64) -> bool {
    let (from, to) = (at - half, at + half);
    let mut walked = 0.0;
    let mut turned = 0.0;
    let mut previous: Option<f64> = None;
    for w in line.windows(2) {
        let seg = distance(w[0], w[1]);
        if seg == 0.0 {
            continue;
        }
        let (start, end) = (walked, walked + seg);
        walked = end;
        if end < from {
            continue;
        }
        if start > to {
            break;
        }
        let angle = (w[1].y - w[0].y).atan2(w[1].x - w[0].x);
        if let Some(p) = previous {
            let mut delta = (angle - p).abs();
            if delta > std::f64::consts::PI {
                delta = 2.0 * std::f64::consts::PI - delta;
            }
            turned += delta;
            if turned > max_angle {
                return false;
            }
        }
        previous = Some(angle);
    }
    true
}

/// Anchors along a line for a label `label_length` long, one every `spacing` points, each
/// where the line is straight enough (`max_angle`, radians) for the label to lie on it:
/// MapLibre's `getAnchors`. A line shorter than the label gets none.
pub fn line_anchors(
    line: &[Point],
    spacing: f64,
    label_length: f64,
    max_angle: f64,
) -> Vec<Anchor> {
    let total = length(line);
    let spacing = spacing.max(1.0);
    let half = label_length / 2.0;
    if total < label_length || line.len() < 2 {
        return Vec::new();
    }
    // Start half a spacing in, or centered if only one fits, so anchors do not sit on the
    // tile edge where the neighbour tile places the same label.
    let mut at = if total < spacing * 2.0 {
        total / 2.0
    } else {
        spacing / 2.0
    };
    let mut anchors = Vec::new();
    while at + half <= total {
        if at - half >= 0.0
            && fits_angle(line, at, half, max_angle)
            && let Some(anchor) = point_along(line, at)
        {
            anchors.push(anchor);
        }
        at += spacing;
    }
    anchors
}

/// The one anchor at the middle of a line (`symbol-placement: line-center`), if the label
/// fits it.
pub fn line_center_anchor(line: &[Point], label_length: f64, max_angle: f64) -> Option<Anchor> {
    let total = length(line);
    if total < label_length || line.len() < 2 {
        return None;
    }
    let at = total / 2.0;
    fits_angle(line, at, label_length / 2.0, max_angle)
        .then(|| point_along(line, at))
        .flatten()
}

/// An angle made upright: text along a line reads left to right, so a baseline pointing
/// west is flipped.
pub fn upright(angle: f64) -> f64 {
    let half = std::f64::consts::FRAC_PI_2;
    if angle > half {
        angle - std::f64::consts::PI
    } else if angle < -half {
        angle + std::f64::consts::PI
    } else {
        angle
    }
}

/// Break `text` into lines no wider than `max_width` (points) as `measure` reports widths,
/// at spaces, greedily; a single word wider than the limit stays on its own line. MapLibre
/// balances line lengths with a penalty search (`shaping.ts`); greedy is close for the two- and
/// three-line labels a map has.
pub fn wrap(text: &str, max_width: f64, measure: &dyn Fn(&str) -> f64) -> Vec<String> {
    let words: Vec<&str> = text.split(' ').filter(|w| !w.is_empty()).collect();
    if words.len() <= 1 || max_width <= 0.0 {
        return vec![text.trim().to_string()];
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in words {
        let candidate = if line.is_empty() {
            word.to_string()
        } else {
            format!("{line} {word}")
        };
        if line.is_empty() || measure(&candidate) <= max_width {
            line = candidate;
        } else {
            lines.push(std::mem::take(&mut line));
            line = word.to_string();
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_finds_overlaps_through_its_grid() {
        let mut index = CollisionIndex::new(Rect::new(0.0, 0.0, 800.0, 600.0));
        let a = Rect::new(10.0, 10.0, 100.0, 20.0);
        assert!(!index.collides(&a));
        index.insert(a);
        assert!(index.collides(&Rect::new(100.0, 20.0, 30.0, 30.0)));
        assert!(!index.collides(&Rect::new(200.0, 200.0, 30.0, 30.0)));
        // Across cells: a wide box reaches many cells; a query in a far cell still hits it.
        index.insert(Rect::new(0.0, 300.0, 700.0, 10.0));
        assert!(index.collides(&Rect::new(650.0, 305.0, 10.0, 10.0)));
        assert!(index.is_visible(&Rect::new(790.0, 590.0, 50.0, 50.0)));
        assert!(!index.is_visible(&Rect::new(900.0, 0.0, 50.0, 50.0)));
        assert_eq!(index.len(), 2);
    }

    #[test]
    fn anchors_are_spaced_along_a_straight_line_and_refused_on_a_corner() {
        let line = [Point::new(0.0, 0.0), Point::new(1000.0, 0.0)];
        let anchors = line_anchors(&line, 250.0, 100.0, 0.5);
        assert_eq!(anchors.len(), 4);
        assert_eq!(anchors[0].point, Point::new(125.0, 0.0));
        assert_eq!(anchors[0].angle, 0.0);
        let short = [Point::new(0.0, 0.0), Point::new(50.0, 0.0)];
        assert!(line_anchors(&short, 250.0, 100.0, 0.5).is_empty());
        // A right angle at the middle: the center anchor is refused, the straight parts are fine.
        let corner = [
            Point::new(0.0, 0.0),
            Point::new(300.0, 0.0),
            Point::new(300.0, 300.0),
        ];
        assert!(line_center_anchor(&corner, 100.0, 0.5).is_none());
        assert!(line_center_anchor(&line, 100.0, 0.5).is_some());
        let along = line_anchors(&corner, 200.0, 50.0, 0.5);
        assert!(
            along
                .iter()
                .all(|a| (a.point.x - 300.0).abs() > 25.0 || (a.point.y).abs() > 25.0)
        );
        assert!(!along.is_empty());
    }

    #[test]
    fn boxes_hang_on_their_anchor_and_text_wraps_at_spaces() {
        assert_eq!(anchor_offset("center", 40.0, 10.0), Point::new(0.0, 0.0));
        assert_eq!(anchor_offset("left", 40.0, 10.0), Point::new(20.0, 0.0));
        assert_eq!(
            anchor_offset("top-right", 40.0, 10.0),
            Point::new(-20.0, 5.0)
        );
        let b = rotated_bounds(
            Point::new(0.0, 0.0),
            100.0,
            10.0,
            std::f64::consts::FRAC_PI_2,
            0.0,
        );
        assert!((b.size.width - 10.0).abs() < 1e-9 && (b.size.height - 100.0).abs() < 1e-9);
        assert_eq!(upright(3.0), 3.0 - std::f64::consts::PI);
        let measure = |s: &str| s.chars().count() as f64 * 8.0;
        assert_eq!(
            wrap("Royal Botanic Gardens Kew", 120.0, &measure),
            vec!["Royal Botanic", "Gardens Kew"]
        );
        assert_eq!(wrap("Antananarivo", 20.0, &measure), vec!["Antananarivo"]);
        assert_eq!(wrap("", 20.0, &measure), vec![""]);
    }
}
