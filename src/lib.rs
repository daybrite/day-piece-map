// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! day-piece-map: a vector map for Day apps, drawn on the canvas from a MapLibre style and
//! Mapbox Vector Tiles.
//!
//! An external Day Piece with **no native half**. Every frame is recorded as a `Draw` display
//! list and replayed by whichever backend the app runs on, so the map looks the same on every
//! target because it *is* the same. There is no per-toolkit map code and no WebView.
//!
//! The design follows [MapLibre GL JS](https://github.com/maplibre/maplibre-gl-js): a
//! [`style::Style`] is a MapLibre style document (sources, layers, filters, paint and layout
//! properties as [`expression`]s), a [`source::TileSource`] serves Mapbox Vector Tiles from a
//! z/x/y URL template or a PMTiles archive, [`mvt`] decodes them, the [`camera::Camera`] maps
//! between the globe and the screen, and [`render`] turns the visible tiles into canvas ops with
//! [`symbol`] placing the labels and icons so they do not collide. What MapLibre does with
//! WebGL shaders this crate does with the canvas's fills, strokes, text and images.
//!
//! The only entry point an app needs is [`map`]:
//!
//! ```ignore
//! use day::prelude::*;
//! use day_piece_map::{LngLat, MapView, map};
//!
//! fn page() -> impl Piece {
//!     let view = MapView::new(LngLat::new(-0.1276, 51.5072), 11.0);
//!     map("<https://tiles.openfreemap.org/styles/liberty",> view).grow(Dimension::Both)
//! }
//! ```

use std::collections::BTreeMap;

pub mod camera;
pub mod expression;
pub mod mvt;
pub mod render;
pub mod sample;
pub mod source;
pub mod style;
pub mod symbol;
mod view;

pub use camera::{Camera, LngLat};
pub use render::FeatureHit;
pub use source::TileId;
pub use view::{MapStatus, MapTap, MapView, Marker, MarkerId, map, map_with_client};

/// A value in a feature's properties or produced by an expression: the JSON value model of the
/// MapLibre style spec plus the types its expressions add (a color, a formatted string, an
/// image reference). Property values decoded from a tile are only `Null`, `Bool`, `Number` and
/// `String`.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Color(day_geometry::Color),
    Array(Vec<Value>),
    Object(BTreeMap<String, Value>),
    /// The result of a `format` expression: sections of text with their own size, font and
    /// color overrides, or an image. A plain string is one section with no overrides.
    Formatted(Vec<FormattedSection>),
    /// The result of an `image` expression: the name of a sprite image, and whether the sprite
    /// the context was given has it.
    Image {
        name: String,
        available: bool,
    },
    /// A `collator`, which compares strings case- and diacritic-sensitively or not.
    Collator {
        case_sensitive: bool,
        diacritic_sensitive: bool,
        locale: Option<String>,
    },
}

/// One run of a `Formatted` value (the `format` expression's sections).
#[derive(Clone, Debug, PartialEq)]
pub struct FormattedSection {
    pub text: String,
    /// `None` where the section is an image, named by `image`.
    pub image: Option<String>,
    pub scale: Option<f64>,
    pub font_stack: Option<Vec<String>>,
    pub text_color: Option<day_geometry::Color>,
    /// `"bottom"`, `"center"` or `"top"`, from the section's `vertical-align` option.
    pub vertical_align: Option<String>,
}

impl Value {
    /// The number this value is, if it is one.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }
    /// The string this value is, if it is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
    /// The boolean this value is, if it is one.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    /// The color this value is, if it is one.
    pub fn as_color(&self) -> Option<day_geometry::Color> {
        match self {
            Value::Color(c) => Some(*c),
            _ => None,
        }
    }
    /// A JSON value, as the style spec's `literal` and the fixtures express it.
    pub fn from_json(json: &serde_json::Value) -> Value {
        match json {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => Value::Number(n.as_f64().unwrap_or(f64::NAN)),
            serde_json::Value::String(s) => Value::String(s.clone()),
            serde_json::Value::Array(items) => {
                Value::Array(items.iter().map(Value::from_json).collect())
            }
            serde_json::Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), Value::from_json(v)))
                    .collect(),
            ),
        }
    }
}

/// The geometry kind of a feature, as the vector tile encodes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GeometryType {
    Unknown,
    Point,
    LineString,
    Polygon,
}

impl GeometryType {
    /// The name the `geometry-type` expression answers with.
    pub fn name(self) -> &'static str {
        match self {
            GeometryType::Unknown => "Unknown",
            GeometryType::Point => "Point",
            GeometryType::LineString => "LineString",
            GeometryType::Polygon => "Polygon",
        }
    }
}

/// One feature of a vector tile layer: its id, properties and geometry.
///
/// The geometry is in the tile's own integer coordinate space (`0..extent` across the tile,
/// y down), as a list of parts: each point of a multipoint, each line of a multiline, each
/// ring of a polygon (exterior rings wind clockwise in that y-down space and interior rings
/// counter-clockwise, which is what makes [`day_spec::FillRule::NonZero`] cut the holes).
#[derive(Clone, Debug, PartialEq)]
pub struct Feature {
    pub id: Option<Value>,
    pub kind: GeometryType,
    pub properties: BTreeMap<String, Value>,
    pub geometry: Vec<Vec<day_geometry::Point>>,
}

/// What can go wrong between a style URL and a drawn frame.
#[derive(Clone, Debug, PartialEq)]
pub enum Error {
    /// A style, TileJSON, expression or PMTiles document is malformed: where, and what.
    Invalid(String),
    /// A fetch failed, after the HTTP part's own retries; the message is the transport's.
    Http(String),
    /// The server answered with this status for this URL.
    Status { url: String, status: u16 },
    /// A source kind, layer type, expression or archive feature this crate does not implement.
    Unsupported(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Invalid(why) => write!(f, "invalid: {why}"),
            Error::Http(why) => write!(f, "request failed: {why}"),
            Error::Status { url, status } => write!(f, "{url} answered {status}"),
            Error::Unsupported(what) => write!(f, "unsupported: {what}"),
        }
    }
}

impl std::error::Error for Error {}
