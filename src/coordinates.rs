// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

use day_geometry::Point;
const WORLD: f64 = 512.0;

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
    #[must_use]
    pub fn wrapped(self) -> LngLat {
        let lng = if self.lng.is_finite() {
            (self.lng + 180.0).rem_euclid(360.0) - 180.0
        } else {
            0.0
        };
        let lat = if self.lat.is_finite() {
            self.lat.clamp(-MAX_LATITUDE, MAX_LATITUDE)
        } else {
            0.0
        };
        LngLat::new(lng, lat)
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
