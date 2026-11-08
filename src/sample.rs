// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! The built-in sample: an island drawn by hand, served as a MapLibre style and vector tiles
//! from inside the app through `day-part-http`'s interception, so a demo, a showcase page or
//! a test has a map without a network and draws the same one on every target.
//!
//! ```ignore
//! let sim = day_part_http::simulation::Simulation::new();
//! sim.set_strict(false); // everything else still reaches the network
//! crate::sample::routes(&sim);
//! day_part_http::Session::global().set_simulation(Some(sim));
//! let view = MapView::new(crate::sample::HOME, 9.0);
//! map(crate::sample::STYLE_URL, view)
//! ```
//!
//! The island is a handful of shapes in longitude and latitude, each named in English and in
//! French, so the language picker has something to show offline. Each tile request projects them
//! into that tile's coordinates and encodes the ones that touch it as a vector tile, the same
//! format a tile server sends, so the piece exercises its whole path: TileJSON-less inline
//! tiles, decoding, filters, zoom ramps, line and point labels, collision.

use crate::camera::LngLat;
use crate::mvt::{self, Layer, Tile};
use crate::{Feature, GeometryType, Value};
use day_geometry::Point;
use day_part_http::HttpError;
use day_part_http::simulation::{Reply, Simulation};

/// The origin every sample URL is under; only these are intercepted, so the OpenFreeMap
/// style keeps going out to the network.
const ORIGIN: &str = "http://sample.map.local";
/// The style document's URL.
pub const STYLE_URL: &str = "http://sample.map.local/style.json";
/// The middle of the island, where a view of it starts; zoom 9 shows the whole island.
pub const HOME: LngLat = LngLat::new(0.0, 0.0);
/// The island's capital, Port Meridian, a place with a point label, and a spot on its east
/// coast: two places a demo can put markers on and fly between.
pub const CAPITAL: LngLat = LngLat::new(-0.20, 0.0);
pub const EAST_COAST: LngLat = LngLat::new(1.18, 0.30);

/// The tile side in tile units (the vector tile convention).
const EXTENT: f64 = 4096.0;
/// How far past the tile edge a shape is still encoded, in tile units, so strokes and labels
/// at the edge are complete.
const BUFFER: f64 = 512.0;
/// The archive serves no tiles past this zoom; the piece overzooms from here.
const MAX_ZOOM: u32 = 14;

/// Put the sample's routes on `sim`: only the sample's origin is answered, so what the
/// simulation does with every other URL (pass it to the network, or refuse it) is the
/// caller's choice, and one simulation can answer for the sample and for a recording.
pub fn routes(sim: &Simulation) {
    sim.handle(
        |_, url| url.starts_with(ORIGIN),
        |request| {
            let path = request.url.trim_start_matches(ORIGIN);
            if path == "/style.json" {
                return Ok(Reply::new(200, STYLE.as_bytes().to_vec())
                    .header("content-type", "application/json"));
            }
            let parts: Vec<&str> = path
                .trim_start_matches('/')
                .trim_end_matches(".pbf")
                .split('/')
                .collect();
            if let [z, x, y] = parts[..]
                && let (Ok(z), Ok(x), Ok(y)) =
                    (z.parse::<u32>(), x.parse::<u32>(), y.parse::<u32>())
            {
                if z > MAX_ZOOM {
                    return Ok(Reply::new(404, Vec::new()));
                }
                return Ok(
                    Reply::new(200, tile(z, x, y)).header("content-type", "application/x-protobuf")
                );
            }
            Err(HttpError::Status(404))
        },
    );
}

/// The style: a basemap in miniature. Zoom ramps on the widths, a filter on the road class,
/// labels along the roads and on the towns with a halo, the capital first by sort key.
const STYLE: &str = r##"{
  "version": 8,
  "name": "Sample island",
  "sources": {
    "island": {
      "type": "vector",
      "tiles": ["http://sample.map.local/{z}/{x}/{y}.pbf"],
      "maxzoom": 14,
      "attribution": "Sample data, drawn by hand"
    }
  },
  "layers": [
    { "id": "sea", "type": "background", "paint": { "background-color": "#a9c9f0" } },
    { "id": "land", "type": "fill", "source": "island", "source-layer": "land",
      "paint": { "fill-color": "#f3efe6", "fill-outline-color": "#c9c3b4" } },
    { "id": "park", "type": "fill", "source": "island", "source-layer": "landuse",
      "filter": ["==", ["get", "class"], "park"],
      "paint": { "fill-color": "#cfe8b4", "fill-opacity": 0.9 } },
    { "id": "lake", "type": "fill", "source": "island", "source-layer": "water",
      "paint": { "fill-color": "#a9c9f0" } },
    { "id": "river", "type": "line", "source": "island", "source-layer": "waterway",
      "layout": { "line-cap": "round", "line-join": "round" },
      "paint": { "line-color": "#86b3e6",
                 "line-width": ["interpolate", ["exponential", 1.4], ["zoom"], 6, 1, 14, 6] } },
    { "id": "road-casing", "type": "line", "source": "island", "source-layer": "transportation",
      "filter": ["==", ["get", "class"], "primary"],
      "layout": { "line-cap": "round", "line-join": "round" },
      "paint": { "line-color": "#d9a066",
                 "line-width": ["interpolate", ["exponential", 1.5], ["zoom"], 6, 1.5, 14, 12] } },
    { "id": "road-minor", "type": "line", "source": "island", "source-layer": "transportation",
      "filter": ["==", ["get", "class"], "minor"],
      "minzoom": 8,
      "layout": { "line-cap": "round", "line-join": "round" },
      "paint": { "line-color": "#ffffff", "line-dasharray": [3, 1],
                 "line-width": ["interpolate", ["exponential", 1.5], ["zoom"], 8, 0.5, 14, 4] } },
    { "id": "road", "type": "line", "source": "island", "source-layer": "transportation",
      "filter": ["==", ["get", "class"], "primary"],
      "layout": { "line-cap": "round", "line-join": "round" },
      "paint": { "line-color": "#fbe3a0",
                 "line-width": ["interpolate", ["exponential", 1.5], ["zoom"], 6, 1, 14, 9] } },
    { "id": "road-name", "type": "symbol", "source": "island", "source-layer": "transportation",
      "minzoom": 9,
      "layout": { "symbol-placement": "line", "text-field": ["get", "name"], "text-size": 11,
                  "text-font": ["Noto Sans Regular"], "symbol-spacing": 300 },
      "paint": { "text-color": "#5a4a2a", "text-halo-color": "#ffffff", "text-halo-width": 1.5 } },
    { "id": "town-dot", "type": "circle", "source": "island", "source-layer": "place",
      "paint": { "circle-radius": ["match", ["get", "rank"], 1, 5, 3],
                 "circle-color": "#333333", "circle-stroke-color": "#ffffff", "circle-stroke-width": 1.5 } },
    { "id": "town-name", "type": "symbol", "source": "island", "source-layer": "place",
      "layout": { "text-field": ["get", "name"], "text-anchor": "top", "text-offset": [0, 0.6],
                  "text-size": ["match", ["get", "rank"], 1, 15, 12],
                  "text-font": ["match", ["get", "rank"], 1, ["literal", ["Noto Sans Bold"]], ["literal", ["Noto Sans Regular"]]],
                  "symbol-sort-key": ["get", "rank"], "text-max-width": 8 },
      "paint": { "text-color": "#222222", "text-halo-color": "#ffffff", "text-halo-width": 1.5 } }
  ]
}"##;

// ---------------------------------------------------------------------------
// The island
// ---------------------------------------------------------------------------

struct Shape {
    layer: &'static str,
    kind: GeometryType,
    /// Rings of a polygon (first exterior, then holes), the one line of a line, the one
    /// point of a point: longitude, latitude.
    parts: Vec<Vec<(f64, f64)>>,
    tags: Vec<(&'static str, Tag)>,
}

#[derive(Clone)]
enum Tag {
    Text(&'static str),
    Number(f64),
}

fn shapes() -> Vec<Shape> {
    let coast = vec![
        (-1.20, 0.40),
        (-0.90, 0.95),
        (-0.40, 1.20),
        (0.30, 1.25),
        (0.85, 1.00),
        (1.20, 0.55),
        (1.30, 0.00),
        (1.05, -0.55),
        (0.60, -0.95),
        (0.00, -1.10),
        (-0.55, -0.95),
        (-1.00, -0.50),
        (-1.25, -0.05),
        (-1.20, 0.40),
    ];
    let lake = vec![
        (0.30, 0.45),
        (0.55, 0.60),
        (0.75, 0.45),
        (0.60, 0.25),
        (0.35, 0.25),
        (0.30, 0.45),
    ];
    vec![
        Shape {
            layer: "land",
            kind: GeometryType::Polygon,
            parts: vec![coast, lake.clone()],
            tags: vec![("class", Tag::Text("land"))],
        },
        Shape {
            layer: "water",
            kind: GeometryType::Polygon,
            parts: vec![lake],
            tags: vec![("class", Tag::Text("lake"))],
        },
        Shape {
            layer: "landuse",
            kind: GeometryType::Polygon,
            parts: vec![vec![
                (-0.70, 0.10),
                (-0.45, 0.35),
                (-0.20, 0.15),
                (-0.40, -0.15),
                (-0.70, 0.10),
            ]],
            tags: vec![
                ("class", Tag::Text("park")),
                ("name", Tag::Text("Harbour Park")),
                ("name:fr", Tag::Text("Parc du Port")),
            ],
        },
        Shape {
            layer: "waterway",
            kind: GeometryType::LineString,
            parts: vec![vec![
                (0.45, 0.30),
                (0.35, 0.05),
                (0.20, -0.30),
                (0.25, -0.70),
                (0.10, -1.05),
            ]],
            tags: vec![
                ("class", Tag::Text("river")),
                ("name", Tag::Text("Long River")),
                ("name:fr", Tag::Text("Longue Rivière")),
            ],
        },
        Shape {
            layer: "transportation",
            kind: GeometryType::LineString,
            parts: vec![vec![
                (-1.05, -0.30),
                (-0.60, -0.20),
                (-0.20, 0.00),
                (0.20, -0.05),
                (0.60, -0.30),
                (1.00, -0.40),
            ]],
            tags: vec![
                ("class", Tag::Text("primary")),
                ("name", Tag::Text("Coast Road")),
                ("name:fr", Tag::Text("Route de la Côte")),
            ],
        },
        Shape {
            layer: "transportation",
            kind: GeometryType::LineString,
            parts: vec![vec![
                (-0.20, 0.00),
                (-0.25, 0.40),
                (-0.30, 0.80),
                (-0.35, 1.10),
            ]],
            tags: vec![
                ("class", Tag::Text("primary")),
                ("name", Tag::Text("North Street")),
                ("name:fr", Tag::Text("Rue du Nord")),
            ],
        },
        Shape {
            layer: "transportation",
            kind: GeometryType::LineString,
            parts: vec![vec![
                (0.20, -0.05),
                (0.15, -0.35),
                (0.00, -0.60),
                (-0.10, -0.90),
            ]],
            tags: vec![
                ("class", Tag::Text("minor")),
                ("name", Tag::Text("Mill Lane")),
                ("name:fr", Tag::Text("Chemin du Moulin")),
            ],
        },
        Shape {
            layer: "transportation",
            kind: GeometryType::LineString,
            parts: vec![vec![
                (0.60, -0.30),
                (0.80, 0.00),
                (0.95, 0.40),
                (0.80, 0.75),
            ]],
            tags: vec![
                ("class", Tag::Text("minor")),
                ("name", Tag::Text("Lake Road")),
                ("name:fr", Tag::Text("Route du Lac")),
            ],
        },
        Shape {
            layer: "place",
            kind: GeometryType::Point,
            parts: vec![vec![(-0.20, 0.00)]],
            tags: vec![
                ("name", Tag::Text("Port Meridian")),
                ("name:fr", Tag::Text("Port-Méridien")),
                ("rank", Tag::Number(1.0)),
            ],
        },
        Shape {
            layer: "place",
            kind: GeometryType::Point,
            parts: vec![vec![(0.60, -0.30)]],
            tags: vec![
                ("name", Tag::Text("Eastbridge")),
                ("name:fr", Tag::Text("Pont-de-l'Est")),
                ("rank", Tag::Number(2.0)),
            ],
        },
        Shape {
            layer: "place",
            kind: GeometryType::Point,
            parts: vec![vec![(-0.35, 1.10)]],
            tags: vec![
                ("name", Tag::Text("North Point")),
                ("name:fr", Tag::Text("Pointe-Nord")),
                ("rank", Tag::Number(2.0)),
            ],
        },
        Shape {
            layer: "place",
            kind: GeometryType::Point,
            parts: vec![vec![(-0.10, -0.90)]],
            tags: vec![
                ("name", Tag::Text("Millbrook")),
                ("name:fr", Tag::Text("Ruisseau-du-Moulin")),
                ("rank", Tag::Number(3.0)),
            ],
        },
        Shape {
            layer: "place",
            kind: GeometryType::Point,
            parts: vec![vec![(0.80, 0.75)]],
            tags: vec![
                ("name", Tag::Text("Lakeside")),
                ("name:fr", Tag::Text("Bord-du-Lac")),
                ("rank", Tag::Number(3.0)),
            ],
        },
    ]
}

/// Web Mercator, as a fraction of the world's side.
fn project(lng: f64, lat: f64) -> (f64, f64) {
    let x = (lng + 180.0) / 360.0;
    let lat = lat.to_radians();
    let y = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0;
    (x, y)
}

/// The tile `z/x/y`, encoded: every shape that touches it, in its coordinates.
pub fn tile(z: u32, x: u32, y: u32) -> Vec<u8> {
    let n = 2f64.powi(z as i32);
    let to_tile = |(lng, lat): (f64, f64)| {
        let (px, py) = project(lng, lat);
        Point::new(
            (px * n - f64::from(x)) * EXTENT,
            (py * n - f64::from(y)) * EXTENT,
        )
    };
    let mut layers: Vec<Layer> = Vec::new();
    for (index, shape) in shapes().into_iter().enumerate() {
        let geometry: Vec<Vec<Point>> = shape
            .parts
            .iter()
            .map(|part| part.iter().map(|&p| to_tile(p)).collect())
            .collect();
        let (x0, y0, x1, y1) = geometry.iter().flatten().fold(
            (f64::MAX, f64::MAX, f64::MIN, f64::MIN),
            |(x0, y0, x1, y1), p| (x0.min(p.x), y0.min(p.y), x1.max(p.x), y1.max(p.y)),
        );
        if x1 < -BUFFER || y1 < -BUFFER || x0 > EXTENT + BUFFER || y0 > EXTENT + BUFFER {
            continue;
        }
        let feature = Feature {
            id: Some(Value::Number(index as f64 + 1.0)),
            kind: shape.kind,
            properties: shape
                .tags
                .iter()
                .map(|(k, v)| {
                    let value = match v {
                        Tag::Text(t) => Value::String((*t).to_string()),
                        Tag::Number(n) => Value::Number(*n),
                    };
                    ((*k).to_string(), value)
                })
                .collect(),
            geometry,
        };
        match layers.iter_mut().find(|l| l.name == shape.layer) {
            Some(layer) => layer.features.push(feature),
            None => layers.push(Layer {
                name: shape.layer.to_string(),
                version: 2,
                extent: EXTENT as u32,
                features: vec![feature],
            }),
        }
    }
    mvt::encode(&Tile { layers })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_whole_island_is_in_the_zoom_zero_tile_and_decodes() {
        let bytes = tile(0, 0, 0);
        let decoded = crate::mvt::decode(&bytes).unwrap();
        let names: Vec<&str> = decoded.layers.iter().map(|l| l.name.as_str()).collect();
        assert!(
            names.contains(&"land")
                && names.contains(&"place")
                && names.contains(&"transportation")
        );
        let place = decoded.layer("place").unwrap();
        assert_eq!(place.features.len(), 5);
        assert_eq!(
            place.features[0].properties["name"],
            crate::Value::String("Port Meridian".into())
        );
        let land = decoded.layer("land").unwrap();
        assert_eq!(
            land.features[0].geometry.len(),
            2,
            "the coast and the lake hole"
        );
    }

    #[test]
    fn far_tiles_are_empty_and_nearby_ones_keep_what_touches_them() {
        // Zoom 14 at the antipode: nothing.
        assert!(
            crate::mvt::decode(&tile(14, 0, 0))
                .unwrap()
                .layers
                .is_empty()
        );
        // The tile over Port Meridian at zoom 10 has the town and the roads through it.
        let (px, py) = project(-0.20, 0.00);
        let n = 1024.0;
        let near = tile(10, (px * n) as u32, (py * n) as u32);
        let decoded = crate::mvt::decode(&near).unwrap();
        assert!(decoded.layer("place").is_some());
        assert!(decoded.layer("transportation").is_some());
        let style = crate::style::Style::parse_str(STYLE).unwrap();
        assert_eq!(style.layers.len(), 11);
    }
}
