// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! The camera: a center, a zoom, a bearing and a viewport size, and the Web Mercator arithmetic
//! between longitude/latitude, world coordinates and screen points (MapLibre's `Transform`,
//! without the pitch).
//!
//! World coordinates put the whole globe in a square `0..WORLD` at zoom 0 and double it per
//! zoom level, so a tile at zoom `z` is `WORLD / 2^z` world units wide at zoom 0 and `TILE`
//! screen points wide when the camera is at zoom `z`. The screen is the world scaled by
//! `2^zoom`, translated so the center lands mid-viewport.

use crate::source::TileId;
use day_geometry::{Point, Size};

/// The side of a tile on screen, in points, at its own zoom level.
pub const TILE: f64 = 512.0;
/// The world's side in world units at zoom 0 (one tile).
pub const WORLD: f64 = TILE;

/// A bearing brought into `-180.0..180.0`.
pub fn wrap_bearing(bearing: f64) -> f64 {
    if !bearing.is_finite() {
        return 0.0;
    }
    let b = bearing.rem_euclid(360.0);
    if b >= 180.0 { b - 360.0 } else { b }
}

/// The highest zoom a camera accepts; sources end sooner and are overzoomed up to here.
pub const MAX_ZOOM: f64 = 22.0;
/// The latitude limit of Web Mercator.
pub const MAX_LATITUDE: f64 = 85.051_129;

/// A position on the globe, in degrees.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LngLat {
    pub lng: f64,
    pub lat: f64,
}

impl LngLat {
    pub const fn new(lng: f64, lat: f64) -> LngLat {
        LngLat { lng, lat }
    }

    /// The same position with the longitude wrapped into `-180..=180` and the latitude held to
    /// the projection's range.
    pub fn wrapped(self) -> LngLat {
        let mut lng = self.lng;
        while lng > 180.0 {
            lng -= 360.0;
        }
        while lng < -180.0 {
            lng += 360.0;
        }
        LngLat::new(lng, self.lat.clamp(-MAX_LATITUDE, MAX_LATITUDE))
    }

    /// World coordinates at zoom 0 (`0..WORLD` on both axes, y down).
    pub fn to_world(self) -> Point {
        let p = self.wrapped();
        let x = (p.lng + 180.0) / 360.0 * WORLD;
        let lat = p.lat.to_radians();
        let y = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * WORLD;
        Point::new(x, y)
    }

    /// The position of a world coordinate.
    pub fn from_world(p: Point) -> LngLat {
        let lng = p.x / WORLD * 360.0 - 180.0;
        let n = std::f64::consts::PI * (1.0 - 2.0 * p.y / WORLD);
        let lat = n.sinh().atan().to_degrees();
        LngLat::new(lng, lat)
    }
}

/// A view of the map: where it looks, how close, which way is up, and how big the viewport is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub center: LngLat,
    pub zoom: f64,
    /// Degrees the map is turned clockwise on screen: `0.0` puts north up, `90.0` east up.
    pub bearing: f64,
    pub size: Size,
}

impl Camera {
    pub fn new(center: LngLat, zoom: f64, size: Size) -> Camera {
        Camera::with_bearing(center, zoom, 0.0, size)
    }

    pub fn with_bearing(center: LngLat, zoom: f64, bearing: f64, size: Size) -> Camera {
        Camera {
            center: center.wrapped(),
            zoom: zoom.clamp(0.0, MAX_ZOOM),
            bearing: wrap_bearing(bearing),
            size,
        }
    }

    /// The same view turned to `bearing`, the position at the viewport's center held.
    pub fn turned(&self, bearing: f64) -> Camera {
        Camera {
            bearing: wrap_bearing(bearing),
            ..*self
        }
    }

    /// The bearing in radians, the sense `Affine::rotate` and `sin_cos` use on a y-down screen.
    pub fn bearing_radians(&self) -> f64 {
        self.bearing.to_radians()
    }

    /// The viewport's middle.
    pub fn middle(&self) -> Point {
        Point::new(self.size.width / 2.0, self.size.height / 2.0)
    }

    /// A north-up screen point (the frame the tiles are laid out in) turned by the bearing
    /// about the viewport's middle: where it shows on screen.
    pub fn turn(&self, p: Point) -> Point {
        if self.bearing == 0.0 {
            return p;
        }
        let (s, c) = self.bearing_radians().sin_cos();
        let m = self.middle();
        let (dx, dy) = (p.x - m.x, p.y - m.y);
        Point::new(m.x + dx * c - dy * s, m.y + dx * s + dy * c)
    }

    /// The inverse of [`Camera::turn`]: a screen point back in the north-up frame.
    pub fn unturn(&self, p: Point) -> Point {
        if self.bearing == 0.0 {
            return p;
        }
        let (s, c) = (-self.bearing_radians()).sin_cos();
        let m = self.middle();
        let (dx, dy) = (p.x - m.x, p.y - m.y);
        Point::new(m.x + dx * c - dy * s, m.y + dx * s + dy * c)
    }

    /// The scale from zoom-0 world units to screen points.
    pub fn scale(&self) -> f64 {
        2f64.powf(self.zoom)
    }

    /// The world's side in screen points at this zoom.
    pub fn world_size(&self) -> f64 {
        WORLD * self.scale()
    }

    /// The north-up screen point of a world coordinate: where it lands before the bearing
    /// turns the view. Tiles are laid out and rasterized in this frame.
    pub fn world_to_unturned(&self, world: Point) -> Point {
        let c = self.center.to_world();
        let s = self.scale();
        Point::new(
            (world.x - c.x) * s + self.size.width / 2.0,
            (world.y - c.y) * s + self.size.height / 2.0,
        )
    }

    /// The world coordinate at a north-up screen point.
    pub fn unturned_to_world(&self, screen: Point) -> Point {
        let c = self.center.to_world();
        let s = self.scale();
        Point::new(
            (screen.x - self.size.width / 2.0) / s + c.x,
            (screen.y - self.size.height / 2.0) / s + c.y,
        )
    }

    /// The screen point of a world coordinate.
    pub fn world_to_screen(&self, world: Point) -> Point {
        self.turn(self.world_to_unturned(world))
    }

    /// The world coordinate under a screen point.
    pub fn screen_to_world(&self, screen: Point) -> Point {
        self.unturned_to_world(self.unturn(screen))
    }

    /// The screen point of a position.
    pub fn project(&self, p: LngLat) -> Point {
        self.world_to_screen(p.to_world())
    }

    /// The position under a screen point.
    pub fn unproject(&self, screen: Point) -> LngLat {
        LngLat::from_world(self.screen_to_world(screen))
    }

    /// The camera after the map moved by `delta` screen points (a drag or a pan: the content
    /// follows the finger, so the center moves the other way).
    pub fn panned(&self, delta: Point) -> Camera {
        let c = self.center.to_world();
        let s = self.scale();
        // A screen displacement, back in the north-up frame, is a world displacement.
        let m = self.middle();
        let d = self.unturn(Point::new(m.x + delta.x, m.y + delta.y));
        let moved = Point::new(c.x - (d.x - m.x) / s, c.y - (d.y - m.y) / s);
        Camera::with_bearing(
            LngLat::from_world(self.clamp_world(moved)),
            self.zoom,
            self.bearing,
            self.size,
        )
    }

    /// The camera at `zoom` with the position under `anchor` held still on screen.
    pub fn zoomed_about(&self, zoom: f64, anchor: Point) -> Camera {
        let zoom = zoom.clamp(0.0, MAX_ZOOM);
        let under = self.screen_to_world(anchor);
        Camera { zoom, ..*self }.centered_so(under, anchor)
    }

    /// The camera turned to `bearing` with the position under `anchor` held still on screen.
    pub fn turned_about(&self, bearing: f64, anchor: Point) -> Camera {
        let under = self.screen_to_world(anchor);
        self.turned(bearing).centered_so(under, anchor)
    }

    /// This camera moved so that `world` shows at `anchor`.
    fn centered_so(&self, world: Point, anchor: Point) -> Camera {
        let s = self.scale();
        let m = self.middle();
        let a = self.unturn(anchor);
        let center = Point::new(world.x - (a.x - m.x) / s, world.y - (a.y - m.y) / s);
        Camera::with_bearing(
            LngLat::from_world(self.clamp_world(center)),
            self.zoom,
            self.bearing,
            self.size,
        )
    }

    /// The north-up camera that shows the box from `sw` to `ne` whole, with `padding` points
    /// kept clear on every side; the zoom is a fraction, so the box fills the view.
    pub fn fitting(sw: LngLat, ne: LngLat, size: Size, padding: f64) -> Camera {
        let a = sw.to_world();
        let b = ne.to_world();
        let (w, h) = ((b.x - a.x).abs().max(1e-9), (b.y - a.y).abs().max(1e-9));
        let avail_w = (size.width - 2.0 * padding).max(1.0);
        let avail_h = (size.height - 2.0 * padding).max(1.0);
        let zoom = (avail_w / w).min(avail_h / h).log2().clamp(0.0, MAX_ZOOM);
        let center = LngLat::from_world(Point::new((a.x + b.x) / 2.0, (a.y + b.y) / 2.0));
        Camera::new(center, zoom, size)
    }

    /// The same view in a viewport of `size`.
    pub fn resized(&self, size: Size) -> Camera {
        Camera { size, ..*self }
    }

    /// Keep the center inside the world vertically (the map never shows past the poles);
    /// longitude wraps on its own.
    fn clamp_world(&self, p: Point) -> Point {
        let mut x = p.x;
        while x < 0.0 {
            x += WORLD;
        }
        while x >= WORLD {
            x -= WORLD;
        }
        Point::new(x, p.y.clamp(0.0, WORLD))
    }

    /// The integer zoom the tiles are fetched at for this view: the camera's zoom rounded
    /// down past the half-way point, as MapLibre does, held to the source's range.
    pub fn tile_zoom(&self, min_zoom: u8, max_zoom: u8) -> u8 {
        let z = (self.zoom + 0.001).floor();
        (z as i32).clamp(i32::from(min_zoom), i32::from(max_zoom)) as u8
    }

    /// The tiles at zoom `z` that cover the viewport, left to right then top to bottom, with
    /// columns wrapped around the antimeridian. `wrap` is how many worlds east (positive) or
    /// west the tile is drawn at, so a map zoomed out shows the globe repeating.
    pub fn covering_tiles(&self, z: u8) -> Vec<(TileId, i32)> {
        let n = 1u64 << z;
        let tile_world = WORLD / n as f64; // one tile's side in world units
        // The world box of the viewport: its four corners, since a turned view is a diamond
        // in world space.
        let corners = [
            Point::ZERO,
            Point::new(self.size.width, 0.0),
            Point::new(0.0, self.size.height),
            Point::new(self.size.width, self.size.height),
        ]
        .map(|p| self.screen_to_world(p));
        let (mut minx, mut miny, mut maxx, mut maxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for c in corners {
            minx = minx.min(c.x);
            miny = miny.min(c.y);
            maxx = maxx.max(c.x);
            maxy = maxy.max(c.y);
        }
        let x0 = (minx / tile_world).floor() as i64;
        let x1 = (maxx / tile_world).ceil() as i64;
        let y0 = ((miny / tile_world).floor() as i64).max(0);
        let y1 = ((maxy / tile_world).ceil() as i64).min(n as i64);
        let mut tiles = Vec::new();
        for y in y0..y1 {
            for x in x0..x1 {
                let wrap = x.div_euclid(n as i64);
                let col = x.rem_euclid(n as i64);
                tiles.push((TileId::new(z, col as u32, y as u32), wrap as i32));
            }
        }
        tiles
    }

    /// The north-up screen rect of a tile drawn `wrap` worlds from the one at longitude 0:
    /// where it is laid out before the bearing turns the frame (see [`Camera::turn`]).
    pub fn tile_rect(&self, id: TileId, wrap: i32) -> day_geometry::Rect {
        let n = (1u64 << id.z) as f64;
        let tile_world = WORLD / n;
        let origin = Point::new(
            (f64::from(id.x) + f64::from(wrap) * n) * tile_world,
            f64::from(id.y) * tile_world,
        );
        let top_left = self.world_to_unturned(origin);
        let side = tile_world * self.scale();
        day_geometry::Rect::new(top_left.x, top_left.y, side, side)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turned_camera_keeps_its_center_and_round_trips() {
        let size = Size::new(800.0, 600.0);
        let cam = Camera::with_bearing(LngLat::new(-0.1276, 51.5072), 14.0, 37.0, size);
        let mid = cam.project(cam.center);
        assert!((mid.x - 400.0).abs() < 1e-6 && (mid.y - 300.0).abs() < 1e-6);
        for p in [
            Point::new(10.0, 20.0),
            Point::new(790.0, 580.0),
            Point::ZERO,
        ] {
            let back = cam.project(cam.unproject(p));
            assert!(
                (back.x - p.x).abs() < 1e-6 && (back.y - p.y).abs() < 1e-6,
                "{p:?} -> {back:?}"
            );
        }
        // Turning about a point keeps it still; panning moves the content with the finger.
        let anchor = Point::new(100.0, 500.0);
        let under = cam.unproject(anchor);
        let turned = cam.turned_about(-60.0, anchor);
        let after = turned.project(under);
        assert!((after.x - anchor.x).abs() < 1e-6 && (after.y - anchor.y).abs() < 1e-6);
        let dragged = turned.panned(Point::new(30.0, -40.0));
        let moved = dragged.project(under);
        assert!(
            (moved.x - 130.0).abs() < 1e-6 && (moved.y - 460.0).abs() < 1e-6,
            "{moved:?}"
        );
        assert_eq!(wrap_bearing(370.0), 10.0);
        assert_eq!(wrap_bearing(-190.0), 170.0);
        // A turned viewport covers more tiles than a north-up one of the same size.
        let north = cam.turned(0.0).covering_tiles(14).len();
        assert!(cam.covering_tiles(14).len() >= north);
    }

    #[test]
    fn fitting_shows_the_whole_box() {
        let size = Size::new(800.0, 600.0);
        let (sw, ne) = (LngLat::new(-0.2, 51.45), LngLat::new(-0.05, 51.55));
        let cam = Camera::fitting(sw, ne, size, 20.0);
        for corner in [
            sw,
            ne,
            LngLat::new(sw.lng, ne.lat),
            LngLat::new(ne.lng, sw.lat),
        ] {
            let p = cam.project(corner);
            assert!(
                p.x >= 19.0 && p.x <= 781.0 && p.y >= 19.0 && p.y <= 581.0,
                "{p:?}"
            );
        }
        // And nearly fills it on the limiting axis.
        let a = cam.project(sw);
        let b = cam.project(ne);
        let filled = ((b.x - a.x).abs() / 760.0).max((a.y - b.y).abs() / 560.0);
        assert!(filled > 0.99, "{filled}");
    }

    #[test]
    fn projection_round_trips() {
        for &(lng, lat) in &[
            (0.0, 0.0),
            (-0.1276, 51.5072),
            (139.69, 35.68),
            (-180.0, -60.0),
        ] {
            let p = LngLat::new(lng, lat);
            let back = LngLat::from_world(p.to_world());
            assert!((back.lng - lng).abs() < 1e-9, "{lng} -> {}", back.lng);
            assert!((back.lat - lat).abs() < 1e-9, "{lat} -> {}", back.lat);
        }
        assert_eq!(
            LngLat::new(0.0, 0.0).to_world(),
            Point::new(WORLD / 2.0, WORLD / 2.0)
        );
    }

    #[test]
    fn the_center_sits_mid_viewport_and_zoom_keeps_the_anchor() {
        let cam = Camera::new(LngLat::new(10.0, 50.0), 5.0, Size::new(800.0, 600.0));
        let c = cam.project(cam.center);
        assert!((c.x - 400.0).abs() < 1e-9 && (c.y - 300.0).abs() < 1e-9);
        let anchor = Point::new(100.0, 500.0);
        let under = cam.unproject(anchor);
        let zoomed = cam.zoomed_about(7.3, anchor);
        let after = zoomed.project(under);
        assert!((after.x - anchor.x).abs() < 1e-6 && (after.y - anchor.y).abs() < 1e-6);
        let panned = cam.panned(Point::new(50.0, -20.0));
        let moved = panned.project(cam.center);
        assert!((moved.x - 450.0).abs() < 1e-6 && (moved.y - 280.0).abs() < 1e-6);
    }

    #[test]
    fn covering_tiles_wrap_around_the_antimeridian() {
        let cam = Camera::new(LngLat::new(179.0, 0.0), 3.0, Size::new(1024.0, 512.0));
        let tiles = cam.covering_tiles(3);
        assert!(tiles.iter().any(|(t, w)| t.x == 7 && *w == 0));
        assert!(tiles.iter().any(|(t, w)| t.x == 0 && *w == 1));
        let whole = Camera::new(LngLat::new(0.0, 0.0), 0.0, Size::new(512.0, 512.0));
        assert_eq!(whole.covering_tiles(0), vec![(TileId::new(0, 0, 0), 0)]);
        let rect = whole.tile_rect(TileId::new(0, 0, 0), 0);
        assert_eq!(rect, day_geometry::Rect::new(0.0, 0.0, 512.0, 512.0));
    }
}
