// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! OpenFreeMap's Liberty style, recorded: the style, its TileJSON, its 2x sprite and the vector
//! tiles around central London, served from the generated `res::assets::map_recording` bundle
//! through `day-part-http`'s `Simulation` so the map renders the real style with no network.
//! The renderer keeps a matching test fixture so its published crate tests are self-contained.
//!
//! Tile assets use a `.bin` suffix so Android packaging preserves their gzip bytes and names.
//! The answers are the server's own bytes and headers. The tiles go out exactly as they came
//! over the wire, gzip with `content-encoding: gzip`: a native transport would inflate them
//! before the piece saw them, the simulation hands them over as they are, and the piece's tile
//! fetch inflates a gzip body itself, so both arrive at the same decoded tile.

use day_part_http::simulation::{Reply, Simulation};
use day_part_http::{HttpError, Session};

/// A simulation-only URL, distinct from live Liberty so switching sources reloads the style.
pub const STYLE_URL: &str = "https://day-map-recording.invalid/style.json";
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
/// Answer the recorded URLs on `sim`: the style, the TileJSON and the sprite with their bodies,
/// a recorded tile with its gzip bytes, and any other tile of the planet build with 404, which
/// the piece takes as "no tile here". Nothing else is matched, so on a strict simulation every
/// other request fails, and on a lenient one it goes out to the network.
///
/// The rules are added after any already on `sim`, and the first match wins, so this composes
/// with other route sets on one simulation (`Session::set_simulation` holds only one).
pub fn routes(sim: &Simulation) {
    use crate::res::assets::map_recording as assets;
    let file = |url: String, content_type: &'static str, name: day::AssetName| {
        let body = day::resource(name).map_or_else(Vec::new, |r| r.to_vec());
        sim.route("GET", &url, move |_| {
            Ok(Reply::new(200, body.clone()).header("content-type", content_type))
        });
    };
    file(STYLE_URL.into(), JSON, assets::style_liberty_json);
    file(TILEJSON_URL.into(), JSON, assets::tilejson_planet_json);
    file(
        format!("{SPRITE_URL}@2x.json"),
        JSON,
        assets::sprite_ofm_2x_json,
    );
    file(
        format!("{SPRITE_URL}@2x.png"),
        "image/png",
        assets::sprite_ofm_2x_png,
    );
    sim.handle(
        |method, url| method == "GET" && url.starts_with(TILES_URL),
        |request| {
            let path = request
                .url
                .strip_prefix(TILES_URL)
                .and_then(|p| p.strip_suffix(".pbf"))
                .ok_or(HttpError::Status(404))?;
            let tiles = [
                ("14/8185/5447", assets::tile_14_8185_5447_pbf_gz_bin),
                ("13/4093/2723", assets::tile_13_4093_2723_pbf_gz_bin),
                ("12/2046/1362", assets::tile_12_2046_1362_pbf_gz_bin),
                ("14/8186/5447", assets::tile_14_8186_5447_pbf_gz_bin),
                ("14/8185/5448", assets::tile_14_8185_5448_pbf_gz_bin),
                ("14/8186/5448", assets::tile_14_8186_5448_pbf_gz_bin),
                ("13/4093/2724", assets::tile_13_4093_2724_pbf_gz_bin),
                ("12/2046/1361", assets::tile_12_2046_1361_pbf_gz_bin),
            ];
            Ok(match tiles.iter().find(|(id, _)| *id == path) {
                Some((_, name)) => match day::resource(name.clone()) {
                    Some(body) => Reply::new(200, body.to_vec())
                        .header("content-type", "application/vnd.mapbox-vector-tile")
                        .header("content-encoding", "gzip"),
                    None => Reply::new(404, Vec::new()),
                },
                None => Reply::new(404, Vec::new()),
            })
        },
    );
}

/// Install the demo's bundled recording on the default session.
pub fn install() {
    let sim = Simulation::new();
    routes(&sim);
    Session::global().set_simulation(Some(sim));
}
