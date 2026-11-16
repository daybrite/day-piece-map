<!--
Copyright © The Daybrite Project
SPDX-License-Identifier: MPL-2.0
-->

# Test data

## `openfreemap/`

OpenFreeMap's Liberty style, its TileJSON, sprite and a handful of London tiles, recorded from
the live server for `tests/openfreemap.rs` and the demo. Sources, capture date and licenses are in
[`openfreemap/README.md`](openfreemap/README.md).

## `expression/`

MapLibre's style expression test fixtures, vendored unmodified.

- Source repository: https://github.com/maplibre/maplibre-style-spec
- Path in that repository: `test/integration/expression/tests`
- Snapshot: the `main` branch on 2026-11-07 (`package.json` version 26.5.1)
- License: BSD-3-Clause, reproduced beside the fixtures as `expression/LICENSE.txt`
- The files are copied as-is; nothing in them was edited. The directory names are the
  fixture names `tests/expression.rs` reports.

The vendored files carry MapLibre's copyright, not ours, so they take no Daybrite license
header.

To regenerate from a fresh checkout of the style spec at `$SPEC`:

```sh
rm -rf tests/data/expression && mkdir -p tests/data/expression && (cd "$SPEC/test/integration/expression/tests" && find . -name test.json | cpio -pdm "$PWD/tests/data/expression") && cp "$SPEC/LICENSE.txt" tests/data/expression/LICENSE.txt
```

(Run from the crate root; `$PWD` inside the subshell is the fixture directory, so write the
destination as an absolute path if your shell does not expand it the same way.)
