// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! Private implementation shared by the Apple and Google adapters, enabled only by them.
//! Rust patches are typed and independent. JSON is only the FFI wire encoding, not an app API.

use crate::{
    CameraRequest, CameraTarget, Interaction, LngLat, MapStatus, MapTap, MapView, Marker, MarkerId,
    Overlay, clamp_zoom, wrap_bearing,
};
use day_core::{BuildCx, Flex, RNode, with_tree};
use day_geometry::Point;
use day_reactive::bind_seeded;
use day_spec::Event;
use serde_json::{Value, json};
use std::{cell::Cell, rc::Rc};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    center: LngLat,
    zoom: f64,
    bearing: f64,
}
impl Camera {
    fn read(view: MapView) -> Self {
        Self {
            center: view.center.get().wrapped(),
            zoom: clamp_zoom(view.zoom.get()),
            bearing: wrap_bearing(view.bearing.get()),
        }
    }
    fn json(self) -> Value {
        json!({"center": position(self.center), "zoom": self.zoom, "bearing": self.bearing})
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct Props {
    camera: Camera,
    markers: Vec<(MarkerId, Marker)>,
    overlays: Vec<Overlay>,
    interaction: Interaction,
    options: Value,
}
impl Props {
    fn read(view: MapView, options: Value) -> Self {
        Self {
            camera: Camera::read(view),
            markers: view.markers.get(),
            overlays: view.overlays.get(),
            interaction: view.interaction.get(),
            options,
        }
    }
    pub fn json(&self) -> String {
        json!({"camera": self.camera.json(), "markers": markers(&self.markers), "overlays": overlays(&self.overlays),
            "interaction": interaction(self.interaction), "options": self.options}).to_string()
    }
}
#[derive(Clone, Debug, PartialEq)]
pub enum Patch {
    Camera(Camera),
    Markers(Vec<(MarkerId, Marker)>),
    Overlays(Vec<Overlay>),
    Interaction(Interaction),
    Options(Value),
    Request(CameraRequest),
}
impl Patch {
    pub fn json(&self) -> String {
        match self {
            Self::Camera(camera) => json!({"camera": camera.json()}),
            Self::Markers(value) => json!({"markers": markers(value)}),
            Self::Overlays(value) => json!({"overlays": overlays(value)}),
            Self::Interaction(value) => json!({"interaction": interaction(*value)}),
            Self::Options(value) => json!({"options": value}),
            Self::Request(request) => json!({"request": command(*request)}),
        }
        .to_string()
    }
}
fn position(p: LngLat) -> Value {
    let p = p.wrapped();
    json!([p.lng, p.lat])
}
fn color(c: day_geometry::Color) -> Value {
    json!([c.r, c.g, c.b, c.a])
}
fn interaction(value: Interaction) -> Value {
    json!({"pan": value.pan, "zoomGesture": value.zoom, "rotate": value.rotate})
}
fn command(request: CameraRequest) -> Value {
    let mut value = match request.target {
        CameraTarget::View {
            center,
            zoom,
            bearing,
        } => {
            json!({"kind":"view", "center":position(center), "zoom":clamp_zoom(zoom), "bearing":bearing.map(wrap_bearing)})
        }
        CameraTarget::Bounds { sw, ne, padding } => {
            json!({"kind":"bounds", "sw":position(sw), "ne":position(ne), "padding":padding})
        }
    };
    value["serial"] = json!(request.serial.to_string());
    value["duration"] = json!(request.duration);
    value
}
fn markers(entries: &[(MarkerId, Marker)]) -> Vec<Value> {
    entries
        .iter()
        .map(|(id, m)| {
            json!({"id":id.value().to_string(), "position":position(m.position),
        "title":m.title, "subtitle":m.subtitle, "color":color(m.color)})
        })
        .collect()
}
fn overlays(entries: &[Overlay]) -> Vec<Value> {
    entries.iter().map(|o| match o {
        Overlay::Polyline { points, color: c, width } => json!({"kind":"line", "points":points.iter().copied().map(position).collect::<Vec<_>>(), "stroke":color(*c), "width":width.max(0.0)}),
        Overlay::Polygon { points, fill, stroke, width } => json!({"kind":"polygon", "points":points.iter().copied().map(position).collect::<Vec<_>>(), "fill":color(*fill), "stroke":color(*stroke), "width":width.max(0.0)}),
        Overlay::Circle { center, radius, fill, stroke, width } => json!({"kind":"circle", "center":position(*center), "radius":radius.max(0.0), "fill":color(*fill), "stroke":color(*stroke), "width":width.max(0.0)}),
    }).collect()
}

pub fn build(
    cx: &mut BuildCx,
    kind: &'static str,
    view: MapView,
    options: impl Fn() -> Value + 'static,
    supported: bool,
) -> RNode {
    view.tap.set(None);
    view.attribution.set(String::new());
    view.status.set(if supported {
        MapStatus::Loading
    } else {
        MapStatus::Failed("unsupported-platform".into())
    });
    let initial = Props::read(view, options());
    let reported = Rc::new(Cell::new(initial.camera));
    let node = cx.leaf(
        kind,
        &initial,
        Flex {
            grow_w: true,
            grow_h: true,
            ..Default::default()
        },
    );
    let echo = reported.clone();
    cx.on(node, move |event| {
        if let Event::Custom { text, .. } = event {
            receive(view, text, &echo);
        }
    });
    bind(
        view,
        options,
        initial,
        reported,
        Rc::new(move |patch| {
            with_tree(|t| t.patch(node, Box::new(patch), false));
        }),
    );
    node
}

fn bind(
    view: MapView,
    options: impl Fn() -> Value + 'static,
    initial: Props,
    reported: Rc<Cell<Camera>>,
    send: Rc<dyn Fn(Patch)>,
) {
    let emit = send.clone();
    bind_seeded(
        initial.camera,
        move || Camera::read(view),
        move |camera: &Camera| {
            // Record native reports before publishing signals. Deferred bindings can then distinguish
            // an app edit from the latest report without comparing against a moving SDK camera.
            if *camera != reported.get() {
                reported.set(*camera);
                emit(Patch::Camera(*camera));
            }
        },
    );
    let emit = send.clone();
    bind_seeded(
        initial.markers,
        move || view.markers.get(),
        move |value: &Vec<(MarkerId, Marker)>| emit(Patch::Markers(value.clone())),
    );
    let emit = send.clone();
    bind_seeded(
        initial.overlays,
        move || view.overlays.get(),
        move |value: &Vec<Overlay>| emit(Patch::Overlays(value.clone())),
    );
    let emit = send.clone();
    bind_seeded(
        initial.interaction,
        move || view.interaction.get(),
        move |value: &Interaction| emit(Patch::Interaction(*value)),
    );
    let emit = send.clone();
    bind_seeded(initial.options, options, move |value: &Value| {
        emit(Patch::Options(value.clone()));
    });
    // A remount starts at the current command; only later commands can run.
    bind_seeded(
        view.request.get_untracked(),
        move || view.request.get(),
        move |value: &Option<CameraRequest>| {
            if let Some(request) = value {
                send(Patch::Request(*request));
            }
        },
    );
}

fn receive(view: MapView, text: &str, reported: &Cell<Camera>) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return;
    };
    match value["event"].as_str() {
        Some("camera") => {
            let (Some(lng), Some(lat), Some(zoom), Some(bearing)) = (
                value["lng"].as_f64(),
                value["lat"].as_f64(),
                value["zoom"].as_f64(),
                value["bearing"].as_f64(),
            ) else {
                return;
            };
            if ![lng, lat, zoom, bearing].iter().all(|v| v.is_finite()) {
                return;
            }
            let camera = Camera {
                center: LngLat::new(lng, lat).wrapped(),
                zoom: clamp_zoom(zoom),
                bearing: wrap_bearing(bearing),
            };
            reported.set(camera);
            day_reactive::batch(|| {
                view.center.set(camera.center);
                view.zoom.set(camera.zoom);
                view.bearing.set(camera.bearing);
            });
        }
        Some("tap") => {
            let (Some(lng), Some(lat)) = (value["lng"].as_f64(), value["lat"].as_f64()) else {
                return;
            };
            let marker = value["marker"]
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .and_then(|id| {
                    view.markers
                        .get_untracked()
                        .iter()
                        .find(|(m, _)| m.value() == id)
                        .map(|(m, _)| *m)
                });
            view.tap.set(Some(MapTap {
                location: LngLat::new(lng, lat).wrapped(),
                point: Point::new(
                    value["x"].as_f64().unwrap_or(0.0),
                    value["y"].as_f64().unwrap_or(0.0),
                ),
                marker,
            }));
        }
        Some("ready") => view.status.set(MapStatus::Ready),
        Some("error") => view.status.set(MapStatus::Failed(
            value["code"].as_str().unwrap_or("native-map-error").into(),
        )),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use day_reactive::{Signal, flush_sync};
    use std::cell::RefCell;

    #[test]
    fn patches_are_independent_and_camera_reports_do_not_echo() {
        let view = MapView::new(LngLat::new(0.0, 0.0), 3.0);
        view.fly_to(LngLat::new(1.0, 2.0), 4.0);
        let options = Signal::new(json!({"traffic": false}));
        let initial = Props::read(view, options.get_untracked());
        let reported = Rc::new(Cell::new(initial.camera));
        let patches = Rc::new(RefCell::new(Vec::new()));
        let output = patches.clone();
        bind(
            view,
            move || options.get(),
            initial,
            reported.clone(),
            Rc::new(move |p| output.borrow_mut().push(p)),
        );
        flush_sync();
        assert!(
            patches.borrow().is_empty(),
            "remount must not replay a flight"
        );
        for lng in [10, 11] {
            receive(
                view,
                &format!(r#"{{"event":"camera","lng":{lng},"lat":20,"zoom":9,"bearing":270}}"#),
                &reported,
            );
        }
        flush_sync();
        assert!(
            patches.borrow().is_empty(),
            "SDK camera updates must never become commands"
        );
        view.zoom.set(10.0);
        flush_sync();
        let patch = patches.borrow_mut().pop().unwrap();
        assert!(matches!(
            patch,
            Patch::Camera(Camera {
                center: LngLat { lng: 11.0, .. },
                ..
            })
        ));
        assert_eq!(
            serde_json::from_str::<Value>(&patch.json())
                .unwrap()
                .as_object()
                .unwrap()
                .len(),
            1
        );
        view.add_marker(Marker::new(LngLat::new(2.0, 3.0)).subtitle("fixture subtitle"));
        flush_sync();
        assert!(matches!(patches.borrow().as_slice(), [Patch::Markers(_)]));
        patches.borrow_mut().clear();
        options.set(json!({"traffic": true}));
        flush_sync();
        assert!(matches!(patches.borrow().as_slice(), [Patch::Options(_)]));
        patches.borrow_mut().clear();
        view.fly_to(LngLat::new(1.0, 2.0), 4.0);
        flush_sync();
        assert!(matches!(patches.borrow().as_slice(), [Patch::Request(_)]));
    }
    #[test]
    fn malformed_events_and_stale_marker_ids_are_ignored() {
        let view = MapView::new(LngLat::new(0.0, 0.0), 3.0);
        let reported = Cell::new(Camera::read(view));
        receive(
            view,
            r#"{"event":"tap","lng":10,"lat":20,"marker":"9999"}"#,
            &reported,
        );
        assert!(view.tap.get_untracked().unwrap().marker.is_none());
        receive(
            view,
            r#"{"event":"error","code":"missing-api-key"}"#,
            &reported,
        );
        receive(view, "invalid fixture JSON", &reported);
        assert_eq!(
            view.status.get_untracked(),
            MapStatus::Failed("missing-api-key".into())
        );
    }
}
