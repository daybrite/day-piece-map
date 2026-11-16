// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! Apple maps using a native SDK view and the shared `day-piece-map` camera and annotations.
//! Native sources and system/package dependencies are contributed through Day package metadata.
//! Provider options stay in this crate; unsupported targets report `Support::Unsupported`.

use day_core::{BuildCx, Piece, RNode};
use day_piece_map::MapView;
use day_pieces::{Decorated, IntoReactive, Reactive};
use serde_json::json;

/// Day renderer registration key.
pub const KIND: &str = "day.piece.map.apple";

/// Whether this build contains the native renderer. This does not check network or credentials.
pub const fn support() -> day_spec::Support {
    if cfg!(any(
        all(feature = "appkit", target_os = "macos"),
        all(feature = "uikit", target_os = "ios")
    )) {
        day_spec::Support::Native
    } else {
        day_spec::Support::Unsupported
    }
}

/// The provider's basemap family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapType {
    Standard,
    Satellite,
    Hybrid,
}

/// Options belonging to the Apple SDK. Changes to a bound options value patch the mounted map.
#[derive(Clone, Debug, PartialEq)]
pub struct AppleOptions {
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
    /// Display the built-in points of interest.
    pub points_of_interest: bool,
}
impl Default for AppleOptions {
    fn default() -> Self {
        Self {
            map_type: MapType::Standard,
            traffic: false,
            buildings: true,
            compass: true,
            pitch: 0.0,
            points_of_interest: true,
        }
    }
}

/// A native map leaf. Share its view with other providers to preserve camera and annotations.
#[must_use]
pub struct AppleMap {
    view: MapView,
    options: Reactive<AppleOptions>,
}

/// Construct a native Apple map with default options.
///
/// Mount a shared view in only one provider at a time. On unsupported targets
/// the leaf reports `MapStatus::Failed` with `unsupported-platform`.
pub fn map(view: MapView) -> AppleMap {
    AppleMap {
        view,
        options: Reactive::Const(AppleOptions::default()),
    }
}

#[day_pieces::builder(AppleMapBuilder)]
impl AppleMap {
    /// Set constant or reactive provider options without remounting the map.
    pub fn options<M>(mut self, options: impl IntoReactive<AppleOptions, M>) -> Self {
        self.options = options.into_reactive();
        self
    }
}
impl Piece for AppleMap {
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
                "pointsOfInterest": o.points_of_interest,})
            },
            support() == day_spec::Support::Native,
        )
    }
}

#[cfg(any(
    all(feature = "appkit", target_os = "macos"),
    all(feature = "uikit", target_os = "ios")
))]
mod apple;
