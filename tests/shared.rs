// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0
//! Synthetic coordinates and marker text exercise the provider-neutral contract.
use day_piece_map::{CameraTarget, LngLat, MapView, Marker};

#[test]
fn non_finite_coordinates_and_zoom_are_normalized() {
    let view = MapView::new(LngLat::new(f64::INFINITY, f64::NAN), f64::INFINITY);
    assert_eq!(view.center.get_untracked(), LngLat::new(0.0, 0.0));
    assert_eq!(view.zoom.get_untracked().to_bits(), 0.0_f64.to_bits());
    assert_eq!(
        LngLat::new(721.0, 90.0).wrapped(),
        LngLat::new(1.0, day_piece_map::MAX_LATITUDE)
    );
}
#[test]
fn markers_keep_identity_across_updates() {
    let view = MapView::new(LngLat::new(0.0, 0.0), 3.0);
    let id = view.add_marker(Marker::new(LngLat::new(1.0, 2.0)).title("fixture title"));
    assert!(view.update_marker(
        id,
        Marker::new(LngLat::new(2.0, 3.0)).subtitle("fixture subtitle")
    ));
    assert_eq!(view.marker(id).unwrap().subtitle, "fixture subtitle");
    view.remove_marker(id);
    assert!(!view.update_marker(id, Marker::new(LngLat::new(0.0, 0.0))));
    assert!(view.marker(id).is_none());
}
#[test]
fn bounds_preserve_an_antimeridian_crossing() {
    let view = MapView::new(LngLat::new(180.0, 0.0), 3.0);
    view.fit_bounds(LngLat::new(170.0, -10.0), LngLat::new(-170.0, 10.0), 24.0);
    let CameraTarget::Bounds { sw, ne, .. } = view.request.get_untracked().unwrap().target else {
        panic!("bounds command")
    };
    assert!(sw.lng > ne.lng);
}
