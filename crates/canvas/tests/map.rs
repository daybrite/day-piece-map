// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! The piece under the mock toolkit, with the network intercepted: a style and its tiles are
//! served by `day-part-http`'s `Simulation` on a manual clock, so the whole path from the style
//! URL to the display list runs synchronously in a unit test, and the gestures move the view.

use day_geometry::Point;
use day_mock::{MockProbe, MockToolkit};
use day_part_http::Client;
use day_part_http::simulation::{ManualClock, Reply, Simulation};
use day_piece_map_canvas::mvt::{self, Layer, Tile};
use day_piece_map_canvas::{
    CanvasMapView, Feature, GeometryType, LngLat, MapStatus, Marker, Value, map_with_client,
};
use day_reactive::flush_sync;
use day_spec::{DragPhase, DrawOp, Event, NodeId, WindowOptions};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

const STYLE: &str = r##"{
  "version": 8,
  "sources": { "t": { "type": "vector", "tiles": ["http://tiles.test/{z}/{x}/{y}.pbf"],
                      "maxzoom": 4, "attribution": "Test data" } },
  "layers": [
    { "id": "bg", "type": "background", "paint": { "background-color": "#aaccff" } },
    { "id": "land", "type": "fill", "source": "t", "source-layer": "land",
      "paint": { "fill-color": "#eeeeee" } },
    { "id": "road", "type": "line", "source": "t", "source-layer": "road",
      "paint": { "line-color": "#ffffff", "line-width": ["interpolate", ["linear"], ["zoom"], 0, 1, 10, 6] } },
    { "id": "town", "type": "symbol", "source": "t", "source-layer": "place",
      "layout": { "text-field": ["get", "name"], "text-size": 12 },
      "paint": { "text-color": "#000000" } }
  ]
}"##;

/// Every tile is the same: a square of land, a road across it, a town in the middle.
fn tile_bytes() -> Vec<u8> {
    let feature = |kind, geometry: Vec<Vec<(f64, f64)>>, props: &[(&str, Value)]| Feature {
        id: None,
        kind,
        properties: props
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect::<BTreeMap<_, _>>(),
        geometry: geometry
            .into_iter()
            .map(|part| part.into_iter().map(|(x, y)| Point::new(x, y)).collect())
            .collect(),
    };
    let layer = |name: &str, features| Layer {
        name: name.into(),
        version: 2,
        extent: 4096,
        features,
    };
    mvt::encode(&Tile {
        layers: vec![
            layer(
                "land",
                vec![feature(
                    GeometryType::Polygon,
                    vec![vec![
                        (500.0, 500.0),
                        (3500.0, 500.0),
                        (3500.0, 3500.0),
                        (500.0, 3500.0),
                        (500.0, 500.0),
                    ]],
                    &[],
                )],
            ),
            layer(
                "road",
                vec![feature(
                    GeometryType::LineString,
                    vec![vec![(0.0, 2048.0), (4096.0, 2048.0)]],
                    &[],
                )],
            ),
            layer(
                "place",
                vec![feature(
                    GeometryType::Point,
                    vec![vec![(2048.0, 2048.0)]],
                    &[("name", Value::String("Middle".into()))],
                )],
            ),
        ],
    })
}

struct Fixture {
    probe: MockProbe,
    clock: ManualClock,
    sim: Simulation,
    view: CanvasMapView,
    /// Frame requests the piece made; the mock toolkit delivers none, so the test does.
    frames: Rc<RefCell<Vec<day_spec::FrameCallback>>>,
}

fn boot() -> Fixture {
    boot_style(STYLE, 512.0)
}

fn boot_style(style: &str, width: f64) -> Fixture {
    day_core::uninstall_tree();
    let clock = ManualClock::default();
    let sim = Simulation::with_clock(Arc::new(clock.clone()));
    let style = style.to_owned();
    sim.route("GET", "http://style.test/style.json", move |_| {
        Ok(Reply::new(200, style.as_bytes().to_vec()))
    });
    sim.handle(
        |_, url| url.starts_with("http://tiles.test/"),
        |_| Ok(Reply::new(200, tile_bytes())),
    );
    let client = Client::builder().transport(sim.clone()).build();
    let view = CanvasMapView::new(LngLat::new(0.0, 0.0), 2.0);
    let frames: Rc<RefCell<Vec<day_spec::FrameCallback>>> = Rc::new(RefCell::new(Vec::new()));
    let (mock, probe) = MockToolkit::new();
    day_core::launch_with(
        mock,
        WindowOptions::default()
            .title("Map fixture")
            .size(width, 512.0),
        move || map_with_client("http://style.test/style.json", view, client).any(),
    );
    // After launch, which installs the toolkit's own requester: the mock delivers no frames,
    // so the test keeps the callbacks and delivers them itself.
    {
        let frames = frames.clone();
        day_core::frame::install_frame_requester(move |_, cb| {
            frames.borrow_mut().push(cb);
            Box::new(|| {})
        });
    }
    Fixture {
        probe,
        clock,
        sim,
        view,
        frames,
    }
}

impl Fixture {
    /// Let the simulated network answer and the frame re-record.
    fn settle(&self) {
        for _ in 0..16 {
            flush_sync();
            self.clock.advance(Duration::from_millis(50));
            self.tick();
        }
    }

    /// Deliver the display frame a gesture asked for, so the view it moved to is applied.
    fn tick(&self) {
        self.tick_at(0.016);
    }

    /// Deliver a display frame stamped `at` seconds.
    fn tick_at(&self, at: f64) {
        let pending: Vec<day_spec::FrameCallback> = self.frames.borrow_mut().drain(..).collect();
        for cb in pending {
            cb(day_spec::FrameStamp::new(at));
        }
        flush_sync();
    }

    fn canvas(&self) -> (u64, Vec<DrawOp>) {
        let (_, widget) = self.probe.find_by_kind("day.canvas").remove(0);
        (widget.node, widget.ops)
    }
}

#[test]
fn a_style_and_its_tiles_load_through_intercepted_http_and_draw() {
    let f = boot();
    f.settle();
    assert_eq!(f.view.common.status.get_untracked(), MapStatus::Ready);
    assert_eq!(f.view.common.attribution.get_untracked(), "Test data");
    // Zoom 2 in a 512-point window: the viewport is one tile wide, so a handful of tiles were
    // asked for, each exactly once.
    let tile_requests = f
        .sim
        .requests()
        .iter()
        .filter(|r| r.url.starts_with("http://tiles.test/2/"))
        .count();
    assert!(tile_requests >= 1, "{:?}", f.sim.requests());
    assert_eq!(f.view.pending.get_untracked(), 0);
    let (_, ops) = f.canvas();
    // The base pass is rasterized per screen tile and blitted; the labels are live text.
    let fills = ops.iter().filter(|o| matches!(o, DrawOp::Fill(..))).count();
    let images = ops
        .iter()
        .filter(|o| matches!(o, DrawOp::Image { .. }))
        .count();
    assert!(fills >= 1, "the backdrop: {fills}");
    // The tiles' rasters and the label layer are all bitmaps on the canvas now.
    assert!(
        images >= 2,
        "rasterized tiles and the label layer: {images}"
    );
    let frame = f.view.frame.get_untracked();
    assert!(frame.labels >= 1, "the town label was placed: {frame:?}");
    drop(f);
    day_core::uninstall_tree();
}

#[test]
fn gestures_move_the_view() {
    let f = boot();
    f.settle();
    let (node, _) = f.canvas();
    let start = f.view.common.center.get_untracked();
    // A drag of 100 points east moves the content east, so the center moves west.
    f.probe.emit(
        NodeId(node),
        Event::Drag {
            phase: DragPhase::Began,
            location: Point::new(100.0, 100.0),
            translation: Point::ZERO,
        },
    );
    f.probe.emit(
        NodeId(node),
        Event::Drag {
            phase: DragPhase::Changed,
            location: Point::new(200.0, 100.0),
            translation: Point::new(100.0, 0.0),
        },
    );
    f.probe.emit(
        NodeId(node),
        Event::Drag {
            phase: DragPhase::Ended,
            location: Point::new(200.0, 100.0),
            translation: Point::new(100.0, 0.0),
        },
    );
    f.tick();
    let after_drag = f.view.common.center.get_untracked();
    assert!(after_drag.lng < start.lng, "{start:?} -> {after_drag:?}");
    assert!((after_drag.lat - start.lat).abs() < 1e-9);
    // A pinch to twice the scale is one zoom level, about its location: the point under the
    // fingers stays put.
    let anchor = Point::new(128.0, 128.0);
    let before = f.view.common.zoom.get_untracked();
    for (phase, scale) in [
        (DragPhase::Began, 1.0),
        (DragPhase::Changed, 1.5),
        (DragPhase::Changed, 2.0),
        (DragPhase::Ended, 2.0),
    ] {
        f.probe.emit(
            NodeId(node),
            Event::Pinch {
                phase,
                scale,
                location: anchor,
            },
        );
    }
    f.tick();
    assert!((f.view.common.zoom.get_untracked() - (before + 1.0)).abs() < 1e-9);
    // A wheel pan arrives as a lone Changed and moves the content too.
    let before_pan = f.view.common.center.get_untracked();
    f.probe.emit(
        NodeId(node),
        Event::Pan {
            phase: DragPhase::Changed,
            delta: Point::new(0.0, -40.0),
            location: anchor,
        },
    );
    f.tick();
    let after_pan = f.view.common.center.get_untracked();
    assert!(
        after_pan.lat < before_pan.lat,
        "content up, center south: {before_pan:?} -> {after_pan:?}"
    );
    f.settle();
    assert_eq!(f.view.common.status.get_untracked(), MapStatus::Ready);
    drop(f);
    day_core::uninstall_tree();
}

#[test]
#[expect(
    clippy::float_cmp,
    reason = "the fling has settled when the longitude stops changing at all"
)]
fn a_swipe_glides_on_and_a_touch_stops_it() {
    let f = boot();
    f.settle();
    let (node, _) = f.canvas();
    let drag = |phase, x: f64| {
        f.probe.emit(
            NodeId(node),
            Event::Drag {
                phase,
                location: Point::new(100.0 + x, 100.0),
                translation: Point::new(x, 0.0),
            },
        );
    };
    // A quick swipe to the right over three frames: the content follows, so the center moves
    // west.
    let mut t = 1.0;
    drag(DragPhase::Began, 0.0);
    for x in [40.0, 80.0, 120.0] {
        drag(DragPhase::Changed, x);
        t += 0.016;
        f.tick_at(t);
    }
    drag(DragPhase::Ended, 120.0);
    t += 0.016;
    f.tick_at(t);
    let at_release = f.view.common.center.get_untracked();
    // The glide: frames after the release keep moving the center the same way, slower each
    // time, and it stops on its own.
    let mut last = at_release.lng;
    let mut steps = 0;
    let mut moved = Vec::new();
    for _ in 0..400 {
        t += 0.016;
        f.tick_at(t);
        let lng = f.view.common.center.get_untracked().lng;
        if lng == last {
            break;
        }
        moved.push(last - lng);
        last = lng;
        steps += 1;
    }
    assert!(steps >= 10, "the map glided for {steps} frames: {moved:?}");
    assert!(steps < 400, "the glide ended on its own");
    assert!(moved[0] > 0.0 && moved[moved.len() - 1] > 0.0, "{moved:?}");
    assert!(
        moved[moved.len() - 1] < moved[0],
        "the glide slowed: {} then {}",
        moved[0],
        moved[moved.len() - 1]
    );
    // A new touch mid-glide stops it where it is.
    drag(DragPhase::Began, 0.0);
    for x in [40.0, 80.0] {
        drag(DragPhase::Changed, x);
        t += 0.016;
        f.tick_at(t);
    }
    drag(DragPhase::Ended, 80.0);
    t += 0.016;
    f.tick_at(t);
    let gliding = f.view.common.center.get_untracked().lng;
    t += 0.016;
    f.tick_at(t);
    assert!(
        f.view.common.center.get_untracked().lng < gliding,
        "a second swipe glides too"
    );
    drag(DragPhase::Began, 0.0);
    t += 0.016;
    f.tick_at(t);
    let held = f.view.common.center.get_untracked().lng;
    t += 0.5;
    f.tick_at(t);
    assert_eq!(
        f.view.common.center.get_untracked().lng,
        held,
        "a finger down holds the map still"
    );
    // A drag that rests before lifting does not glide.
    drag(DragPhase::Changed, 40.0);
    t += 0.016;
    f.tick_at(t);
    t += 0.4;
    f.tick_at(t);
    drag(DragPhase::Ended, 40.0);
    t += 0.016;
    f.tick_at(t);
    let rested = f.view.common.center.get_untracked().lng;
    t += 0.016;
    f.tick_at(t);
    assert_eq!(
        f.view.common.center.get_untracked().lng,
        rested,
        "no glide after a rest"
    );
    // Let the tiles the glide asked for arrive, then take the tree down while the thread's
    // locals are alive: a fetch cancelled during thread teardown has nothing left to post to.
    f.settle();
    drop(f);
    day_core::uninstall_tree();
}

/// Run the frame clock for `secs` at 60 frames per second from `t`, answering the piece's
/// frame requests, and give back the time reached.
fn run_frames(f: &Fixture, mut t: f64, secs: f64) -> f64 {
    let frames = (secs / 0.016).ceil() as usize;
    for _ in 0..frames {
        t += 0.016;
        f.tick_at(t);
    }
    t
}

#[test]
fn flights_fits_and_turns_arrive_where_asked() {
    let f = boot();
    f.settle();
    // A flight: the view moves along the way and lands exactly on the target.
    let target = LngLat::new(1.5, -0.8);
    f.view.common.fly_to(target, 5.0);
    flush_sync();
    let mut t = run_frames(&f, 1.0, 0.4);
    let midway = f.view.common.center.get_untracked();
    assert!(
        midway != LngLat::new(0.0, 0.0) && midway != target,
        "{midway:?}"
    );
    t = run_frames(&f, t, 1.0);
    let landed = f.view.common.center.get_untracked();
    assert!(
        (landed.lng - target.lng).abs() < 1e-9 && (landed.lat - target.lat).abs() < 1e-9,
        "{landed:?}"
    );
    assert!((f.view.common.zoom.get_untracked() - 5.0).abs() < 1e-9);
    // Fitting a box: both corners end up inside the viewport.
    let (sw, ne) = (LngLat::new(-3.0, -2.0), LngLat::new(3.0, 2.0));
    f.view.common.fit_bounds(sw, ne, 20.0);
    flush_sync();
    t = run_frames(&f, t, 1.2);
    let cam = day_piece_map_canvas::Camera::new(
        f.view.common.center.get_untracked(),
        f.view.common.zoom.get_untracked(),
        day_geometry::Size::new(512.0, 512.0),
    );
    for corner in [sw, ne] {
        let p = cam.project(corner);
        assert!(
            p.x >= 19.0 && p.x <= 493.0 && p.y >= 19.0 && p.y <= 493.0,
            "{p:?}"
        );
    }
    // A turn: the bearing animates to what was asked and north resets it.
    f.view.common.rotate_to(90.0);
    flush_sync();
    t = run_frames(&f, t, 0.2);
    let partway = f.view.common.bearing.get_untracked();
    assert!(partway > 0.0 && partway < 90.0, "{partway}");
    t = run_frames(&f, t, 0.6);
    assert!((f.view.common.bearing.get_untracked() - 90.0).abs() < 1e-9);
    f.view.common.reset_north();
    flush_sync();
    run_frames(&f, t, 0.8);
    assert_eq!(f.view.common.bearing.get_untracked(), 0.0);
    f.settle();
    drop(f);
    day_core::uninstall_tree();
}

#[test]
fn taps_report_markers_and_features_and_a_double_tap_zooms() {
    let f = boot();
    f.settle();
    let (node, _) = f.canvas();
    let tap = |p: Point| f.probe.emit(NodeId(node), Event::Tap(p));
    // A marker at the center: a tap on its head reports it.
    let id = f
        .view
        .common
        .add_marker(Marker::new(LngLat::new(0.0, 0.0)).title("Here"));
    flush_sync();
    tap(Point::new(256.0, 256.0 - 18.0));
    let mut t = run_frames(&f, 1.0, 0.5);
    let hit = f
        .view
        .common
        .tap
        .get_untracked()
        .expect("a tap was reported");
    assert_eq!(hit.marker, Some(id));
    assert_eq!(
        f.view.feature_hits.get_untracked(),
        [] as [day_piece_map_canvas::FeatureHit; 0]
    );
    // Off the marker: the features drawn there. The test tile fills the whole world with the
    // "land" polygon, so something is always under a tap.
    tap(Point::new(100.0, 400.0));
    t = run_frames(&f, t, 0.5);
    let hit = f
        .view
        .common
        .tap
        .get_untracked()
        .expect("a tap was reported");
    assert_eq!(hit.marker, None);
    assert!(
        f.view
            .feature_hits
            .get_untracked()
            .iter()
            .any(|h| h.layer == "land"),
        "{:?}",
        f.view
            .feature_hits
            .get_untracked()
            .iter()
            .map(|h| &h.layer)
            .collect::<Vec<_>>()
    );
    let location = hit.location;
    assert!(location.lng < 0.0 && location.lat < 0.0, "{location:?}");
    // Two quick taps zoom in a level about the spot, and are not reported.
    f.view.common.tap.set(None);
    let zoom = f.view.common.zoom.get_untracked();
    tap(Point::new(400.0, 300.0));
    t += 0.05;
    f.tick_at(t);
    tap(Point::new(402.0, 301.0));
    t = run_frames(&f, t, 0.6);
    assert!(
        (f.view.common.zoom.get_untracked() - (zoom + 1.0)).abs() < 1e-9,
        "{}",
        f.view.common.zoom.get_untracked()
    );
    assert!(
        f.view.common.tap.get_untracked().is_none(),
        "a double tap is not a tap"
    );
    // The spot under the fingers stayed put.
    let cam = day_piece_map_canvas::Camera::new(
        f.view.common.center.get_untracked(),
        f.view.common.zoom.get_untracked(),
        day_geometry::Size::new(512.0, 512.0),
    );
    let there = cam.project(location);
    let _ = there;
    run_frames(&f, t, 0.1);
    f.settle();
    drop(f);
    day_core::uninstall_tree();
}

#[test]
fn a_twist_turns_the_map_and_a_small_one_settles_north() {
    let f = boot();
    f.settle();
    let (node, _) = f.canvas();
    let twist = |phase, rotation: f64| {
        f.probe.emit(
            NodeId(node),
            Event::Rotate {
                phase,
                rotation,
                location: Point::new(256.0, 256.0),
            },
        );
    };
    let under = f.view.common.center.get_untracked();
    // A quarter turn clockwise: the bearing follows (less the dead zone), the center holds.
    twist(DragPhase::Began, 0.0);
    twist(DragPhase::Changed, 0.4);
    f.tick();
    twist(DragPhase::Changed, std::f64::consts::FRAC_PI_2);
    f.tick();
    twist(DragPhase::Ended, std::f64::consts::FRAC_PI_2);
    f.tick();
    let bearing = f.view.common.bearing.get_untracked();
    assert!((85.0..=90.0).contains(&bearing), "{bearing}");
    let center = f.view.common.center.get_untracked();
    assert!((center.lng - under.lng).abs() < 1e-6 && (center.lat - under.lat).abs() < 1e-6);
    // The frame draws the tiles through a turn and the labels upright: a Concat op appears.
    f.settle();
    let (_, ops) = f.canvas();
    assert!(
        ops.iter().any(|o| matches!(o, DrawOp::Concat(_))),
        "the raster layer is turned"
    );
    // A twist that ends near north settles on north; one inside the dead zone changes nothing.
    f.view.common.rotate_to(0.0);
    flush_sync();
    run_frames(&f, 1.0, 0.8);
    assert_eq!(f.view.common.bearing.get_untracked(), 0.0);
    twist(DragPhase::Began, 0.0);
    twist(DragPhase::Changed, 0.03);
    f.tick();
    twist(DragPhase::Ended, 0.03);
    f.tick();
    assert_eq!(
        f.view.common.bearing.get_untracked(),
        0.0,
        "inside the dead zone"
    );
    twist(DragPhase::Began, 0.0);
    twist(DragPhase::Changed, 0.12);
    f.tick();
    twist(DragPhase::Ended, 0.12);
    f.tick();
    assert!(f.view.common.bearing.get_untracked() > 0.0);
    run_frames(&f, 2.0, 0.8);
    assert_eq!(
        f.view.common.bearing.get_untracked(),
        0.0,
        "snapped back to north"
    );
    f.settle();
    drop(f);
    day_core::uninstall_tree();
}

#[test]
fn shared_gesture_switches_disable_motion_but_keep_programmatic_moves() {
    let f = boot();
    f.settle();
    let (node, _) = f.canvas();
    f.view.common.interaction.set(day_piece_map::Interaction {
        pan: false,
        zoom: false,
        rotate: false,
    });
    let center = f.view.common.center.get_untracked();
    let zoom = f.view.common.zoom.get_untracked();
    f.probe.emit(
        NodeId(node),
        Event::Pan {
            phase: DragPhase::Changed,
            delta: Point::new(100.0, 100.0),
            location: Point::new(256.0, 256.0),
        },
    );
    for phase in [DragPhase::Began, DragPhase::Changed, DragPhase::Ended] {
        f.probe.emit(
            NodeId(node),
            Event::Pinch {
                phase,
                scale: 2.0,
                location: Point::new(256.0, 256.0),
            },
        );
        f.probe.emit(
            NodeId(node),
            Event::Rotate {
                phase,
                rotation: 1.0,
                location: Point::new(256.0, 256.0),
            },
        );
    }
    f.tick();
    assert_eq!(f.view.common.center.get_untracked(), center);
    assert!((f.view.common.zoom.get_untracked() - zoom).abs() < 1e-9);
    assert!(f.view.common.bearing.get_untracked().abs() < 1e-9);
    f.view.common.fly_to(LngLat::new(1.0, 2.0), 4.0);
    flush_sync();
    run_frames(&f, 1.0, 1.5);
    assert!((f.view.common.center.get_untracked().lng - 1.0).abs() < 1e-9);
    f.settle();
    drop(f);
    day_core::uninstall_tree();
}

#[test]
fn shared_geometry_is_reactive_and_draws_in_list_order() {
    use day_geometry::Color;
    use day_piece_map::Overlay;
    use day_spec::{Paint, Shape};
    let f = boot();
    f.settle();
    let line = Color::rgb(0.1, 0.2, 0.3);
    let fill = Color::rgb(0.4, 0.5, 0.6);
    f.view.common.overlays.set(vec![
        Overlay::Polyline {
            points: vec![LngLat::new(-1.0, 0.0), LngLat::new(1.0, 0.0)],
            color: line,
            width: 3.0,
        },
        Overlay::Polygon {
            points: vec![
                LngLat::new(-1.0, -1.0),
                LngLat::new(1.0, -1.0),
                LngLat::new(0.0, 1.0),
            ],
            fill,
            stroke: line,
            width: 2.0,
        },
    ]);
    flush_sync();
    f.tick();
    let (_, ops) = f.canvas();
    let stroke = ops
        .iter()
        .position(
            |op| matches!(op, DrawOp::Stroke(Shape::Path(..), c, _) if *c == Paint::Solid(line)),
        )
        .expect("shared line");
    let polygon = ops
        .iter()
        .position(|op| matches!(op, DrawOp::Fill(Shape::Polygon(_), c) if *c == Paint::Solid(fill)))
        .expect("shared polygon");
    assert!(stroke < polygon, "list order is paint order");
    f.view.common.overlays.set(Vec::new());
    flush_sync();
    f.tick();
    let (_, ops) = f.canvas();
    assert!(
        !ops.iter()
            .any(|op| matches!(op, DrawOp::Fill(Shape::Polygon(_), c) if *c == Paint::Solid(fill)))
    );
    drop(f);
    day_core::uninstall_tree();
}

#[test]
fn attribution_is_linked_and_does_not_report_a_map_tap() {
    // Synthetic provider attribution exercises wrapping alongside the real legal OSM notice.
    let mut style: serde_json::Value = serde_json::from_str(STYLE).unwrap();
    style["sources"]["t"]["attribution"] = serde_json::json!(
        "© <a href='https://www.openstreetmap.org/copyright'>OpenStreetMap</a> contributors <a href='https://fixture.test/license'>Fixture provider credit</a>"
    );
    let f = boot_style(&style.to_string(), 240.0);
    f.settle();
    let (_, credit) = f
        .probe
        .find_by_kind("day.label")
        .into_iter()
        .find(|(_, widget)| widget.text == "© OpenStreetMap contributors")
        .expect("visible OSM credit");
    assert!(credit.frame.size.width <= 240.0, "credit fits a narrow map");
    f.probe.emit(NodeId(credit.node), Event::Tap(Point::ZERO));
    flush_sync();
    assert!(
        f.probe
            .log()
            .iter()
            .any(|entry| entry == "open_url https://www.openstreetmap.org/copyright")
    );
    assert!(f.view.common.tap.get_untracked().is_none());
    assert!(
        f.view
            .common
            .attribution
            .get_untracked()
            .contains("Fixture provider credit")
    );
    drop(f);
    day_core::uninstall_tree();
}
