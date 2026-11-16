// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! Google maps using a native SDK view and the shared `day-piece-map` camera and annotations.
//! Native sources and system/package dependencies are contributed through Day package metadata.
//! Provider options stay in this crate; unsupported targets report `Support::Unsupported`.

use day_core::{BuildCx, Piece, RNode};
use day_piece_map::MapView;
use day_pieces::{Decorated, IntoReactive, Reactive};
use serde_json::json;

/// Day renderer registration key.
pub const KIND: &str = "day.piece.map.google";

/// Whether this build contains the native renderer. This does not check network or credentials.
pub const fn support() -> day_spec::Support {
    if cfg!(all(feature = "material", target_os = "android")) {
        day_spec::Support::Native
    } else {
        day_spec::Support::Unsupported
    }
}

/// The provider's basemap family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapType {
    Normal,
    Satellite,
    Terrain,
    Hybrid,
    None,
}

/// Options belonging to the Google SDK. Changes to a bound options value patch the mounted map.
#[derive(Clone, Debug, PartialEq)]
pub struct GoogleOptions {
    /// Basemap imagery family.
    pub map_type: MapType,
    /// Display traffic where the service supplies it.
    pub traffic: bool,
    /// Display three-dimensional buildings where available.
    pub buildings: bool,
    /// Show the SDK compass control.
    pub compass: bool,
    /// Camera tilt in degrees. The SDK clamps this to the current camera's supported range.
    pub pitch: f64,
    /// Enable indoor maps in supported buildings.
    pub indoor: bool,
    /// Show the SDK zoom buttons.
    pub zoom_controls: bool,
    /// Google map styling JSON; errors are reported through the shared status.
    pub style_json: Option<String>,
}
impl Default for GoogleOptions {
    fn default() -> Self {
        Self {
            map_type: MapType::Normal,
            traffic: false,
            buildings: true,
            compass: true,
            pitch: 0.0,
            indoor: true,
            zoom_controls: false,
            style_json: None,
        }
    }
}

/// A native map leaf. Share its view with other providers to preserve camera and annotations.
#[must_use]
pub struct GoogleMap {
    view: MapView,
    options: Reactive<GoogleOptions>,
}

/// Construct a native Google map with default options.
///
/// Mount a shared view in only one provider at a time. On unsupported targets
/// the leaf reports `MapStatus::Failed` with `unsupported-platform`.
pub fn map(view: MapView) -> GoogleMap {
    GoogleMap {
        view,
        options: Reactive::Const(GoogleOptions::default()),
    }
}

#[day_pieces::builder(GoogleMapBuilder)]
impl GoogleMap {
    /// Set constant or reactive provider options without remounting the map.
    pub fn options<M>(mut self, options: impl IntoReactive<GoogleOptions, M>) -> Self {
        self.options = options.into_reactive();
        self
    }
}
impl Piece for GoogleMap {
    fn build(self, cx: &mut BuildCx) -> RNode {
        day_piece_map::__native::build(
            cx,
            KIND,
            self.view,
            move || {
                let o = self.options.get();
                json!({"mapType":format!("{:?}",o.map_type), "traffic":o.traffic,
                "buildings":o.buildings, "compass":o.compass,
                "pitch": if o.pitch.is_finite() { o.pitch.clamp(0.0, 80.0) } else { 0.0 },
                "indoor":o.indoor, "zoomControls":o.zoom_controls, "style":o.style_json,})
            },
            support() == day_spec::Support::Native,
        )
    }
}

#[cfg(all(feature = "material", target_os = "android"))]
mod android;
