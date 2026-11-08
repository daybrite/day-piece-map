// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! OpenFreeMap's Liberty style, recorded: the style, its TileJSON, its 2x sprite and the vector
//! tiles around central London, served from `tests/data/openfreemap` through `day-part-http`'s
//! `Simulation` so the map renders the real style with no network.
//!
//! Shared by the piece's integration test (`tests/openfreemap.rs`) and the demo
//! (`demo/src/offline.rs`), which both reach this file with `#[path]`. It stays out of the
//! library so no app that depends on the piece carries the recording.
//!
//! The answers are the server's own bytes and headers. The tiles go out exactly as they came
//! over the wire, gzip with `content-encoding: gzip`: a native transport would inflate them
//! before the piece saw them, the simulation hands them over as they are, and the piece's tile
//! fetch inflates a gzip body itself, so both arrive at the same decoded tile.

use day_part_http::HttpError;
use day_part_http::simulation::{Reply, Simulation};

/// The Liberty style document's URL, as the demo's source picker names it.
pub const STYLE_URL: &str = "https://tiles.openfreemap.org/styles/liberty";
/// The TileJSON the style's `openmaptiles` source points at.
const TILEJSON_URL: &str = "https://tiles.openfreemap.org/planet";
/// The style's `sprite`; the piece appends `@2x.json` and `@2x.png`.
const SPRITE_URL: &str = "https://tiles.openfreemap.org/sprites/ofm_f384/ofm";
/// Everything before `{z}/{x}/{y}.pbf` in the TileJSON's tile template: the planet build the
/// recording was taken from.
const TILES_URL: &str = "https://tiles.openfreemap.org/planet/20261101_080013_pt/";

const JSON: &str = "application/json";

/// The recorded tiles by `z/x/y`: the 2×2 block at zoom 14 that a 1280×800 view at zoom 15
/// centered on Charing Cross (51.5080, -0.1281) needs, and at zoom 13 and at zoom 12 the tile
/// that holds that center and the one north of it, since the center sits just below a tile row
/// boundary at both.
const TILES: [(&str, &[u8]); 8] = [
    (
        "14/8185/5447",
        include_bytes!("../data/openfreemap/tiles/14-8185-5447.pbf.gz"),
    ),
    (
        "14/8186/5447",
        include_bytes!("../data/openfreemap/tiles/14-8186-5447.pbf.gz"),
    ),
    (
        "14/8185/5448",
        include_bytes!("../data/openfreemap/tiles/14-8185-5448.pbf.gz"),
    ),
    (
        "14/8186/5448",
        include_bytes!("../data/openfreemap/tiles/14-8186-5448.pbf.gz"),
    ),
    (
        "13/4093/2723",
        include_bytes!("../data/openfreemap/tiles/13-4093-2723.pbf.gz"),
    ),
    (
        "13/4093/2724",
        include_bytes!("../data/openfreemap/tiles/13-4093-2724.pbf.gz"),
    ),
    (
        "12/2046/1361",
        include_bytes!("../data/openfreemap/tiles/12-2046-1361.pbf.gz"),
    ),
    (
        "12/2046/1362",
        include_bytes!("../data/openfreemap/tiles/12-2046-1362.pbf.gz"),
    ),
];

/// Answer the recorded URLs on `sim`: the style, the TileJSON and the sprite with their bodies,
/// a recorded tile with its gzip bytes, and any other tile of the planet build with 404, which
/// the piece takes as "no tile here". Nothing else is matched, so on a strict simulation every
/// other request fails, and on a lenient one it goes out to the network.
///
/// The rules are added after any already on `sim`, and the first match wins, so this composes
/// with other route sets on one simulation (`Session::set_simulation` holds only one).
pub fn routes(sim: &Simulation) {
    let file = |url: String, content_type: &'static str, body: &'static [u8]| {
        sim.route("GET", &url, move |_| {
            Ok(Reply::new(200, body).header("content-type", content_type))
        });
    };
    file(
        STYLE_URL.into(),
        JSON,
        include_bytes!("../data/openfreemap/style-liberty.json"),
    );
    file(
        TILEJSON_URL.into(),
        JSON,
        include_bytes!("../data/openfreemap/tilejson-planet.json"),
    );
    file(
        format!("{SPRITE_URL}@2x.json"),
        JSON,
        include_bytes!("../data/openfreemap/sprite-ofm@2x.json"),
    );
    file(
        format!("{SPRITE_URL}@2x.png"),
        "image/png",
        include_bytes!("../data/openfreemap/sprite-ofm@2x.png"),
    );
    sim.handle(
        |method, url| method == "GET" && url.starts_with(TILES_URL),
        |request| {
            let path = request
                .url
                .strip_prefix(TILES_URL)
                .and_then(|p| p.strip_suffix(".pbf"))
                .ok_or(HttpError::Status(404))?;
            Ok(match TILES.iter().find(|(id, _)| *id == path) {
                Some((_, body)) => Reply::new(200, *body)
                    .header("content-type", "application/vnd.mapbox-vector-tile")
                    .header("content-encoding", "gzip"),
                None => Reply::new(404, Vec::new()),
            })
        },
    );
}
