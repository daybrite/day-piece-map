// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! Shared state for interchangeable map providers. This crate loads no map SDK.
//! Use `day-piece-map-canvas`, `day-piece-map-apple`, or `day-piece-map-google` to render it.
//! A view belongs to one mounted provider at a time. Keep it outside the provider-switch scope
//! to preserve camera and annotations. Provider-specific options live in the provider crates.

mod coordinates;
pub use coordinates::{LngLat, MAX_LATITUDE, MAX_ZOOM, wrap_bearing};
#[doc(hidden)]
#[cfg(feature = "native-bridge")]
#[path = "native.rs"]
pub mod __native;
use day_geometry::{Color, Point};
use day_reactive::Signal;

/// Normalize a zoom, including non-finite input, to the shared range.
pub fn clamp_zoom(zoom: f64) -> f64 {
    if zoom.is_finite() {
        zoom.clamp(0.0, MAX_ZOOM)
    } else {
        0.0
    }
}

/// Initialization state reported by the active provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MapStatus {
    /// The provider is initializing or loading its map.
    Loading,
    /// The map is usable; additional tiles can still be loading.
    Ready,
    /// Initialization or a map operation failed. The detail is diagnostic, not localized UI.
    Failed(String),
}

/// A marker's handle, from [`MapView::add_marker`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MarkerId(u64);

impl MarkerId {
    /// Stable numeric identity for provider bridges.
    pub const fn value(self) -> u64 {
        self.0
    }
}

/// A pin on the map at a position, with an optional title shown beside it and a tag the app
/// uses to know it again when it is tapped.
#[derive(Clone, Debug, PartialEq)]
#[must_use]
pub struct Marker {
    pub position: LngLat,
    pub title: String,
    pub color: Color,
    /// Secondary annotation text, supplied by the app.
    pub subtitle: String,
    /// The app's own key for this marker (an item id, a route key), handed back through
    /// [`MapView::marker`] when [`MapView::tap`] names it; not shown.
    pub tag: String,
}

impl Marker {
    pub fn new(position: LngLat) -> Marker {
        Marker {
            position,
            title: String::new(),
            subtitle: String::new(),
            color: Color::rgb(0.86, 0.2, 0.18),
            tag: String::new(),
        }
    }

    pub fn title(mut self, title: impl Into<String>) -> Marker {
        self.title = title.into();
        self
    }

    pub fn subtitle(mut self, text: impl Into<String>) -> Self {
        self.subtitle = text.into();
        self
    }

    pub fn tag(mut self, tag: impl Into<String>) -> Marker {
        self.tag = tag.into();
        self
    }

    pub fn color(mut self, color: Color) -> Marker {
        self.color = color;
        self
    }
}

/// Where the user tapped ([`MapView::tap`]): the point, the position under it, the marker
/// hit if any. Provider-specific basemap feature queries live in provider crates.
#[derive(Clone, Debug, PartialEq)]
pub struct MapTap {
    pub point: Point,
    pub location: LngLat,
    pub marker: Option<MarkerId>,
}

/// A move the app asked for, carried out by the piece over `duration` seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraRequest {
    pub target: CameraTarget,
    pub duration: f64,
    /// Distinguishes one request from the next identical one.
    pub serial: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CameraTarget {
    /// A view: `None` keeps the current bearing.
    View {
        center: LngLat,
        zoom: f64,
        bearing: Option<f64>,
    },
    /// The north-up view that shows a box whole, `padding` points clear around it.
    Bounds {
        sw: LngLat,
        ne: LngLat,
        padding: f64,
    },
}

/// How long [`MapView::fly_to`] takes, in seconds.
const FLY_SECS: f64 = 1.0;
/// How long [`MapView::rotate_to`] takes, in seconds.
const TURN_SECS: f64 = 0.5;

/// The state of a map, shared between the piece and the app: where it looks and how close,
/// which the app sets to move the map and reads to learn where the user took it, plus what
/// the piece reports back about loading and drawing.
#[derive(Clone, Copy)]
pub struct MapView {
    /// Center in WGS84 degrees; longitude comes first.
    pub center: Signal<LngLat>,
    /// Web Mercator zoom with a 512-point world at zoom zero. Providers clamp to their limits.
    pub zoom: Signal<f64>,
    /// Clockwise heading in degrees; zero is north.
    pub bearing: Signal<f64>,
    /// Provider loading state. Failure details are diagnostic, not localized UI text.
    pub status: Signal<MapStatus>,
    /// Provider credits, when supplied as text. Native SDKs retain their attribution controls.
    pub attribution: Signal<String>,
    /// Ordered annotations. Prefer the marker methods to preserve stable identities.
    pub markers: Signal<Vec<(MarkerId, Marker)>>,
    /// Last map or marker tap. Native SDKs may reserve double taps for zooming.
    pub tap: Signal<Option<MapTap>>,
    /// Geometry drawn above the basemap and below annotations, in list order.
    pub overlays: Signal<Vec<Overlay>>,
    /// Which user camera gestures are enabled; programmatic moves remain available.
    pub interaction: Signal<Interaction>,
    /// Provider-facing command stream. Observe serial numbers; do not replay on remount.
    pub request: Signal<Option<CameraRequest>>,
    next_marker: Signal<u64>,
}

impl MapView {
    pub fn new(center: LngLat, zoom: f64) -> MapView {
        MapView {
            center: Signal::new(center.wrapped()),
            zoom: Signal::new(clamp_zoom(zoom)),
            status: Signal::new(MapStatus::Loading),
            attribution: Signal::new(String::new()),
            bearing: Signal::new(0.0),
            markers: Signal::new(Vec::new()),
            overlays: Signal::new(Vec::new()),
            interaction: Signal::new(Interaction::default()),
            tap: Signal::new(None),
            request: Signal::new(None),
            next_marker: Signal::new(1),
        }
    }

    /// Put a marker on the map.
    pub fn add_marker(&self, marker: Marker) -> MarkerId {
        let id = MarkerId(self.next_marker.get_untracked());
        self.next_marker.set(id.0 + 1);
        let mut markers = self.markers.get_untracked();
        markers.push((id, marker));
        self.markers.set(markers);
        id
    }

    /// Replace a marker in place, keeping its id: a new title or position. `false` when `id`
    /// is no longer on the map.
    pub fn update_marker(&self, id: MarkerId, marker: Marker) -> bool {
        let mut markers = self.markers.get_untracked();
        let Some(slot) = markers.iter_mut().find(|(m, _)| *m == id) else {
            return false;
        };
        slot.1 = marker;
        self.markers.set(markers);
        true
    }

    /// Take a marker off the map.
    pub fn remove_marker(&self, id: MarkerId) {
        let mut markers = self.markers.get_untracked();
        markers.retain(|(m, _)| *m != id);
        self.markers.set(markers);
    }

    pub fn clear_markers(&self) {
        self.markers.set(Vec::new());
    }

    /// The marker with `id`, if it is still on the map.
    pub fn marker(&self, id: MarkerId) -> Option<Marker> {
        self.markers
            .get_untracked()
            .into_iter()
            .find(|(m, _)| *m == id)
            .map(|(_, marker)| marker)
    }

    /// Animate to `center` at `zoom`. The canvas renderer uses a one-second flight; native SDKs
    /// choose their own trajectory and may approximate the requested duration.
    pub fn fly_to(&self, center: LngLat, zoom: f64) {
        self.fly_to_with(center, zoom, None, FLY_SECS);
    }

    /// [`MapView::fly_to`] with the bearing to arrive at (`None` keeps the current one) and
    /// the requested duration in seconds; `0.0` jumps. MapKit uses its standard animation
    /// timing for positive durations.
    pub fn fly_to_with(&self, center: LngLat, zoom: f64, bearing: Option<f64>, duration: f64) {
        self.request_move(
            CameraTarget::View {
                center: center.wrapped(),
                zoom: clamp_zoom(zoom),
                bearing,
            },
            duration,
        );
    }

    /// Glide to the north-up view that shows the box from `sw` to `ne` whole, with `padding`
    /// points kept clear on every side.
    pub fn fit_bounds(&self, sw: LngLat, ne: LngLat, padding: f64) {
        self.request_move(
            CameraTarget::Bounds {
                sw: sw.wrapped(),
                ne: ne.wrapped(),
                padding: if padding.is_finite() {
                    padding.max(0.0)
                } else {
                    0.0
                },
            },
            FLY_SECS,
        );
    }

    /// Turn to `bearing` degrees (clockwise, `0.0` north up) over half a second, the position
    /// at the center held.
    pub fn rotate_to(&self, bearing: f64) {
        self.request_move(
            CameraTarget::View {
                center: self.center.get_untracked(),
                zoom: self.zoom.get_untracked(),
                bearing: Some(bearing),
            },
            TURN_SECS,
        );
    }

    /// Turn back to north up.
    pub fn reset_north(&self) {
        self.rotate_to(0.0);
    }

    fn request_move(&self, target: CameraTarget, duration: f64) {
        let serial = self
            .request
            .get_untracked()
            .map_or(1, |r| r.serial.wrapping_add(1));
        self.request.set(Some(CameraRequest {
            target,
            duration: if duration.is_finite() {
                duration.clamp(0.0, 60.0)
            } else {
                0.0
            },
            serial,
        }));
    }

    /// Zoom by `levels` (negative zooms out), keeping the center.
    pub fn zoom_by(&self, levels: f64) {
        let z = clamp_zoom(self.zoom.get_untracked() + levels);
        self.zoom.set(z);
    }

    /// Look at `center` from `zoom`, at once.
    pub fn jump_to(&self, center: LngLat, zoom: f64) {
        self.center.set(center.wrapped());
        self.zoom.set(clamp_zoom(zoom));
    }

    /// [`MapView::jump_to`] with the bearing too (`0.0` is north up).
    pub fn jump_to_with(&self, center: LngLat, zoom: f64, bearing: f64) {
        self.jump_to(center, zoom);
        self.bearing.set(wrap_bearing(bearing));
    }
}

/// Gesture controls shared by all providers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interaction {
    pub pan: bool,
    pub zoom: bool,
    pub rotate: bool,
}
impl Default for Interaction {
    fn default() -> Self {
        Self {
            pan: true,
            zoom: true,
            rotate: true,
        }
    }
}

/// Geographic geometry in WGS84. Widths are logical points; circle radii are meters.
/// Coordinates are normalized by the provider; empty lines/rings are ignored.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Overlay {
    Polyline {
        points: Vec<LngLat>,
        color: Color,
        width: f64,
    },
    Polygon {
        points: Vec<LngLat>,
        fill: Color,
        stroke: Color,
        width: f64,
    },
    Circle {
        center: LngLat,
        radius: f64,
        fill: Color,
        stroke: Color,
        width: f64,
    },
}
