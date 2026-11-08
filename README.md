<!--
Copyright © The Daybrite Project
SPDX-License-Identifier: MPL-2.0
-->

# day-piece-map

A vector map for Day apps: a MapLibre style over vector tiles, drawn on the canvas.

[Day](https://daybrite.dev/docs/) is a Rust framework for building apps with native desktop and
mobile controls, plus HTML controls on the web.

The piece takes a [MapLibre style](https://maplibre.org/maplibre-style-spec/) and draws it from
[Mapbox Vector Tiles](https://github.com/mapbox/vector-tile-spec) with Day's canvas: fills, lines,
circles, text labels placed so they do not collide, and sprite icons. Every frame is a display
list the toolkit replays, so the same style draws the same map on macOS, iOS, Android, Windows,
Linux, HarmonyOS and the web, with no map SDK and no WebView. The style engine, the tile sources
and the symbol placement follow [MapLibre GL JS](https://github.com/maplibre/maplibre-gl-js), and
the expression engine passes MapLibre's own test suite.

## Use

Add `day-piece-map = "0.*"` to your app's `[dependencies]` and build with the Day CLI:

```rust
use day::prelude::*;
use day_piece_map::{LngLat, MapView, Marker, map};

fn page() -> impl Piece {
    let view = MapView::new(LngLat::new(-0.1276, 51.5072), 12.0);
    column((
        map("https://tiles.openfreemap.org/styles/liberty", view).grow(),
        label(move || view.attribution.get()).font(Font::Caption),
    ))
}
```

`map` takes a style URL, the style's JSON itself, or a reactive value of either, which reloads
the map when it changes (a light and a dark style, say). `MapView` is the state you share with
the map: `center` and `zoom` move it when you write them and follow the user when they pan,
pinch, drag, scroll or wheel; `language` picks the language of the labels where the data has
it (`fr` shows `name:fr`, the local name otherwise); `status`, `attribution`, `pending` and
`frame` report what the piece is doing. The map draws the attribution in its corner, as the
tile services require.

### Gestures

One finger drags, two pinch to zoom and twist to turn, and a wheel or trackpad pans and
zooms, on every platform. A swipe glides on and slows the way Apple Maps and Google Maps do,
a pinch carries its speed on for a moment, a double tap zooms in a level about the spot, and
a twist that ends near north settles on north. A finger down stops any glide or flight.

### Markers, taps and the camera

```rust
let view = MapView::new(LngLat::new(-0.1276, 51.5072), 12.0);
let bridge = view.add_marker(Marker::new(LngLat::new(-0.0877, 51.5055)).title("Tower Bridge"));
Effect::new(move || {
    if let Some(tap) = view.tap.get() {
        if tap.marker == Some(bridge) {
            view.fly_to(LngLat::new(-0.0877, 51.5055), 16.0);
        } else if let Some(hit) = tap.features.first() {
            info!("{}: {:?}", hit.layer, hit.properties.get("name"));
        }
    }
});
```

- `add_marker`, `remove_marker`, `clear_markers` and `marker` manage pins with a title; they
  stay upright when the map turns.
- `tap` is the last tap: its point and position, the marker under it if any, else the
  features the style drew there, topmost first, each with its style layer, source layer and
  properties. A double tap zooms instead and is not reported.
- `fly_to` glides to a position and zoom over a second, dipping out to show the way on a long
  flight; `fly_to_with` sets the bearing and duration too; `fit_bounds` frames a box with
  padding; `rotate_to` and `reset_north` turn the map; `bearing` is the current turn in
  degrees; `jump_to` cuts straight there.

### Tiles

A style's `vector` sources resolve two ways:

- A z/x/y URL template, given inline in `tiles` or through the TileJSON document at `url`.
  This is what every hosted tile service speaks, [OpenFreeMap](https://openfreemap.org) among
  them with no API key.
- A [PMTiles](https://github.com/protomaps/PMTiles) archive, named as `pmtiles://https://…` or
  by a URL ending in `.pmtiles`: one static file on any object storage, read with HTTP range
  requests. Self-host the planet, or an extract of your region, with nothing but storage.

Everything is fetched through `day-part-http`, so requests go through the platform's own network
stack and a test intercepts them with its `Simulation`.

### How a frame is drawn

The base of the map (background, fills, lines, circles) is rasterized once per screen tile at
the current zoom, through the toolkit's offscreen painter (which paints on a background thread
on macOS, iOS and Android), and the frames blit those bitmaps: a pan or a zoom costs a few
dozen image draws however dense the city. While a tile's raster is
being painted, a scaled ancestor or its children stand in, so zooming never shows a hole.
Labels and icons are placed over the bitmaps and kept as one more bitmap, placed again when the
view has moved a quarter of its width, zoomed, or gained tiles, so text stays crisp while
panning. Tiles are decoded and filtered off the UI thread. `MapView::frame` reports what a
frame drew, and `DAY_LOG=debug` logs every frame, raster and placement with its timing.

### What it draws

Layer types `background`, `fill`, `line`, `symbol` and `circle`, which is what a basemap is made
of; a `fill-extrusion` draws flat. The expression language is complete except for `distance`,
`within`, `heatmap-density` and `elevation`, legacy stop functions and legacy filters included.
Text is drawn in the platform's own fonts: a style's `text-font` names pick the weight and the
slant, and the glyph server a style names is never contacted. Label placement follows MapLibre's
rules: along lines where the line is straight enough, point labels anchored and offset as the
style says, sorted by `symbol-sort-key`, and dropped when they would overlap a label placed
before them.

Icons and `fill-pattern` draw from the style's sprite, which the canvas cuts regions of on every
Day toolkit. Not drawn: `raster`, `hillshade`, `heatmap` and `sky` layers, `line-pattern`,
`line-gap-width` (the width is drawn, the gap is not), and pitch; bearing is drawn, as the two-finger twist shows.

## Testing

`tests/expression.rs` runs MapLibre's expression fixtures, vendored under `tests/data/expression`
(see [tests/data/README.md](tests/data/README.md) for provenance and license). The tile sources
are tested against intercepted HTTP, with an in-memory PMTiles archive answering range requests.
The renderer is tested on hand-built tiles, and `tests/openfreemap.rs` renders London from
OpenFreeMap's Liberty style as the live server sent it: the style, TileJSON, sprite and tiles
under `tests/data/openfreemap` are unmodified recordings (see its README for provenance and
licenses), answered by a strict `Simulation`, so nothing comes from the network. `tests/map.rs`
drives the piece on the mock toolkit: gestures, flings, flights, fits, turns, taps and double
taps. `demo/dayscript/demo.yaml` is the test on a real toolkit: it switches the demo to the
built-in island served in-process, taps its marker, flies, fits, turns, pans and zooms it, then
draws the recorded Liberty style and taps that.

## Demo

[`demo/`](demo/README.md) is a one-page app with the map, a picker between OpenFreeMap's and
VersaTiles' styles, the built-in sample and the recorded Liberty style, a language picker, zoom
and camera buttons and a readout of where the map is and what was tapped. From `demo/`, run
`day launch -t macos-appkit --script dayscript/demo.yaml`, or any target its `Day.toml` lists.

## Documentation

- Rust API: `cargo doc --open`
- [The MapLibre style spec](https://maplibre.org/maplibre-style-spec/): what a style can say
- [MapLibre GL JS](https://github.com/maplibre/maplibre-gl-js): the design this follows

[Source](https://github.com/daybrite/day-piece-map) · [MPL-2.0 license](LICENSE)
# day-piece-map
