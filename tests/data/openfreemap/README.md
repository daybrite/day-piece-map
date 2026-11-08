<!--
Copyright © The Daybrite Project
SPDX-License-Identifier: MPL-2.0
-->

# OpenFreeMap Liberty, recorded

Server responses from [OpenFreeMap](https://openfreemap.org) for its Liberty style over central
London, so the piece's test (`tests/openfreemap.rs`) and the demo (`demo/src/offline.rs`) can
draw a production style and production tiles with no network. `tests/support/openfreemap.rs`
serves them through `day-part-http`'s `Simulation`.

Captured on 2026-11-08 between 02:20:16 and 02:21:17 UTC with `curl`. Every file is the response
body exactly as the server sent it; nothing in them was edited.

| File | URL | Response headers that matter |
| --- | --- | --- |
| `style-liberty.json` | `https://tiles.openfreemap.org/styles/liberty` | `content-type: application/json` |
| `tilejson-planet.json` | `https://tiles.openfreemap.org/planet` | `content-type: application/json` |
| `sprite-ofm@2x.json` | `https://tiles.openfreemap.org/sprites/ofm_f384/ofm@2x.json` | `content-type: application/json` |
| `sprite-ofm@2x.png` | `https://tiles.openfreemap.org/sprites/ofm_f384/ofm@2x.png` | `content-type: image/png` |
| `tiles/{z}-{x}-{y}.pbf.gz` | `https://tiles.openfreemap.org/planet/20261101_080013_pt/{z}/{x}/{y}.pbf` | `content-type: application/vnd.mapbox-vector-tile`, `content-encoding: gzip` |

The style, the TileJSON and the sprite were fetched without `Accept-Encoding`, so they are the
plain documents a transport hands over after decoding. The tiles were fetched with
`Accept-Encoding: gzip` and are the gzip bytes on the wire (OpenFreeMap stores them compressed:
each file's size is the one in its `ETag`). They are replayed with `content-encoding: gzip`; the
piece inflates a gzip tile body itself, so the replay reaches the same decoded tile a native
transport would deliver.

The TileJSON's template names the planet build `20261101_080013_pt`. OpenFreeMap replaces builds
regularly, so the live style soon points at a newer one; the recording keeps this one.

## The tiles

| Tile | Why |
| --- | --- |
| `14/8185/5447`, `14/8186/5447`, `14/8185/5448`, `14/8186/5448` | The tiles a 1280×800 view at zoom 15 centered on 51.5080, -0.1281 (Charing Cross) needs (`Camera::covering_tiles(14)`) |
| `13/4093/2724`, `12/2046/1362` | The zoom 13 and zoom 12 tiles that contain that center |
| `13/4093/2723`, `12/2046/1361` | The tiles directly north of those: the center lies just below their shared edge |

Any other tile of the build is answered 404, which the piece draws as empty.

## Licenses

- **Map data**: © [OpenStreetMap](https://www.openstreetmap.org/copyright) contributors,
  available under the [Open Database License (ODbL)](https://opendatacommons.org/licenses/odbl/).
- **Tile schema**: [OpenMapTiles](https://openmaptiles.org); its code is BSD-3-Clause and its
  schema and design are [CC BY 4.0](https://github.com/openmaptiles/openmaptiles/blob/master/LICENSE.md).
  Credit: © OpenMapTiles, © OpenStreetMap contributors.
- **Style (`style-liberty.json`)**: OpenFreeMap's Liberty, a fork of
  [maputnik/osm-liberty](https://github.com/maputnik/osm-liberty), itself derived from OSM Bright
  (OpenMapTiles, from Mapbox Open Styles). The style JSON is under the BSD 3-Clause License and
  the design under CC BY 4.0, as stated in
  [osm-liberty's LICENSE.md](https://github.com/maputnik/osm-liberty/blob/gh-pages/LICENSE.md)
  and [openfreemap-styles' LICENSE.md](https://github.com/hyperknot/openfreemap-styles/blob/main/LICENSE.md).
- **Sprite (`sprite-ofm@2x.*`)**: built by openfreemap-styles (MIT) from the
  [Maki](https://github.com/mapbox/maki) icon set (CC0 1.0) and a public-domain arrow from
  Wikimedia Commons.

These files carry their authors' licenses, not ours, so they take no Daybrite license header.

## Recording again

From the crate root, with `T` the tile template's prefix from a fresh `tilejson-planet.json`:

```sh
D=tests/data/openfreemap
curl -sS -o $D/style-liberty.json https://tiles.openfreemap.org/styles/liberty
curl -sS -o $D/tilejson-planet.json https://tiles.openfreemap.org/planet
curl -sS -o $D/sprite-ofm@2x.json https://tiles.openfreemap.org/sprites/ofm_f384/ofm@2x.json
curl -sS -o $D/sprite-ofm@2x.png https://tiles.openfreemap.org/sprites/ofm_f384/ofm@2x.png
for t in 14/8185/5447 14/8186/5447 14/8185/5448 14/8186/5448 13/4093/2723 13/4093/2724 12/2046/1361 12/2046/1362; do
  curl -sS -H 'Accept-Encoding: gzip' -o "$D/tiles/$(echo $t | tr / -).pbf.gz" "$T/$t.pbf"
done
```

Then update the build name and sprite path in `tests/support/openfreemap.rs` if the style or the
TileJSON changed them, and the capture date above.
