// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0
use day_piece_map::__native::{Patch, Props};
use day_spec::{Event, NodeId};
use objc2::rc::Retained;
use std::ffi::{CStr, CString, c_char, c_void};
#[cfg(target_os = "macos")]
use {
    day_toolkit_appkit::{AppKit as Toolkit, emit},
    objc2_app_kit::NSView as View,
};
#[cfg(target_os = "ios")]
use {
    day_toolkit_uikit::{Uikit as Toolkit, emit},
    objc2_ui_kit::UIView as View,
};
unsafe extern "C" {
    fn day_apple_map_new(
        node: u64,
        json: *const c_char,
        callback: extern "C" fn(u64, *const c_char),
    ) -> *mut c_void;
    fn day_apple_map_update(view: *mut c_void, json: *const c_char);
    fn day_apple_map_release(view: *mut c_void);
}
extern "C" fn report(node: u64, json: *const c_char) {
    if json.is_null() {
        return;
    }
    // Swift lends a UTF-8 C string for this synchronous call.
    let text = unsafe { CStr::from_ptr(json) }
        .to_string_lossy()
        .into_owned();
    emit(NodeId(node), Event::custom("map", text));
}
fn make(_: &mut Toolkit, props: &Props, node: NodeId) -> Retained<View> {
    let json = CString::new(props.json()).expect("JSON escapes NUL");
    // Swift returns a retained MKMapView; Day owns that retain until toolkit release.
    let view = unsafe { day_apple_map_new(node.0, json.as_ptr(), report) };
    unsafe { Retained::from_raw(view.cast()) }.expect("MapKit map view")
}
fn update(_: &mut Toolkit, view: &Retained<View>, patch: &Patch) {
    let json = CString::new(patch.json()).expect("JSON escapes NUL");
    unsafe { day_apple_map_update(Retained::as_ptr(view).cast_mut().cast(), json.as_ptr()) };
}
fn release(_: &mut Toolkit, view: &Retained<View>) {
    unsafe { day_apple_map_release(Retained::as_ptr(view).cast_mut().cast()) };
}
#[cfg(target_os = "macos")]
day_spec::renderer!(day_toolkit_appkit::RENDERERS, Toolkit,
    kind: super::KIND, props: Props, patch: Patch, make: make, update: update,
    measure: day_spec::fill_measure, release: release);
#[cfg(target_os = "ios")]
day_spec::renderer!(day_toolkit_uikit::RENDERERS, Toolkit,
    kind: super::KIND, props: Props, patch: Patch, make: make, update: update,
    measure: day_spec::fill_measure, release: release);
