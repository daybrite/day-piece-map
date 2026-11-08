// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! The piece: a canvas showing the map, the state an app shares with it, and the gestures
//! that move it.
//!
//! Loading is a tree of tasks on the UI thread (docs/async.md): the style first, then its
//! sources in parallel and its sprite, then every tile a frame asks for. Each arrival writes
//! the shared state and notifies the repaint trigger, which the canvas's draw closure tracks,
//! so a frame re-records when the next tile lands and never earlier. The draw closure itself
//! only reads: it notes which tiles the camera needs and spawns their fetches, and the fetches
//! write when they complete.

use crate::camera::{Camera, LngLat, MAX_ZOOM};
use crate::render::{self, FeatureHit, Pass, RenderTile, Scene, Sprite, Stats, TileData};
use crate::source::{SourceSpec, TileId, TileSource};
use crate::style::Style;
use crate::{Error, mvt, source};
use day_geometry::{Affine, Color, Point, Rect, Size};
use day_pieces::prelude::*;
use day_pieces::{Draw, IntoReactive, TextStyle};
use day_reactive::{Signal, Trigger};
use day_spec::{DragPhase, Shape, TextAnchor};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

/// Where a map is in loading its style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MapStatus {
    /// The style is being fetched and its sources opened.
    Loading,
    /// The style is in and tiles are drawn as they arrive.
    Ready,
    /// The style or a source could not be loaded; the message says why.
    Failed(String),
}

/// A marker's handle, from [`MapView::add_marker`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MarkerId(u64);

/// A pin on the map at a position, with an optional title shown beside it and a tag the app
/// uses to know it again when it is tapped.
#[derive(Clone, Debug, PartialEq)]
pub struct Marker {
    pub position: LngLat,
    pub title: String,
    pub color: Color,
    /// The app's own key for this marker (an item id, a route key), handed back through
    /// [`MapView::marker`] when [`MapView::tap`] names it; not shown.
    pub tag: String,
}

impl Marker {
    pub fn new(position: LngLat) -> Marker {
        Marker {
            position,
            title: String::new(),
            color: Color::rgb(0.86, 0.2, 0.18),
            tag: String::new(),
        }
    }

    pub fn title(mut self, title: impl Into<String>) -> Marker {
        self.title = title.into();
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
/// hit if any, and the map features drawn there, topmost first.
#[derive(Clone, Debug, PartialEq)]
pub struct MapTap {
    pub point: Point,
    pub location: LngLat,
    pub marker: Option<MarkerId>,
    pub features: Vec<FeatureHit>,
}

/// A move the app asked for, carried out by the piece over `duration` seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct CameraRequest {
    pub(crate) target: CameraTarget,
    pub(crate) duration: f64,
    /// Distinguishes one request from the next identical one.
    pub(crate) serial: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum CameraTarget {
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
/// How long a double tap's zoom step takes, in seconds.
const TAP_ZOOM_SECS: f64 = 0.3;

/// The state of a map, shared between the piece and the app: where it looks and how close,
/// which the app sets to move the map and reads to learn where the user took it, plus what
/// the piece reports back about loading and drawing.
#[derive(Clone, Copy)]
pub struct MapView {
    /// The position at the middle of the viewport.
    pub center: Signal<LngLat>,
    /// The zoom level, `0.0..=22.0`: each level doubles the scale.
    pub zoom: Signal<f64>,
    pub status: Signal<MapStatus>,
    /// The credit lines the style's sources ask to be shown, joined; empty until the sources
    /// are open. The map draws it in its bottom-right corner; an app that wants it elsewhere
    /// too reads it here.
    pub attribution: Signal<String>,
    /// Tiles requested and not yet answered.
    pub pending: Signal<usize>,
    /// What the last frame drew.
    pub frame: Signal<Stats>,
    /// The language labels are shown in, as a BCP 47 language code (`fr`, `zh-Hant`), where
    /// the data has a name in it; `None` shows the style's own choice, usually the local name.
    /// Changing it reloads the style with its label fields rewritten (see
    /// [`crate::style::localize`]).
    pub language: Signal<Option<String>>,
    /// Degrees the map is turned clockwise: `0.0` is north up. A two-finger twist changes it;
    /// [`MapView::rotate_to`] and [`MapView::reset_north`] animate it.
    pub bearing: Signal<f64>,
    /// The markers shown, in the order added.
    pub markers: Signal<Vec<(MarkerId, Marker)>>,
    /// The last tap on the map, with what was under it. A double tap zooms instead and is not
    /// reported here.
    pub tap: Signal<Option<MapTap>>,
    pub(crate) request: Signal<Option<CameraRequest>>,
    next_marker: Signal<u64>,
}

impl MapView {
    pub fn new(center: LngLat, zoom: f64) -> MapView {
        MapView {
            center: Signal::new(center.wrapped()),
            zoom: Signal::new(zoom.clamp(0.0, MAX_ZOOM)),
            status: Signal::new(MapStatus::Loading),
            attribution: Signal::new(String::new()),
            pending: Signal::new(0),
            frame: Signal::new(Stats::default()),
            language: Signal::new(None),
            bearing: Signal::new(0.0),
            markers: Signal::new(Vec::new()),
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

    /// Glide to `center` at `zoom` over about a second, rising and settling the way Apple
    /// Maps and Google Maps fly: a long flight zooms out across the distance and back in.
    pub fn fly_to(&self, center: LngLat, zoom: f64) {
        self.fly_to_with(center, zoom, None, FLY_SECS);
    }

    /// [`MapView::fly_to`] with the bearing to arrive at (`None` keeps the current one) and
    /// the flight's duration in seconds; `0.0` jumps.
    pub fn fly_to_with(&self, center: LngLat, zoom: f64, bearing: Option<f64>, duration: f64) {
        self.request_move(
            CameraTarget::View {
                center: center.wrapped(),
                zoom: zoom.clamp(0.0, MAX_ZOOM),
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
                padding: padding.max(0.0),
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
            duration: duration.max(0.0),
            serial,
        }));
    }

    /// Show labels in `language` where the data has them (`MapView::language`).
    pub fn set_language(&self, language: Option<&str>) {
        self.language.set(language.map(str::to_string));
    }

    /// Zoom by `levels` (negative zooms out), keeping the center.
    pub fn zoom_by(&self, levels: f64) {
        let z = (self.zoom.get_untracked() + levels).clamp(0.0, MAX_ZOOM);
        self.zoom.set(z);
    }

    /// Look at `center` from `zoom`, at once.
    pub fn jump_to(&self, center: LngLat, zoom: f64) {
        self.center.set(center.wrapped());
        self.zoom.set(zoom.clamp(0.0, MAX_ZOOM));
    }

    /// [`MapView::jump_to`] with the bearing too (`0.0` is north up).
    pub fn jump_to_with(&self, center: LngLat, zoom: f64, bearing: f64) {
        self.jump_to(center, zoom);
        self.bearing.set(crate::camera::wrap_bearing(bearing));
    }
}

/// The marker that lets a string literal be a style (`map("<https://…",> view)`), next to the
/// `String`, `Signal<String>` and closure forms `IntoReactive` already covers.
pub struct StyleText;

impl IntoReactive<String, StyleText> for &str {
    fn into_reactive(self) -> day_pieces::Reactive<String> {
        day_pieces::Reactive::Const(self.to_string())
    }
}

/// A map of `style` showing `view`. `style` is the URL of a MapLibre style document, or the
/// document itself when the string starts with `{`; a reactive style (a `Signal<String>` or a
/// closure) reloads the map when it changes, which is how an app switches between a light and
/// a dark style.
pub fn map<M>(style: impl IntoReactive<String, M>, view: MapView) -> impl Piece {
    map_with_client(style, view, day_part_http::Client::new())
}

/// How many decoded tiles a map keeps before it drops the ones farthest from the view.
const TILE_CACHE: usize = 256;
/// How many rasterized screen tiles a map keeps; a 2x raster of a 512-point tile is 4 MB.
const RASTER_CACHE: usize = 32;
/// How many rasters render in one display frame's turn: each is a synchronous paint on the UI
/// thread, so a queue of them is spread across frames rather than stalling one.
const RASTER_PARALLEL: usize = 1;
/// Pixels per point of a raster. Day does not report the window's scale to a piece, and 2x is
/// crisp on every dense screen and only oversampled on a plain one.
const RASTER_SCALE: f64 = 2.0;
/// A raster paint slower than this, averaged over `SLOW_PAINT_SAMPLES` paints, moves a device
/// to 1x rasters.
const SLOW_PAINT_MS: f64 = 150.0;
const SLOW_PAINT_SAMPLES: usize = 6;
/// The deepest display zoom rasters are made for; the camera stops there too.
const RASTER_MAX_ZOOM: u8 = 22;
/// The glide after a swipe: the velocity at release decays by this factor every millisecond,
/// `UIScrollView`'s normal deceleration rate and the feel of Apple Maps.
const FLING_DECAY_PER_MS: f64 = 0.998;
/// A release slower than this, in points per second, is a stop, not a swipe.
const FLING_MIN_SPEED: f64 = 60.0;
/// The glide ends below this speed, in points per second.
const FLING_STOP_SPEED: f64 = 2.0;
/// The fastest glide, in points per second; a flick past this is clamped.
const FLING_MAX_SPEED: f64 = 5000.0;
/// How far back the release velocity is read from, in seconds.
const FLING_WINDOW: f64 = 0.1;
/// A finger that rested this long before lifting was a drag that stopped, not a swipe.
const FLING_REST: f64 = 0.15;
/// How far the view may move, as a fraction of its shorter side, before labels are placed
/// again.
const LABEL_SHIFT: f64 = 0.25;
/// The fewest frames between two label placements while the view keeps changing.
const LABEL_FRAMES: u64 = 6;
/// How many zoom levels below the display zoom rasters are cut. Zero: one raster per screen
/// tile. Painting runs off the UI thread on the toolkits that can, and a raster's cost is in
/// its paths rather than its pixels, so smaller rasters would repaint every road that crosses
/// a tile once per piece.
const RASTER_SUBDIVIDE: u8 = 0;

enum Slot {
    Loading,
    Loaded(Arc<TileData>),
    /// The source has no such tile, or it failed: nothing to draw and nothing to retry this
    /// session.
    Missing,
}

struct State {
    client: day_part_http::Client,
    style: Option<Arc<Style>>,
    sources: HashMap<String, Rc<dyn TileSource>>,
    tiles: HashMap<(String, TileId), Slot>,
    sprite: Option<Rc<Sprite>>,
    size: Size,
    /// A drag's translation at its last event, so each event applies only its own movement.
    drag_last: Point,
    /// The zoom when a pinch began; `None` between pinches, which is also how a lone wheel
    /// event (no `Began`) is told from a gesture.
    pinch_base: Option<f64>,
    /// The view a gesture moved to since the last frame, applied once per display frame: a
    /// trackpad reports faster than a frame can be recorded and replayed, and recording every
    /// event is what makes a pan lag behind the finger.
    pending: Option<Camera>,
    /// The frame request that will apply `pending`; dropping it cancels.
    frame: Option<day_core::frame::FrameHandle>,
    /// The base pass of each screen tile, rasterized once and blitted every frame.
    rasters: HashMap<TileId, Raster>,
    /// Screen tiles whose raster is wanted and not yet rendering, nearest the center first.
    raster_queue: Vec<(TileId, DataTiles)>,
    /// Rasters being rendered right now.
    rendering: usize,
    /// The frame request that will start the next queued rasters, and the frame it was made
    /// on: a toolkit that delivers no frames (the mock) gets the pump from a later draw instead.
    pump: Option<(day_core::frame::FrameHandle, u64)>,
    /// Frames drawn, the clock rasters are evicted by.
    frame_no: u64,
    /// Bumped when a tile or the sprite arrives: the labels are placed again.
    tiles_version: u64,
    /// The placed labels of the last placement, as one bitmap the frames blit.
    labels: Option<LabelLayer>,
    /// A label placement is being painted.
    labels_rendering: bool,
    /// The frame the last placement started on, so a zoom in progress re-places at a bounded
    /// rate rather than every frame.
    labels_placed_at: u64,
    /// The frame request that re-checks the labels after a placement came too soon.
    retry: Option<day_core::frame::FrameHandle>,
    /// How many tiles the last placement drew from: more tiles now means place again at once.
    labels_tiles: usize,
    /// A touch is down: its camera positions are sampled for the fling at release.
    tracking: bool,
    /// The last few camera positions (zoom-0 world units) with their frame times, seconds.
    samples: std::collections::VecDeque<(Point, f64)>,
    /// The fling in progress; dropping it stops the glide.
    fling: Option<day_core::frame::FrameHandle>,
    /// Pixels per point of the rasters: `RASTER_SCALE` until this device proves too slow for it.
    raster_scale: f64,
    /// The last few raster paint times, for that decision.
    paint_samples: Vec<f64>,
    /// The zooms a pinch passed through with their times, for its momentum at release.
    pinch_samples: std::collections::VecDeque<(f64, f64)>,
    /// Where the pinch was last, the point its momentum zooms about.
    pinch_location: Point,
    /// The zoom glide after a pinch; dropping it stops it.
    zoom_fling: Option<day_core::frame::FrameHandle>,
    /// The bearing when a twist began; `None` between twists, which is also how a lone
    /// trackpad tick is told from a gesture.
    rotate_base: Option<f64>,
    /// The twist has passed its dead zone and is turning the map.
    rotating: bool,
    /// A flight or turn in progress; dropping it stops it where it is.
    animation: Option<day_core::frame::FrameHandle>,
    /// The last single tap, waiting to learn whether a second one makes it a double.
    last_tap: Option<Point>,
    /// The frames counting down to reporting that tap as a single one.
    tap_timer: Option<day_core::frame::FrameHandle>,
    /// The data tiles the last frame drew from, for the tap's feature query.
    frame_tiles: Vec<RenderTile>,
    /// The last move request carried out.
    request_serial: u64,
}

/// The labels and icons placed for one view, rendered to a bitmap: frames blit it where its
/// world rectangle lands, and a new placement is made when the view has moved far enough,
/// zoomed, or gained tiles. Text stays crisp while panning and scales between placements
/// while zooming.
struct LabelLayer {
    bitmap: day_core::Bitmap,
    /// The view the labels were placed for; a frame maps that view's screen onto its own.
    camera: Camera,
    tiles_version: u64,
}

/// The loaded tile of each source that covers a screen tile.
type DataTiles = Vec<(String, Arc<TileData>)>;

/// One screen tile's base pass as a bitmap, and what it was rendered from.
struct Raster {
    /// The data tile of each source it was drawn from; a closer tile arriving makes it stale.
    data: Vec<(String, TileId)>,
    bitmap: Option<day_core::Bitmap>,
    /// Rendering, or queued to.
    pending: bool,
    /// The last frame that drew it.
    used: u64,
}

struct Shared {
    state: RefCell<State>,
    view: MapView,
    repaint: Trigger,
    /// The window's frame clock, captured while the piece is built (docs/frames.md).
    clock: day_core::frame::FrameClock,
    /// Cleared when the piece is torn down, so an answer arriving afterwards is dropped.
    alive: Cell<bool>,
    /// Bumped when the style changes, so an answer for the previous style is dropped.
    generation: Cell<u64>,
}

impl Shared {
    /// Whether an answer started under `generation` still has a map to land in.
    fn current(&self, generation: u64) -> bool {
        self.alive.get() && self.generation.get() == generation
    }

    /// Forget everything loaded for the previous style.
    fn reset(&self) {
        self.generation.set(self.generation.get() + 1);
        let mut st = self.state.borrow_mut();
        st.style = None;
        st.sources.clear();
        st.tiles.clear();
        st.sprite = None;
        st.rasters.clear();
        st.raster_queue.clear();
        st.labels = None;
        st.labels_tiles = 0;
        drop(st);
        self.view.status.set(MapStatus::Loading);
        self.view.attribution.set(String::new());
        self.view.pending.set(0);
        self.repaint.notify();
    }
}

/// [`map`] fetching through `client`, which is how a test intercepts the network
/// (`day_part_http::simulation::Simulation`) and how an app shares a client's cookies,
/// cache or headers with its map.
pub fn map_with_client<M>(
    style: impl IntoReactive<String, M>,
    view: MapView,
    client: day_part_http::Client,
) -> impl Piece {
    let style = style.into_reactive();
    let shared = Rc::new(Shared {
        state: RefCell::new(State {
            client,
            style: None,
            sources: HashMap::new(),
            tiles: HashMap::new(),
            sprite: None,
            size: Size::new(1.0, 1.0),
            drag_last: Point::ZERO,
            pinch_base: None,
            pending: None,
            frame: None,
            rasters: HashMap::new(),
            raster_queue: Vec::new(),
            rendering: 0,
            pump: None,
            frame_no: 0,
            tiles_version: 0,
            labels: None,
            labels_rendering: false,
            labels_placed_at: 0,
            retry: None,
            labels_tiles: 0,
            tracking: false,
            samples: std::collections::VecDeque::new(),
            fling: None,
            raster_scale: RASTER_SCALE,
            paint_samples: Vec::new(),
            pinch_samples: std::collections::VecDeque::new(),
            pinch_location: Point::ZERO,
            zoom_fling: None,
            rotate_base: None,
            rotating: false,
            animation: None,
            last_tap: None,
            tap_timer: None,
            frame_tiles: Vec::new(),
            request_serial: 0,
        }),
        view,
        repaint: Trigger::new(),
        clock: day_core::frame::FrameClock::current(),
        alive: Cell::new(true),
        generation: Cell::new(0),
    });
    {
        let shared = shared.clone();
        day_reactive::Scope::current().on_cleanup(move || shared.alive.set(false));
    }
    {
        // The style is tracked here and nowhere else: a change reloads, a frame never does.
        let shared = shared.clone();
        day_reactive::Effect::new(move || {
            let url = style.get();
            let language = shared.view.language.get();
            shared.reset();
            load_style(shared.clone(), url, language);
        });
    }

    {
        // Flights and turns the app asks for (`MapView::fly_to` and friends).
        let shared = shared.clone();
        day_reactive::Effect::new(move || {
            let Some(request) = shared.view.request.get() else {
                return;
            };
            if shared.state.borrow().request_serial == request.serial {
                return;
            }
            shared.state.borrow_mut().request_serial = request.serial;
            let from = camera_now(&shared);
            let target = match request.target {
                CameraTarget::View {
                    center,
                    zoom,
                    bearing,
                } => Camera::with_bearing(center, zoom, bearing.unwrap_or(from.bearing), from.size),
                CameraTarget::Bounds { sw, ne, padding } => {
                    Camera::fitting(sw, ne, from.size, padding)
                }
            };
            animate_to(&shared, target, request.duration);
        });
    }

    let draw = {
        let shared = shared.clone();
        move |d: &mut Draw, size: Size| draw_frame(&shared, d, size)
    };
    let on_drag = {
        let shared = shared.clone();
        move |drag: Drag| {
            let delta = {
                let mut st = shared.state.borrow_mut();
                match drag.phase {
                    DragPhase::Began => {
                        st.drag_last = drag.translation;
                        Point::ZERO
                    }
                    _ => {
                        let d = Point::new(
                            drag.translation.x - st.drag_last.x,
                            drag.translation.y - st.drag_last.y,
                        );
                        st.drag_last = drag.translation;
                        d
                    }
                }
            };
            match drag.phase {
                DragPhase::Began => begin_touch(&shared),
                DragPhase::Changed => {}
                DragPhase::Ended => {
                    pan_by(&shared, delta);
                    end_touch(&shared);
                    return;
                }
            }
            pan_by(&shared, delta);
        }
    };
    let on_pan = {
        let shared = shared.clone();
        move |pan: Pan| {
            // A two-finger pan is bracketed and glides like a drag; a wheel tick is a lone
            // `Changed` and the platform adds its own momentum.
            match pan.phase {
                DragPhase::Began => begin_touch(&shared),
                DragPhase::Changed => {}
                DragPhase::Ended => {
                    pan_by(&shared, pan.delta);
                    end_touch(&shared);
                    return;
                }
            }
            pan_by(&shared, pan.delta);
        }
    };
    let on_pinch = {
        let shared = shared.clone();
        move |pinch: Pinch| {
            let now = day_core::frame::now_secs();
            let zoom = {
                let current = camera_now(&shared).zoom;
                let mut st = shared.state.borrow_mut();
                st.pinch_location = pinch.location;
                match pinch.phase {
                    DragPhase::Began => {
                        stop_motion(&mut st);
                        st.pinch_base = Some(current);
                        st.pinch_samples.clear();
                        st.pinch_samples.push_back((current, now));
                        current
                    }
                    DragPhase::Changed => match st.pinch_base {
                        // Cumulative since Began.
                        Some(base) => {
                            let z = base + pinch.scale.max(1e-6).log2();
                            st.pinch_samples.push_back((z, now));
                            while st
                                .pinch_samples
                                .front()
                                .is_some_and(|(_, t)| now - t > FLING_WINDOW)
                            {
                                st.pinch_samples.pop_front();
                            }
                            z
                        }
                        // A lone wheel event: the scale is this event's own factor.
                        None => current + pinch.scale.max(1e-6).log2(),
                    },
                    DragPhase::Ended => {
                        let z = match st.pinch_base {
                            Some(base) => base + pinch.scale.max(1e-6).log2(),
                            None => current,
                        };
                        st.pinch_base = None;
                        z
                    }
                }
            };
            let camera = camera_now(&shared);
            move_to(&shared, camera.zoomed_about(zoom, pinch.location));
            if pinch.phase == DragPhase::Ended {
                end_pinch(&shared, zoom, now);
            }
        }
    };
    let on_tap = {
        let shared = shared.clone();
        move |point: Point| tap_at(&shared, point)
    };
    let on_rotate = {
        let shared = shared.clone();
        move |r: Rotate| rotate_by(&shared, r.phase, r.rotation, r.location)
    };
    canvas(draw)
        .on_drag(on_drag)
        .on_pan(on_pan)
        .on_pinch(on_pinch)
        .on_rotate(on_rotate)
        .on_tap_at(on_tap)
}

/// A two-finger twist (`Decorate::on_rotate`): the map turns about the fingers once the twist
/// has committed past a small dead zone (fingers turn a little in every pinch), and a twist
/// that ends within a few degrees of north settles on north.
fn rotate_by(shared: &Rc<Shared>, phase: DragPhase, rotation: f64, location: Point) {
    let degrees = rotation.to_degrees();
    let bearing = {
        let current = camera_now(shared).bearing;
        let mut st = shared.state.borrow_mut();
        match phase {
            DragPhase::Began => {
                st.rotate_base = Some(current);
                st.rotating = false;
                return;
            }
            DragPhase::Changed => match st.rotate_base {
                Some(base) => {
                    if !st.rotating {
                        if degrees.abs() < ROTATE_DEAD_ZONE {
                            return;
                        }
                        st.rotating = true;
                        st.fling = None;
                        st.animation = None;
                    }
                    // The dead zone is taken out, so the map starts turning from where it is.
                    base + degrees - ROTATE_DEAD_ZONE.copysign(degrees)
                }
                // A lone trackpad tick: the angle is this event's own.
                None => current + degrees,
            },
            DragPhase::Ended => {
                let b = match st.rotate_base {
                    Some(base) if st.rotating => {
                        base + degrees - ROTATE_DEAD_ZONE.copysign(degrees)
                    }
                    _ => current,
                };
                st.rotate_base = None;
                st.rotating = false;
                b
            }
        }
    };
    let camera = camera_now(shared);
    let turned = camera.turned_about(bearing, location);
    if phase == DragPhase::Ended && crate::camera::wrap_bearing(bearing).abs() < NORTH_SNAP {
        animate_to(shared, turned.turned(0.0), TURN_SECS);
        return;
    }
    move_to(shared, turned);
}

/// A twist ending within this many degrees of north snaps to it.
const NORTH_SNAP: f64 = 5.0;
/// A twist turns the map only once it has passed this many degrees, so the slight turn in
/// every pinch leaves north where it was.
const ROTATE_DEAD_ZONE: f64 = 4.0;

/// Stop every glide and flight: a finger came down, or a new gesture began.
fn stop_motion(st: &mut State) {
    st.fling = None;
    st.zoom_fling = None;
    st.animation = None;
    st.samples.clear();
}

/// The pinch lifted at `zoom`: keep zooming with the speed of its last tenth of a second,
/// decaying, about the point it was at, the way Apple Maps and Google Maps carry a pinch on.
fn end_pinch(shared: &Rc<Shared>, zoom: f64, now: f64) {
    let (velocity, anchor) = {
        let mut st = shared.state.borrow_mut();
        let first = st.pinch_samples.front().copied();
        st.pinch_samples.clear();
        let Some((z0, t0)) = first else {
            return;
        };
        let dt = now - t0;
        if dt < 1e-3 {
            return;
        }
        ((zoom - z0) / dt, st.pinch_location)
    };
    if velocity.abs() < ZOOM_FLING_MIN {
        return;
    }
    let mut v = velocity.clamp(-ZOOM_FLING_MAX, ZOOM_FLING_MAX);
    let shared2 = shared.clone();
    let handle = shared.clock.subscribe(move |frame| {
        let dt = frame.delta.as_secs_f64();
        if dt <= 0.0 {
            return std::ops::ControlFlow::Continue(());
        }
        v *= ZOOM_FLING_DECAY_PER_MS.powf(dt * 1000.0);
        if v.abs() < ZOOM_FLING_STOP {
            return std::ops::ControlFlow::Break(());
        }
        let camera = camera_now(&shared2);
        let target = camera.zoomed_about(camera.zoom + v * dt, anchor);
        if target.zoom == camera.zoom {
            return std::ops::ControlFlow::Break(());
        }
        move_to(&shared2, target);
        std::ops::ControlFlow::Continue(())
    });
    shared.state.borrow_mut().zoom_fling = Some(handle);
}

/// A pinch released slower than this, in zoom levels per second, stops dead.
const ZOOM_FLING_MIN: f64 = 0.4;
/// The fastest zoom glide, in levels per second.
const ZOOM_FLING_MAX: f64 = 6.0;
/// The glide ends below this speed.
const ZOOM_FLING_STOP: f64 = 0.02;
/// A zoom glide is shorter than a pan's: the speed decays by this factor every millisecond.
const ZOOM_FLING_DECAY_PER_MS: f64 = 0.992;

/// A tap: a second one within `DOUBLE_TAP_SECS` and `DOUBLE_TAP_SLOP` points zooms in a level
/// about the spot; otherwise, once that window has passed, the tap is reported with what was
/// under it.
fn tap_at(shared: &Rc<Shared>, point: Point) {
    // The window is counted in display frames, the clock gestures already run on: a second tap
    // while the first one's timer is still running, and close to it, is a double tap.
    let double = {
        let mut st = shared.state.borrow_mut();
        let double = st.tap_timer.is_some()
            && st.last_tap.is_some_and(|p| {
                ((p.x - point.x).powi(2) + (p.y - point.y).powi(2)).sqrt() <= DOUBLE_TAP_SLOP
            });
        st.tap_timer = None;
        st.last_tap = if double { None } else { Some(point) };
        double
    };
    if double {
        let camera = camera_now(shared);
        let target = camera.zoomed_about((camera.zoom + 1.0).round(), point);
        animate_to(shared, target, TAP_ZOOM_SECS);
        return;
    }
    let shared2 = shared.clone();
    let mut elapsed = 0.0;
    let handle = shared.clock.subscribe(move |frame| {
        elapsed += frame.delta.as_secs_f64();
        if elapsed < DOUBLE_TAP_SECS {
            return std::ops::ControlFlow::Continue(());
        }
        shared2.state.borrow_mut().tap_timer = None;
        report_tap(&shared2, point);
        std::ops::ControlFlow::Break(())
    });
    shared.state.borrow_mut().tap_timer = Some(handle);
}

/// Two taps this close in time and space are a double tap.
const DOUBLE_TAP_SECS: f64 = 0.3;
const DOUBLE_TAP_SLOP: f64 = 30.0;

/// Report a single tap: the marker under it, else the features the last frame drew there.
fn report_tap(shared: &Rc<Shared>, point: Point) {
    let camera = camera_now(shared);
    let location = camera.unproject(point);
    let marker = shared
        .view
        .markers
        .get_untracked()
        .iter()
        .rev()
        .find(|(_, m)| marker_hit(&camera, m, point))
        .map(|(id, _)| *id);
    let features = if marker.is_some() {
        Vec::new()
    } else {
        let st = shared.state.borrow();
        match &st.style {
            Some(style) => render::features_at(style, &camera, &st.frame_tiles, point),
            None => Vec::new(),
        }
    };
    shared.view.tap.set(Some(MapTap {
        point,
        location,
        marker,
        features,
    }));
}

/// Glide from the current view to `target` over `duration` seconds: an ease-in-out on the
/// center, zoom and bearing, with a long flight dipping out to a zoom that shows both ends,
/// as MapLibre's `flyTo` does. `0.0` jumps.
fn animate_to(shared: &Rc<Shared>, target: Camera, duration: f64) {
    let from = camera_now(shared);
    {
        let mut st = shared.state.borrow_mut();
        stop_motion(&mut st);
    }
    if duration <= 0.0
        || (from.center == target.center
            && from.zoom == target.zoom
            && from.bearing == target.bearing)
    {
        move_to(shared, target);
        return;
    }
    let a = from.center.to_world();
    let mut b = target.center.to_world();
    // The short way around the globe.
    if (b.x - a.x).abs() > crate::camera::WORLD / 2.0 {
        b.x += if b.x > a.x {
            -crate::camera::WORLD
        } else {
            crate::camera::WORLD
        };
    }
    // The zoom that shows the whole path, and how far the flight dips toward it.
    let span = ((b.x - a.x).powi(2) + (b.y - a.y).powi(2)).sqrt();
    let dip = if span > 0.0 {
        let fit = (from.size.width.min(from.size.height) / span).log2();
        (from.zoom.min(target.zoom) - fit).clamp(0.0, 6.0)
    } else {
        0.0
    };
    let db = crate::camera::wrap_bearing(target.bearing - from.bearing);
    let mut started: Option<f64> = None;
    let shared2 = shared.clone();
    let handle = shared.clock.subscribe(move |frame| {
        let now = frame.timestamp.as_secs_f64();
        let t0 = *started.get_or_insert(now);
        let t = ((now - t0) / duration).clamp(0.0, 1.0);
        let e = ease_in_out(t);
        let center = LngLat::from_world(Point::new(a.x + (b.x - a.x) * e, a.y + (b.y - a.y) * e));
        let zoom =
            from.zoom + (target.zoom - from.zoom) * e - dip * (std::f64::consts::PI * e).sin();
        let bearing = from.bearing + db * e;
        let camera = Camera::with_bearing(center, zoom, bearing, from.size);
        if t >= 1.0 {
            move_to(&shared2, target);
            shared2.state.borrow_mut().animation = None;
            return std::ops::ControlFlow::Break(());
        }
        move_to(&shared2, camera);
        std::ops::ControlFlow::Continue(())
    });
    shared.state.borrow_mut().animation = Some(handle);
}

/// Cubic ease-in-out.
fn ease_in_out(t: f64) -> f64 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// Whether `point` is on the marker: its head, or its stem down to the position.
fn marker_hit(camera: &Camera, marker: &Marker, point: Point) -> bool {
    let p = camera.project(marker.position);
    let head = Point::new(p.x, p.y - MARKER_STEM);
    let on_head =
        ((head.x - point.x).powi(2) + (head.y - point.y).powi(2)).sqrt() <= MARKER_HEAD + 6.0;
    let t = ((point.y - head.y) / MARKER_STEM).clamp(0.0, 1.0);
    let on_stem = (point.x - head.x).abs() <= MARKER_HEAD
        && (point.y - (head.y + t * MARKER_STEM)).abs() <= MARKER_HEAD;
    on_head || on_stem
}

/// A marker: a round head on a stem down to its position, with a halo and the title beside.
const MARKER_HEAD: f64 = 9.0;
const MARKER_STEM: f64 = 18.0;

fn draw_markers(d: &mut Draw, camera: &Camera, markers: &[(MarkerId, Marker)]) {
    for (_, m) in markers {
        let p = camera.project(m.position);
        if p.x < -40.0
            || p.y < -40.0
            || p.x > camera.size.width + 40.0
            || p.y > camera.size.height + 40.0
        {
            continue;
        }
        let head = Point::new(p.x, p.y - MARKER_STEM);
        let stem = Shape::Polygon(vec![
            Point::new(head.x - MARKER_HEAD * 0.7, head.y + MARKER_HEAD * 0.6),
            Point::new(head.x + MARKER_HEAD * 0.7, head.y + MARKER_HEAD * 0.6),
            p,
        ]);
        d.fill(stem.clone(), Color::WHITE);
        d.stroke(stem.clone(), Color::WHITE, 3.0);
        d.fill(stem, m.color);
        let disc = Shape::Ellipse(Rect::new(
            head.x - MARKER_HEAD - 1.5,
            head.y - MARKER_HEAD - 1.5,
            2.0 * MARKER_HEAD + 3.0,
            2.0 * MARKER_HEAD + 3.0,
        ));
        d.fill(disc, Color::WHITE);
        d.fill(
            Shape::Ellipse(Rect::new(
                head.x - MARKER_HEAD,
                head.y - MARKER_HEAD,
                2.0 * MARKER_HEAD,
                2.0 * MARKER_HEAD,
            )),
            m.color,
        );
        d.fill(
            Shape::Ellipse(Rect::new(head.x - 3.0, head.y - 3.0, 6.0, 6.0)),
            Color::WHITE,
        );
        if !m.title.is_empty() {
            let at = Point::new(head.x + MARKER_HEAD + 6.0, head.y);
            let font = render::font_for(&["Noto Sans Bold".to_string()]);
            let halo = TextStyle {
                size: 13.0,
                color: Color::WHITE,
                anchor: TextAnchor {
                    h: day_spec::props::TextAlign::Leading,
                    v: day_spec::TextVAlign::Middle,
                },
                font: font.clone(),
            };
            for (dx, dy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                d.text(&m.title, Point::new(at.x + dx, at.y + dy), halo.clone());
            }
            d.text(
                &m.title,
                at,
                TextStyle {
                    color: Color::rgb(0.1, 0.1, 0.1),
                    ..halo
                },
            );
        }
    }
}

/// The camera a gesture continues from: the one a previous event in this frame moved to, else
/// the view's.
fn camera_now(shared: &Rc<Shared>) -> Camera {
    let st = shared.state.borrow();
    st.pending.unwrap_or_else(|| {
        Camera::with_bearing(
            shared.view.center.get_untracked(),
            shared.view.zoom.get_untracked(),
            shared.view.bearing.get_untracked(),
            st.size,
        )
    })
}

/// Move to `camera` at the next display frame. Several events in one frame leave one camera,
/// one signal write and one re-record.
fn move_to(shared: &Rc<Shared>, camera: Camera) {
    let mut st = shared.state.borrow_mut();
    st.pending = Some(camera);
    if st.frame.is_none() {
        let shared = shared.clone();
        st.frame = Some(shared.clone().clock.request(move |frame| {
            let camera = {
                let mut st = shared.state.borrow_mut();
                st.frame = None;
                st.pending.take()
            };
            if let Some(camera) = camera {
                shared.view.center.set(camera.center);
                shared.view.zoom.set(camera.zoom);
                if shared.view.bearing.get_untracked() != camera.bearing {
                    shared.view.bearing.set(camera.bearing);
                }
                let mut st = shared.state.borrow_mut();
                if st.tracking {
                    let t = frame.timestamp.as_secs_f64();
                    st.samples.push_back((camera.center.to_world(), t));
                    while st
                        .samples
                        .front()
                        .is_some_and(|(_, t0)| t - t0 > FLING_WINDOW)
                    {
                        st.samples.pop_front();
                    }
                }
            }
        }));
    }
}

/// A finger came down: whatever was gliding stops, and its positions are sampled from here.
fn begin_touch(shared: &Rc<Shared>) {
    let mut st = shared.state.borrow_mut();
    stop_motion(&mut st);
    st.tracking = true;
}

/// The finger lifted: read the velocity of its last tenth of a second and, if it was a swipe,
/// glide on with it, decaying as `UIScrollView` does. Tiles and labels follow the camera as
/// they do under a finger.
fn end_touch(shared: &Rc<Shared>) {
    let (velocity, last_t, zoom, size) = {
        let mut st = shared.state.borrow_mut();
        st.tracking = false;
        let (Some((p0, t0)), Some((p1, t1))) =
            (st.samples.front().copied(), st.samples.back().copied())
        else {
            return;
        };
        st.samples.clear();
        let dt = t1 - t0;
        if dt < 1e-3 {
            return;
        }
        (
            Point::new((p1.x - p0.x) / dt, (p1.y - p0.y) / dt),
            t1,
            shared.view.zoom.get_untracked(),
            st.size,
        )
    };
    let scale = 2f64.powf(zoom);
    let speed = (velocity.x * velocity.x + velocity.y * velocity.y).sqrt() * scale;
    if speed < FLING_MIN_SPEED {
        return;
    }
    let clamp = (FLING_MAX_SPEED / speed).min(1.0);
    let mut v = Point::new(velocity.x * clamp, velocity.y * clamp);
    let mut started: Option<f64> = None;
    let shared2 = shared.clone();
    let handle = shared.clock.subscribe(move |frame| {
        let now = frame.timestamp.as_secs_f64();
        let dt = frame.delta.as_secs_f64();
        if started.is_none() {
            // A finger that rested before lifting was not swiping.
            if now - last_t > FLING_REST {
                return std::ops::ControlFlow::Break(());
            }
            started = Some(now);
        }
        if dt <= 0.0 {
            return std::ops::ControlFlow::Continue(());
        }
        let decay = FLING_DECAY_PER_MS.powf(dt * 1000.0);
        v = Point::new(v.x * decay, v.y * decay);
        let speed = (v.x * v.x + v.y * v.y).sqrt() * scale;
        if speed < FLING_STOP_SPEED {
            return std::ops::ControlFlow::Break(());
        }
        let center = shared2.view.center.get_untracked().to_world();
        let moved = Point::new(center.x + v.x * dt, center.y + v.y * dt);
        let camera = Camera::new(LngLat::from_world(moved), zoom, size);
        shared2.view.center.set(camera.center);
        std::ops::ControlFlow::Continue(())
    });
    shared.state.borrow_mut().fling = Some(handle);
}

fn pan_by(shared: &Rc<Shared>, delta: Point) {
    if delta == Point::ZERO {
        return;
    }
    let camera = camera_now(shared);
    move_to(shared, camera.panned(delta));
}

/// The frame: what the camera needs, fetched if missing, drawn from what has arrived.
fn draw_frame(shared: &Rc<Shared>, d: &mut Draw, size: Size) {
    shared.repaint.track();
    let center = shared.view.center.get();
    let zoom = shared.view.zoom.get();
    let bearing = shared.view.bearing.get();
    let markers = shared.view.markers.get();
    let camera = Camera::with_bearing(center, zoom, bearing, size);
    let viewport = Rect::from_size(size);
    let (style, sprite, tiles, screen) = {
        let mut st = shared.state.borrow_mut();
        st.size = size;
        st.frame_no += 1;
        let Some(style) = st.style.clone() else {
            drop(st);
            d.fill(Shape::Rect(viewport), Color::rgb(0.93, 0.93, 0.92));
            return;
        };
        let sprite = st.sprite.clone();
        let mut tiles: Vec<RenderTile> = Vec::new();
        let mut needed_keys: HashSet<(String, TileId)> = HashSet::new();
        let mut to_fetch: Vec<(String, TileId)> = Vec::new();
        let sources: Vec<(String, Rc<dyn TileSource>)> = st
            .sources
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (name, source) in &sources {
            let z = camera.tile_zoom(source.min_zoom(), source.max_zoom());
            let needed = camera.covering_tiles(z);
            for (id, _) in &needed {
                let key = (name.clone(), *id);
                needed_keys.insert(key.clone());
                if !st.tiles.contains_key(&key) {
                    st.tiles.insert(key.clone(), Slot::Loading);
                    to_fetch.push(key);
                }
            }
            let lookup = |id: TileId| match st.tiles.get(&(name.clone(), id)) {
                Some(Slot::Loaded(data)) => Some(data.clone()),
                _ => None,
            };
            tiles.extend(render::resolve(&camera, name, &needed, &lookup));
        }
        // The screen tiles at the display zoom, each with the data tile of every source that
        // covers it: what a raster is keyed and rendered by.
        // The raster tiles: the screen tiles at the display zoom, or finer by `RASTER_SUBDIVIDE`.
        let z_display = ((zoom + 0.001).floor() + f64::from(RASTER_SUBDIVIDE))
            .clamp(0.0, f64::from(RASTER_MAX_ZOOM)) as u8;
        let mut screen: Vec<(TileId, i32, DataTiles)> = Vec::new();
        for (id, wrap) in camera.covering_tiles(z_display) {
            let mut data = Vec::new();
            for (name, source) in &sources {
                let mut probe = Some(ancestor_at(id, source.max_zoom()));
                while let Some(p) = probe {
                    if let Some(Slot::Loaded(found)) = st.tiles.get(&(name.clone(), p)) {
                        data.push((name.clone(), found.clone()));
                        break;
                    }
                    probe = p.parent();
                }
            }
            screen.push((id, wrap, data));
        }
        // Keep the tile cache bounded: drop what the view does not need, farthest zoom first.
        if st.tiles.len() > TILE_CACHE {
            let mut spare: Vec<(String, TileId)> = st
                .tiles
                .iter()
                .filter(|(k, slot)| !needed_keys.contains(*k) && !matches!(slot, Slot::Loading))
                .map(|(k, _)| k.clone())
                .collect();
            spare.sort_by_key(|(_, id)| std::cmp::Reverse((i32::from(id.z) - zoom as i32).abs()));
            let excess = st.tiles.len() - TILE_CACHE;
            for key in spare.into_iter().take(excess) {
                st.tiles.remove(&key);
            }
        }
        st.frame_tiles = tiles.clone();
        drop(st);
        if !to_fetch.is_empty() {
            let n = shared.view.pending.get_untracked() + to_fetch.len();
            shared.view.pending.set(n);
            for (name, id) in to_fetch {
                fetch_tile(shared.clone(), name, id);
            }
        }
        (style, sprite, tiles, screen)
    };
    let started = Stopwatch::start();
    let _ = render::take_bucket_time();
    // Nothing leaves the canvas: rasters and the label layer are blitted here, outside the
    // renderer's own clip, and not every toolkit clips a canvas to its frame on its own.
    d.save();
    d.clip(Shape::Rect(viewport));
    // The backdrop everywhere, then each screen tile's raster where it exists, a scaled
    // ancestor or its children while it renders, and the rasters still wanted go on the queue.
    render::render(
        d,
        &Scene {
            style: &style,
            camera: &camera,
            tiles: &[],
            sprite: None,
            measure: &render::approximate_measure,
            pass: Pass::Backdrop,
        },
    );
    let mut blits = 0usize;
    // The tiles are laid out north-up and the whole layer is turned by the bearing.
    d.save();
    if bearing != 0.0 {
        d.concat(turn_about(camera.middle(), camera.bearing_radians()));
    }
    {
        let mut st = shared.state.borrow_mut();
        let frame_no = st.frame_no;
        let mut queue: Vec<(f64, TileId, DataTiles)> = Vec::new();
        let center = Point::new(size.width / 2.0, size.height / 2.0);
        let mut wanted: HashSet<TileId> = HashSet::new();
        for (id, wrap, data) in &screen {
            wanted.insert(*id);
            let rect = camera.tile_rect(*id, *wrap);
            let key: Vec<(String, TileId)> = data.iter().map(|(s, t)| (s.clone(), t.id)).collect();
            let ready = match st.rasters.get_mut(id) {
                Some(r) if r.data == key => {
                    r.used = frame_no;
                    r.bitmap.clone()
                }
                _ => None,
            };
            match ready {
                Some(bitmap) => {
                    // A turned layer shows a hairline between neighbours where their edges
                    // are smoothed; a half-point overlap hides it.
                    let rect = if bearing != 0.0 {
                        Rect::new(
                            rect.origin.x - 0.5,
                            rect.origin.y - 0.5,
                            rect.size.width + 1.0,
                            rect.size.height + 1.0,
                        )
                    } else {
                        rect
                    };
                    d.image(&bitmap, rect);
                    blits += 1;
                }
                None => {
                    if !data.is_empty() {
                        let fresh = st.rasters.get(id).is_none_or(|r| r.data != key);
                        let pending = st.rasters.get(id).is_some_and(|r| r.pending);
                        if fresh && !pending {
                            let dx = rect.origin.x + rect.size.width / 2.0 - center.x;
                            let dy = rect.origin.y + rect.size.height / 2.0 - center.y;
                            queue.push((dx * dx + dy * dy, *id, data.clone()));
                        }
                    }
                    blits += draw_stand_in(d, &mut st.rasters, *id, rect, frame_no);
                }
            }
        }
        // Nearest the center renders first.
        queue.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        st.raster_queue = queue.into_iter().map(|(_, id, data)| (id, data)).collect();
        // Rasters the view no longer shows go first when the cache is full.
        if st.rasters.len() > RASTER_CACHE {
            let mut spare: Vec<(TileId, u64)> = st
                .rasters
                .iter()
                .filter(|(id, r)| !wanted.contains(*id) && !r.pending)
                .map(|(id, r)| (*id, r.used))
                .collect();
            spare.sort_by_key(|(_, used)| *used);
            let excess = st.rasters.len() - RASTER_CACHE;
            for (id, _) in spare.into_iter().take(excess) {
                st.rasters.remove(&id);
            }
        }
    }
    d.restore();
    schedule_pump(shared);
    // The labels: blit the last placement where its view lands on this one, and start a new
    // placement when the view has moved on from it.
    let (stale, layer) = {
        let st = shared.state.borrow();
        match &st.labels {
            None => (true, None),
            Some(l) => {
                let then_center = camera.world_to_screen(l.camera.center.to_world());
                let m = camera.middle();
                let shifted = ((then_center.x - m.x).powi(2) + (then_center.y - m.y).powi(2))
                    .sqrt()
                    > LABEL_SHIFT * size.width.min(size.height);
                let zoomed = (l.camera.zoom - zoom).abs() > 0.01;
                let turned = crate::camera::wrap_bearing(l.camera.bearing - bearing).abs() > 0.5;
                (
                    shifted
                        || zoomed
                        || turned
                        || l.camera.size != size
                        || l.tiles_version != st.tiles_version,
                    Some((l.bitmap.clone(), view_onto(&l.camera, &camera))),
                )
            }
        }
    };
    if let Some((bitmap, onto)) = layer {
        d.save();
        d.concat(onto);
        d.image(&bitmap, Rect::from_size(size));
        d.restore();
    }
    draw_markers(d, &camera, &markers);
    d.restore();
    let elapsed = started.elapsed();
    let buckets = render::take_bucket_time();
    log::debug!(
        "day-piece-map: frame {:.1} ms ({:.1} ms in buckets), {} ops, {} blits, {} tiles",
        elapsed.as_secs_f64() * 1000.0,
        buckets.as_secs_f64() * 1000.0,
        d.ops().len(),
        blits,
        tiles.len()
    );
    let (placing, waited, empty, grew, retrying) = {
        let st = shared.state.borrow();
        (
            st.labels_rendering,
            st.frame_no.saturating_sub(st.labels_placed_at) >= LABEL_FRAMES,
            st.labels.is_none(),
            tiles.len() > st.labels_tiles,
            st.retry.is_some(),
        )
    };
    // Place again when the view moved on, at a bounded rate while it keeps changing, and at
    // once when there was no placement yet or tiles arrived since the last one.
    if stale && !placing && (waited || empty || grew) {
        place_labels(shared, &camera, &style, sprite.as_deref(), &tiles, blits);
    } else if stale && !placing && !retrying {
        // Too soon after the last placement: ask for a frame to try again. The handle is kept,
        // since dropping it cancels the request.
        let shared2 = shared.clone();
        let handle = shared.clock.request(move |_| {
            shared2.state.borrow_mut().retry = None;
            shared2.repaint.notify();
        });
        shared.state.borrow_mut().retry = Some(handle);
    }
    // The readout's tile count is this frame's; the rest of the statistics are the last
    // placement's.
    let mut stats = shared.view.frame.get_untracked();
    if stats.bitmaps != blits {
        stats.bitmaps = blits;
        shared.view.frame.set(stats);
    }
    draw_attribution(d, size, &shared.view.attribution.get());
    if shared.view.status.get_untracked() == MapStatus::Loading {
        let message = "Loading";
        d.text(
            message,
            Point::new(size.width / 2.0, size.height / 2.0),
            TextStyle {
                size: 13.0,
                color: Color::rgb(0.4, 0.4, 0.4),
                anchor: TextAnchor::CENTERED,
                font: Default::default(),
            },
        );
    }
}

/// The world rectangle (zoom-0 units) a viewport of `size` shows.
/// The transform that turns the screen by `radians` about `center`.
fn turn_about(center: Point, radians: f64) -> Affine {
    Affine::translate(-center.x, -center.y)
        .then(Affine::rotate(radians))
        .then(Affine::translate(center.x, center.y))
}

/// The transform that lays the screen of `then` (a past view) onto the screen of `now`: the
/// same world shows at a different scale, turn and offset, so a bitmap placed for `then` is
/// drawn through this until a new placement lands.
fn view_onto(then: &Camera, now: &Camera) -> Affine {
    let k = now.scale() / then.scale();
    let turn = (now.bearing - then.bearing).to_radians();
    let m = then.middle();
    let shows = now.world_to_screen(then.center.to_world());
    Affine::translate(-m.x, -m.y)
        .then(Affine::scale(k, k))
        .then(Affine::rotate(turn))
        .then(Affine::translate(shows.x, shows.y))
}

/// Place the labels for this view and paint them into a bitmap, off the frame: the symbol
/// pass recorded now, rendered through the toolkit's offscreen painter, kept as the label
/// layer when it lands.
#[allow(clippy::too_many_arguments)]
fn place_labels(
    shared: &Rc<Shared>,
    camera: &Camera,
    style: &Arc<Style>,
    sprite: Option<&Sprite>,
    tiles: &[RenderTile],
    blits: usize,
) {
    let started = Stopwatch::start();
    let mut d = Draw::new();
    let mut stats = render::render(
        &mut d,
        &Scene {
            style,
            camera,
            tiles,
            sprite,
            measure: &|text, size, font| day_core::measure_text(text, size, font),
            pass: Pass::Symbols,
        },
    );
    stats.bitmaps = blits;
    let ops = d.ops().to_vec();
    let placed = started.elapsed();
    let size = camera.size;
    let placed_for = *camera;
    let (frame_no, tiles_version) = {
        let mut st = shared.state.borrow_mut();
        st.labels_rendering = true;
        st.labels_placed_at = st.frame_no;
        st.labels_tiles = tiles.len();
        (st.frame_no, st.tiles_version)
    };
    if shared.view.frame.get_untracked() != stats {
        shared.view.frame.set(stats);
    }
    let generation = shared.generation.get();
    let shared = shared.clone();
    day_core::task(async move {
        let result = day_core::render_bitmap(ops, size, RASTER_SCALE).await;
        let mut st = shared.state.borrow_mut();
        st.labels_rendering = false;
        if !shared.current(generation) {
            return;
        }
        match result {
            Ok(bitmap) => {
                log::debug!(
                    "day-piece-map: labels at frame {frame_no}: placed in {:.1} ms, painted in {:.1} ms, {stats:?}",
                    placed.as_secs_f64() * 1000.0,
                    (started.elapsed() - placed).as_secs_f64() * 1000.0
                );
                st.labels = Some(LabelLayer {
                    bitmap,
                    camera: placed_for,
                    tiles_version,
                });
            }
            Err(e) => log::warn!("day-piece-map: labels: {e}"),
        }
        drop(st);
        shared.repaint.notify();
    });
}

/// The tile at or above `id` that a source with `max_zoom` can have: `id` itself within the
/// source's range, else its ancestor at the source's deepest zoom.
fn ancestor_at(id: TileId, max_zoom: u8) -> TileId {
    if id.z <= max_zoom {
        return id;
    }
    let shift = u32::from(id.z - max_zoom);
    TileId::new(max_zoom, id.x >> shift, id.y >> shift)
}

/// Draw what stands in for a screen tile whose raster is not ready: the matching region of the
/// nearest rasterized ancestor, else whichever of its children are rasterized. Answers how many
/// bitmaps it drew.
fn draw_stand_in(
    d: &mut Draw,
    rasters: &mut HashMap<TileId, Raster>,
    id: TileId,
    rect: Rect,
    frame_no: u64,
) -> usize {
    let mut probe = id.parent();
    let mut levels = 1u32;
    while let Some(p) = probe {
        if levels > 4 {
            break;
        }
        if let Some(r) = rasters.get_mut(&p)
            && let Some(bitmap) = r.bitmap.clone()
        {
            r.used = frame_no;
            // The part of the ancestor's bitmap that is this tile, in its own pixels.
            let n = f64::from(1u32 << levels);
            let pixels = bitmap.info().pixels;
            let (cw, ch) = (pixels.width / n, pixels.height / n);
            let sx = f64::from(id.x - (p.x << levels)) * cw;
            let sy = f64::from(id.y - (p.y << levels)) * ch;
            d.image_region(&bitmap, Rect::new(sx, sy, cw, ch), rect);
            return 1;
        }
        probe = p.parent();
        levels += 1;
    }
    let mut drawn = 0;
    if id.z < RASTER_MAX_ZOOM {
        let half = Size::new(rect.size.width / 2.0, rect.size.height / 2.0);
        for (i, child) in id.children().iter().enumerate() {
            if let Some(r) = rasters.get_mut(child)
                && let Some(bitmap) = r.bitmap.clone()
            {
                r.used = frame_no;
                let at = Point::new(
                    rect.origin.x + half.width * f64::from((i % 2) as u32),
                    rect.origin.y + half.height * f64::from((i / 2) as u32),
                );
                d.image(&bitmap, Rect::new(at.x, at.y, half.width, half.height));
                drawn += 1;
            }
        }
    }
    drawn
}

/// Paint queued rasters on the next display frame rather than inside this one: a paint is a
/// synchronous replay on the UI thread, and a frame that also carried one would stutter.
fn schedule_pump(shared: &Rc<Shared>) {
    let mut st = shared.state.borrow_mut();
    if st.raster_queue.is_empty() || st.rendering >= RASTER_PARALLEL {
        return;
    }
    if let Some((_, asked)) = &st.pump {
        if st.frame_no.saturating_sub(*asked) < 3 {
            return;
        }
        // Three draws and no frame callback: paint from here.
        st.pump = None;
        drop(st);
        pump_rasters(shared);
        return;
    }
    let shared2 = shared.clone();
    let asked = st.frame_no;
    st.pump = Some((
        shared.clock.request(move |_| {
            shared2.state.borrow_mut().pump = None;
            pump_rasters(&shared2);
        }),
        asked,
    ));
}

/// Start rendering queued rasters, up to the parallel limit.
fn pump_rasters(shared: &Rc<Shared>) {
    loop {
        let next = {
            let mut st = shared.state.borrow_mut();
            if st.rendering >= RASTER_PARALLEL || st.raster_queue.is_empty() {
                None
            } else {
                let (id, data) = st.raster_queue.remove(0);
                st.rendering += 1;
                let key = data.iter().map(|(s, t)| (s.clone(), t.id)).collect();
                let used = st.frame_no;
                st.rasters.insert(
                    id,
                    Raster {
                        data: key,
                        bitmap: None,
                        pending: true,
                        used,
                    },
                );
                Some((id, data))
            }
        };
        let Some((id, data)) = next else { return };
        start_raster(shared.clone(), id, data);
    }
}

/// Render one screen tile's base pass into a bitmap, through the toolkit's offscreen painter,
/// and put it in the cache when it lands.
fn start_raster(shared: Rc<Shared>, id: TileId, data: DataTiles) {
    let generation = shared.generation.get();
    let (style, sprite) = {
        let st = shared.state.borrow();
        (st.style.clone(), st.sprite.clone())
    };
    let Some(style) = style else {
        shared.state.borrow_mut().rendering -= 1;
        return;
    };
    let display_zoom = f64::from(id.z.saturating_sub(RASTER_SUBDIVIDE));
    let scale = shared.state.borrow().raster_scale;
    day_core::task(async move {
        let started = Stopwatch::start();
        let (ops, size) = render::tile_drawing(&style, id, display_zoom, &data, sprite.as_deref());
        let recorded = started.elapsed();
        let counts = if log::log_enabled!(log::Level::Debug) {
            geometry_counts(&ops)
        } else {
            (0, 0, 0, 0, 0)
        };
        let result = day_core::render_bitmap(ops, size, scale).await;
        let mut st = shared.state.borrow_mut();
        st.rendering = st.rendering.saturating_sub(1);
        if !shared.current(generation) {
            return;
        }
        match result {
            Ok(bitmap) => {
                let painted = (started.elapsed() - recorded).as_secs_f64() * 1000.0;
                log::debug!(
                    "day-piece-map: raster {id:?}: recorded in {:.1} ms, painted in {:.1} ms at {scale}x ({} ops, {} fills, {} strokes, {} segments, {} images)",
                    recorded.as_secs_f64() * 1000.0,
                    painted,
                    counts.0,
                    counts.1,
                    counts.2,
                    counts.3,
                    counts.4,
                );
                if let Some(r) = st.rasters.get_mut(&id) {
                    r.bitmap = Some(bitmap);
                    r.pending = false;
                }
                // A device whose painter is this slow gets its rasters at 1x from here on:
                // a quarter of the pixels, and a map that keeps up beats one that is sharp.
                if st.raster_scale > 1.0 {
                    st.paint_samples.push(painted);
                    if st.paint_samples.len() >= SLOW_PAINT_SAMPLES {
                        let mean =
                            st.paint_samples.iter().sum::<f64>() / st.paint_samples.len() as f64;
                        if mean > SLOW_PAINT_MS {
                            log::info!(
                                "day-piece-map: rasters average {mean:.0} ms to paint at 2x; switching to 1x"
                            );
                            st.raster_scale = 1.0;
                            st.rasters.clear();
                        }
                        st.paint_samples.clear();
                    }
                }
            }
            Err(e) => {
                log::warn!("day-piece-map: raster {id:?}: {e}");
                st.rasters.remove(&id);
            }
        }
        drop(st);
        pump_rasters(&shared);
        shared.repaint.notify();
    });
}

/// The credit line in the map's bottom-right corner, on a translucent strip, as every map
/// shows it: the tile services' terms require it to be visible with the map.
fn draw_attribution(d: &mut Draw, size: Size, text: &str) {
    if text.is_empty() {
        return;
    }
    let font = day_spec::CanvasFont::default();
    let metrics = day_core::measure_text(text, ATTRIBUTION_SIZE, &font);
    let (pad_x, pad_y) = (6.0, 3.0);
    let width = metrics.width + 2.0 * pad_x;
    let height = metrics.height + 2.0 * pad_y;
    let strip = Rect::new(
        (size.width - width).max(0.0),
        (size.height - height).max(0.0),
        width,
        height,
    );
    d.fill(Shape::Rect(strip), Color::rgba(1.0, 1.0, 1.0, 0.75));
    d.text(
        text,
        Point::new(strip.origin.x + pad_x, strip.origin.y + pad_y),
        TextStyle {
            size: ATTRIBUTION_SIZE,
            color: Color::rgb(0.2, 0.2, 0.2),
            anchor: TextAnchor::LEADING,
            font,
        },
    );
}

/// The attribution strip's text size, in points.
const ATTRIBUTION_SIZE: f64 = 10.0;

fn fail(shared: &Rc<Shared>, what: &str, error: &Error) {
    log::warn!("day-piece-map: {what}: {error}");
    shared
        .view
        .status
        .set(MapStatus::Failed(format!("{what}: {error}")));
    shared.repaint.notify();
}

/// Fetch and compile the style, open its sources and load its sprite.
fn load_style(shared: Rc<Shared>, style: String, language: Option<String>) {
    let generation = shared.generation.get();
    day_core::task(async move {
        let client = shared.state.borrow().client.clone();
        let text = if style.trim_start().starts_with('{') {
            style
        } else {
            match client
                .fetch_future(day_part_http::Request::get(&style))
                .await
            {
                Ok(r) if (200..300).contains(&r.status) => r.text().into_owned(),
                Ok(r) => {
                    fail(
                        &shared,
                        "style",
                        &Error::Status {
                            url: style.clone(),
                            status: r.status,
                        },
                    );
                    return;
                }
                Err(e) => {
                    fail(&shared, "style", &Error::Http(e.to_string()));
                    return;
                }
            }
        };
        if !shared.current(generation) {
            return;
        }
        let parsed = match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(mut json) => {
                if let Some(language) = language.as_deref() {
                    crate::style::localize(&mut json, language);
                }
                match Style::parse(&json) {
                    Ok(s) => Arc::new(s),
                    Err(e) => {
                        fail(&shared, "style", &e);
                        return;
                    }
                }
            }
            Err(e) => {
                fail(
                    &shared,
                    "style",
                    &Error::Invalid(format!("style JSON: {e}")),
                );
                return;
            }
        };
        shared.state.borrow_mut().style = Some(parsed.clone());
        shared.repaint.notify();
        // Sources, each as its own task so a slow TileJSON does not hold the others.
        let specs: Vec<(String, SourceSpec)> = parsed
            .sources
            .iter()
            .filter(|(_, s)| matches!(s, SourceSpec::Vector { .. }))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if specs.is_empty() {
            shared.view.status.set(MapStatus::Ready);
        }
        let remaining = Rc::new(Cell::new(specs.len()));
        for (name, spec) in specs {
            let shared = shared.clone();
            let client = client.clone();
            let remaining = remaining.clone();
            day_core::task(async move {
                match source::open(client, &spec).await {
                    Ok(source) => {
                        if !shared.current(generation) {
                            return;
                        }
                        if let Some(credit) = source.attribution().map(|c| plain_text(&c)) {
                            let mut lines = shared.view.attribution.get_untracked();
                            if !lines.contains(&credit) {
                                if !lines.is_empty() {
                                    lines.push_str(" · ");
                                }
                                lines.push_str(&credit);
                                shared.view.attribution.set(lines);
                            }
                        }
                        shared.state.borrow_mut().sources.insert(name, source);
                        remaining.set(remaining.get().saturating_sub(1));
                        if remaining.get() == 0
                            && shared.view.status.get_untracked() == MapStatus::Loading
                        {
                            shared.view.status.set(MapStatus::Ready);
                        }
                        shared.repaint.notify();
                    }
                    Err(e) => {
                        if shared.current(generation) {
                            fail(&shared, &format!("source {name}"), &e);
                        }
                    }
                }
            });
        }
        if let Some(base) = parsed.sprite.clone() {
            load_sprite(shared.clone(), client, base, generation);
        }
    });
}

/// The sprite: its index, then its sheet decoded by the toolkit. The `@2x` sheet is taken
/// everywhere; it is crisp on a dense screen and scaled on a plain one.
fn load_sprite(shared: Rc<Shared>, client: day_part_http::Client, base: String, generation: u64) {
    day_core::task(async move {
        let index_url = format!("{base}@2x.json");
        let index = match client
            .fetch_future(day_part_http::Request::get(&index_url))
            .await
        {
            Ok(r) if (200..300).contains(&r.status) => {
                match serde_json::from_slice::<serde_json::Value>(&r.body) {
                    Ok(json) => Sprite::parse_index(&json),
                    Err(e) => Err(Error::Invalid(format!("sprite index: {e}"))),
                }
            }
            Ok(r) => Err(Error::Status {
                url: index_url.clone(),
                status: r.status,
            }),
            Err(e) => Err(Error::Http(e.to_string())),
        };
        let images = match index {
            Ok(images) => images,
            Err(e) => {
                log::warn!("day-piece-map: sprite: {e}");
                return;
            }
        };
        if !shared.current(generation) {
            return;
        }
        // The index alone lets icons reserve their space; the sheet lets them draw.
        shared.state.borrow_mut().sprite = Some(Rc::new(Sprite {
            images: images.clone(),
            sheet: None,
        }));
        shared.repaint.notify();
        let sheet_url = format!("{base}@2x.png");
        let bytes = match client
            .fetch_future(day_part_http::Request::get(&sheet_url))
            .await
        {
            Ok(r) if (200..300).contains(&r.status) => r.body,
            Ok(r) => {
                log::warn!(
                    "day-piece-map: sprite sheet: {sheet_url} answered {}",
                    r.status
                );
                return;
            }
            Err(e) => {
                log::warn!("day-piece-map: sprite sheet: {e}");
                return;
            }
        };
        match day_core::image::decode(Arc::new(bytes)).await {
            Ok(sheet) => {
                if !shared.current(generation) {
                    return;
                }
                let mut st = shared.state.borrow_mut();
                st.sprite = Some(Rc::new(Sprite {
                    images,
                    sheet: Some(sheet),
                }));
                st.tiles_version += 1;
                drop(st);
                shared.repaint.notify();
            }
            Err(e) => log::warn!("day-piece-map: sprite sheet: {e}"),
        }
    });
}

fn fetch_tile(shared: Rc<Shared>, name: String, id: TileId) {
    let generation = shared.generation.get();
    day_core::task(async move {
        let source = shared.state.borrow().sources.get(&name).cloned();
        let Some(source) = source else { return };
        let started = Stopwatch::start();
        let answer = source.fetch(id).await;
        if !shared.current(generation) {
            return;
        }
        let fetched = started.elapsed();
        let slot = match answer {
            Ok(Some(bytes)) => {
                // Decode and filter off the UI thread: a city tile is a hundred milliseconds of
                // both on a phone, and the frame must not wait for it.
                let style = shared.state.borrow().style.clone();
                let source_name = name.clone();
                let size = bytes.len();
                let decoded = day_async::spawn_blocking(move || {
                    mvt::decode(&bytes).map(|tile| {
                        let data = TileData::new(id, tile);
                        if let Some(style) = style {
                            data.prepare(&style, &source_name);
                        }
                        data
                    })
                })
                .await;
                if !shared.current(generation) {
                    return;
                }
                match decoded {
                    Ok(Ok(data)) => {
                        log::debug!(
                            "day-piece-map: tile {name} {id:?}: {size} bytes, fetched in {:.0} ms, decoded and bucketed in {:.1} ms {}",
                            fetched.as_secs_f64() * 1000.0,
                            (started.elapsed() - fetched).as_secs_f64() * 1000.0,
                            if cfg!(target_arch = "wasm32") {
                                "inline (the web has no worker threads)"
                            } else {
                                "off the UI thread"
                            }
                        );
                        Slot::Loaded(Arc::new(data))
                    }
                    Ok(Err(e)) => {
                        log::warn!("day-piece-map: tile {name} {id:?}: {e}");
                        Slot::Missing
                    }
                    Err(e) => {
                        log::warn!("day-piece-map: tile {name} {id:?}: {e}");
                        Slot::Missing
                    }
                }
            }
            Ok(None) => Slot::Missing,
            Err(e) => {
                log::warn!("day-piece-map: tile {name} {id:?}: {e}");
                Slot::Missing
            }
        };
        let mut st = shared.state.borrow_mut();
        if let Some(entry) = st.tiles.get_mut(&(name, id)) {
            *entry = slot;
        }
        st.tiles_version += 1;
        drop(st);
        let pending = shared.view.pending.get_untracked().saturating_sub(1);
        shared.view.pending.set(pending);
        shared.repaint.notify();
    });
}

/// How much geometry a recording carries: ops, fills, strokes, path segments and image draws,
/// for the raster log (the painters' cost follows segments and images, not pixels).
fn geometry_counts(ops: &[DrawOp]) -> (usize, usize, usize, usize, usize) {
    let segs = |shape: &Shape| match shape {
        Shape::Path(p) => p.segs.len(),
        Shape::Polygon(pts) => pts.len(),
        _ => 1,
    };
    let (mut fills, mut strokes, mut segments, mut images) = (0, 0, 0, 0);
    for op in ops {
        match op {
            DrawOp::Fill(shape, _) => {
                fills += 1;
                segments += segs(shape);
            }
            DrawOp::Stroke(shape, _, _) => {
                strokes += 1;
                segments += segs(shape);
            }
            DrawOp::Image { .. } => images += 1,
            _ => {}
        }
    }
    (ops.len(), fills, strokes, segments, images)
}

/// A monotonic stopwatch on every target, the web included (`day_core::frame::now_secs`).
struct Stopwatch(f64);

impl Stopwatch {
    fn start() -> Stopwatch {
        Stopwatch(day_core::frame::now_secs())
    }

    fn elapsed(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64((day_core::frame::now_secs() - self.0).max(0.0))
    }
}

/// A credit line as text: tile services write their attribution as HTML links, and a label
/// shows text. Tags go, the common entities are decoded, runs of spaces collapse.
pub fn plain_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    let decoded = out
        .replace("&copy;", "©")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    #[test]
    fn attribution_html_becomes_text() {
        let html = r#"<a href="https://openfreemap.org" target="_blank">OpenFreeMap</a> <a href="https://www.openmaptiles.org/" target="_blank">&copy; OpenMapTiles</a> Data from <a href="https://www.openstreetmap.org/copyright" target="_blank">OpenStreetMap</a>"#;
        assert_eq!(
            super::plain_text(html),
            "OpenFreeMap © OpenMapTiles Data from OpenStreetMap"
        );
        assert_eq!(super::plain_text("plain &amp; simple"), "plain & simple");
    }
}
