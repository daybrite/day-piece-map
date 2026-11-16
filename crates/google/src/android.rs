// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0
use day_piece_map::__native::{Patch, Props};
use day_spec::NodeId;
use day_toolkit_material::jni::objects::JString;
use day_toolkit_material::{AHandle, Material, java};
day_toolkit_material::java_class! {
    DayGoogleMap => dev.daybrite.day.piece.maps.DayGoogleMap,
    methods {
        static fn create(node: jlong, json: JString) -> android.view.View,
        static fn update(view: android.view.View, json: JString),
        static fn dispose(view: android.view.View),
    },
}
fn make(_: &mut Material, props: &Props, id: NodeId) -> AHandle {
    day_toolkit_material::realize("google-map", |env| {
        let json = JString::from_str(env, props.json())?;
        DayGoogleMap::create(env, id.0 as i64, &json)
    })
}
fn update(_: &mut Material, view: &AHandle, patch: &Patch) {
    java(|env| {
        let json = JString::from_str(env, patch.json())?;
        DayGoogleMap::update(env, view.0.as_obj(), &json)
    });
}
fn release(_: &mut Material, view: &AHandle) {
    java(|env| DayGoogleMap::dispose(env, view.0.as_obj()));
}
day_spec::renderer!(day_toolkit_material::RENDERERS, Material,
    kind: super::KIND, props: Props, patch: Patch, make: make, update: update,
    measure: day_spec::fill_measure, release: release);
