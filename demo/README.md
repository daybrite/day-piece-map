<!--
Copyright © The Daybrite Project
SPDX-License-Identifier: CC-BY-SA-4.0
-->

# Map Demo

The demo and test app for [`day-piece-map`](..): one page with the map.

The picker at the top chooses where the style comes from. OpenFreeMap's and VersaTiles' styles are
the real thing: OpenStreetMap data for the whole planet, no key, over the network. The built-in
sample is a small island drawn by hand in the piece's `sample` module and served from inside the
app through `day-part-http`'s interception, as a MapLibre style and vector tiles encoded on
request, so the walkthrough draws the same map on every target without a network. The recorded
Liberty entry is the real style around Trafalgar Square, answered from the recording in the
piece's test data, also with no network. The second picker chooses the language of the labels.

The buttons fly between the two markers each source puts down, fit both into view, turn the map
by 45° and bring it back to north. The readout under the map (status, position, bearing, what
the last frame drew, what the last tap found, the attribution) is what the walkthrough drives
and asserts on; the map itself is a canvas, so the screenshots are what show it.

Pan with a drag, a two-finger scroll or a wheel; zoom with a pinch, a trackpad magnify, or
Control and the wheel; turn with a two-finger twist; double-tap to zoom in on a spot; tap a
marker or the map to see what is there. Swipes and pinches glide on after the fingers lift.

## Run it

```sh
day doctor
day launch -t macos-appkit --script dayscript/demo.yaml
day launch -t ios-uikit
day launch -t web-dom
```

`Day.toml` lists the targets the piece supports; pass any of them to `-t`. The walkthrough switches
to the built-in sample, taps its marker, flies, fits, turns, twists, zooms out to the island,
drags, pinches and pans it, switches to the recorded Liberty style and taps London, and saves
screenshots under `build/day/screenshots/`.
