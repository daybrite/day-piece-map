// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! From a style, a camera and the loaded tiles to canvas ops: the frame.
//!
//! The order follows MapLibre's painter: layers in style order, and within a layer every tile
//! that stands in for a tile the viewport needs. A tile is drawn clipped to the tiles it
//! stands in for, so an ancestor filling in for tiles still loading never paints over a tile
//! that has arrived. Fills, lines and circles are clipped that way; symbols are not, since a
//! label may cross a tile edge, and the collision index keeps the copy from the neighbouring
//! tile from drawing twice.
//!
//! Geometry is transformed to screen points in Rust and decimated to the pixel, so the display
//! list the toolkit replays is as short as the screen can show.

use crate::camera::Camera;
use crate::expression::Context;
use crate::mvt;
use crate::source::TileId;
use crate::style::{Layer, LayerKind, Style, SymbolProps};
use crate::symbol::{self, Anchor, CollisionIndex};
use crate::{Feature, GeometryType, Value};
use day_geometry::{Affine, Color, Point, Rect, Size};
use day_pieces::{Draw, PathBuilder, TextStyle};
use day_spec::{
    CanvasFont, FillRule, FontWeight, LineCap, LineJoin, Shape, StrokeStyle, TextAnchor,
    TextMetrics,
};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// A decoded tile with its per-layer feature buckets, filled on first use: which features of
/// which source layer pass each style layer's filter at the tile's zoom.
#[derive(Debug)]
pub struct TileData {
    pub id: TileId,
    pub tile: mvt::Tile,
    buckets: std::sync::Mutex<HashMap<usize, Arc<Vec<usize>>>>,
    /// The symbol candidates of each symbol layer at each quarter zoom, in tile units: the
    /// expensive half of a placement (projection, text shaping, anchors along lines), done once
    /// per tile and reused by every placement at that zoom.
    symbols: std::sync::Mutex<SymbolSets>,
}

/// The cached symbol candidates of a tile, by (layer index, quarter zoom).
type SymbolSets = HashMap<(usize, u32), Arc<Vec<Candidate>>>;

impl TileData {
    pub fn new(id: TileId, tile: mvt::Tile) -> TileData {
        TileData {
            id,
            tile,
            buckets: std::sync::Mutex::new(HashMap::new()),
            symbols: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn symbols(&self, key: (usize, u32)) -> Option<Arc<Vec<Candidate>>> {
        self.symbols
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .cloned()
    }

    fn remember_symbols(&self, key: (usize, u32), set: Arc<Vec<Candidate>>) {
        self.symbols
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, set);
    }

    /// The indices of the features of `source_layer` that `layer` (the `index`th of the style)
    /// draws, evaluated once per tile.
    pub fn bucket(
        &self,
        index: usize,
        layer: &Layer,
        source_layer: &mvt::Layer,
    ) -> Arc<Vec<usize>> {
        if let Some(b) = self.lock().get(&index) {
            return b.clone();
        }
        #[cfg(not(target_arch = "wasm32"))]
        let started = std::time::Instant::now();
        let bucket = self.filter(layer, source_layer);
        #[cfg(not(target_arch = "wasm32"))]
        BUCKET_TIME.with(|t| t.set(t.get() + started.elapsed()));
        let bucket = Arc::new(bucket);
        self.lock().insert(index, bucket.clone());
        bucket
    }

    /// The bucket table; a poisoned lock (a panic while filtering) yields the table as it was.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<usize, Arc<Vec<usize>>>> {
        self.buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Build the buckets of every layer of `style` that reads this tile's `source`, so a
    /// frame finds them ready.
    pub fn prepare(&self, style: &Style, source: &str) {
        for (index, layer) in style.layers.iter().enumerate() {
            if layer.source.as_deref() != Some(source) {
                continue;
            }
            if let Some(source_layer) = layer
                .source_layer
                .as_deref()
                .and_then(|n| self.tile.layer(n))
            {
                self.bucket(index, layer, source_layer);
            }
        }
    }

    fn filter(&self, layer: &Layer, source_layer: &mvt::Layer) -> Vec<usize> {
        let ctx = Context {
            zoom: f64::from(self.id.z),
            tile: Some(self.id),
            ..Context::default()
        };
        let bucket: Vec<usize> = source_layer
            .features
            .iter()
            .enumerate()
            .filter(|(_, f)| match &layer.filter {
                None => true,
                Some(filter) => filter
                    .evaluate(&Context {
                        feature: Some(f),
                        ..ctx
                    })
                    .ok()
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            })
            .map(|(i, _)| i)
            .collect();
        bucket
    }
}

thread_local! {
    /// Time spent building buckets since the last frame started, for the frame's timing log.
    static BUCKET_TIME: std::cell::Cell<std::time::Duration> =
        const { std::cell::Cell::new(std::time::Duration::ZERO) };
}

/// The bucket time accumulated since the last call, and reset it.
pub fn take_bucket_time() -> std::time::Duration {
    BUCKET_TIME.with(|t| t.replace(std::time::Duration::ZERO))
}

// A tile is decoded and filtered off the UI thread and shared from an `Arc`, so it must cross
// threads and be read from any.
const fn assert_send_sync<T: Send + Sync>() {}
const _: () = assert_send_sync::<TileData>();

/// A loaded tile and where it is drawn this frame.
#[derive(Clone, Debug)]
pub struct RenderTile {
    /// The style source it came from.
    pub source: String,
    pub data: Arc<TileData>,
    /// The tile's own screen rect.
    pub rect: Rect,
    /// The screen rects of the tiles it stands in for (its own, or the descendants the
    /// viewport wanted that have not loaded); drawing is clipped to their union.
    pub clip: Vec<Rect>,
}

/// Choose what to draw for the tiles the viewport needs from `source`: each needed tile
/// itself where loaded, else its nearest loaded ancestor, grouped so an ancestor standing in
/// for several tiles is drawn once with a clip covering all of them.
pub fn resolve(
    camera: &Camera,
    source: &str,
    needed: &[(TileId, i32)],
    loaded: &dyn Fn(TileId) -> Option<Arc<TileData>>,
) -> Vec<RenderTile> {
    let mut groups: Vec<RenderTile> = Vec::new();
    for &(id, wrap) in needed {
        let mut probe = Some(id);
        let mut found = None;
        while let Some(p) = probe {
            if let Some(data) = loaded(p) {
                found = Some(data);
                break;
            }
            probe = p.parent();
        }
        let Some(data) = found else { continue };
        let wanted = camera.tile_rect(id, wrap);
        let rect = camera.tile_rect(data.id, wrap);
        match groups
            .iter_mut()
            .find(|g| g.data.id == data.id && g.rect == rect)
        {
            Some(group) => group.clip.push(wanted),
            None => groups.push(RenderTile {
                source: source.to_string(),
                data,
                rect,
                clip: vec![wanted],
            }),
        }
    }
    // Ancestors first, so a tile that arrived paints over the stand-in beneath it where the
    // clips overlap at their edges.
    groups.sort_by_key(|g| g.data.id.z);
    groups
}

/// One image of a sprite sheet, in sheet pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpriteImage {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub pixel_ratio: f64,
}

/// A sprite: the index of images and, once decoded, the sheet they are cut from.
#[derive(Clone, Debug)]
pub struct Sprite {
    pub images: HashMap<String, SpriteImage>,
    pub sheet: Option<day_core::Bitmap>,
}

impl Sprite {
    /// Parse a sprite index (`{sprite}.json`).
    pub fn parse_index(
        json: &serde_json::Value,
    ) -> Result<HashMap<String, SpriteImage>, crate::Error> {
        let obj = json
            .as_object()
            .ok_or_else(|| crate::Error::Invalid("sprite index is not an object".into()))?;
        let mut images = HashMap::new();
        for (name, entry) in obj {
            let n = |k: &str| entry.get(k).and_then(|v| v.as_f64());
            if let (Some(x), Some(y), Some(width), Some(height)) =
                (n("x"), n("y"), n("width"), n("height"))
            {
                images.insert(
                    name.clone(),
                    SpriteImage {
                        x,
                        y,
                        width,
                        height,
                        pixel_ratio: n("pixelRatio").unwrap_or(1.0),
                    },
                );
            }
        }
        Ok(images)
    }

    /// The names of the images, for `image` expressions to resolve against.
    pub fn names(&self) -> Vec<String> {
        self.images.keys().cloned().collect()
    }
}

/// Measures one line of text: `day_core::measure_text`, or a stand-in under test.
pub type Measure<'a> = &'a dyn Fn(&str, f64, &CanvasFont) -> TextMetrics;

/// Which layers a render draws. A frame is drawn in two passes: the base (background, fills,
/// lines, circles), rasterized once per tile into a bitmap the frames then blit, and the
/// symbols, placed and drawn live over the bitmaps so labels never cut at a tile edge or
/// collide across one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pass {
    /// Every layer, in order.
    All,
    /// The background and the fill, line and circle layers.
    Base,
    /// The background layer alone: what shows where no tile has arrived.
    Backdrop,
    /// The symbol layers alone.
    Symbols,
}

/// Everything a frame is drawn from.
pub struct Scene<'a> {
    pub style: &'a Style,
    pub camera: &'a Camera,
    pub tiles: &'a [RenderTile],
    pub sprite: Option<&'a Sprite>,
    pub measure: Measure<'a>,
    pub pass: Pass,
}

/// What a frame drew, for a readout or a test.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Layers that drew at least one feature.
    pub layers: usize,
    /// Features drawn (a polygon, a line or a point set each count once).
    pub features: usize,
    pub labels: usize,
    pub icons: usize,
    /// Symbols that lost to one placed before them.
    pub collided: usize,
    /// Rasterized tiles blitted (the view counts these; a render answers zero).
    pub bitmaps: usize,
}

/// Draw the scene.
pub fn render(d: &mut Draw, scene: &Scene<'_>) -> Stats {
    let camera = scene.camera;
    let viewport = Rect::from_size(camera.size);
    let zoom = camera.zoom;
    let mut stats = Stats::default();
    let mut index = CollisionIndex::new(viewport);
    let available = scene.sprite.map(Sprite::names);
    // Nothing leaves the canvas: a tile's fill runs past the edge, and not every toolkit clips a
    // canvas to its frame on its own.
    d.save();
    d.clip(Shape::Rect(viewport));
    let pass = scene.pass;
    let shown = |layer: &Layer| {
        let in_pass = match (&layer.kind, pass) {
            (_, Pass::All) => true,
            (LayerKind::Symbol(_), Pass::Symbols) => true,
            (LayerKind::Symbol(_), _) => false,
            (LayerKind::Background(_), Pass::Backdrop) => true,
            (_, Pass::Backdrop) => false,
            (_, Pass::Symbols) => false,
            (_, Pass::Base) => true,
        };
        in_pass && layer.visible && zoom >= layer.minzoom && zoom < layer.maxzoom
    };
    // Symbols are placed from the top layer down, so the layer a style puts last wins the
    // space (MapLibre's placement order), and drawn from the bottom up with everything else.
    let mut placed: HashMap<usize, Vec<Placed>> = HashMap::new();
    for (li, layer) in scene.style.layers.iter().enumerate().rev() {
        if let LayerKind::Symbol(p) = &layer.kind
            && shown(layer)
        {
            let ctx = Context {
                zoom,
                available_images: available.as_deref(),
                ..Context::default()
            };
            let symbols = place_symbols(scene, li, layer, p, &ctx, &mut index, &mut stats);
            placed.insert(li, symbols);
        }
    }
    for (li, layer) in scene.style.layers.iter().enumerate() {
        if !shown(layer) {
            continue;
        }
        let ctx = Context {
            zoom,
            available_images: available.as_deref(),
            ..Context::default()
        };
        match &layer.kind {
            LayerKind::Background(p) => {
                let color = with_opacity(p.color.color(&ctx), p.opacity.number(&ctx));
                d.fill(Shape::Rect(viewport), color);
                stats.layers += 1;
            }
            LayerKind::Unsupported(_) => {}
            LayerKind::Symbol(_) => {
                let symbols = placed.remove(&li).unwrap_or_default();
                if !symbols.is_empty() {
                    stats.layers += 1;
                }
                // The icon first, then the text over it: a road shield is an icon with its
                // number on top.
                for symbol in &symbols {
                    if let (Some(i), Some(sprite)) = (&symbol.icon, scene.sprite) {
                        draw_icon(d, sprite, &symbol.anchor, i);
                    }
                    if let Some(t) = &symbol.text {
                        draw_text(d, &symbol.anchor, t);
                    }
                }
            }
            _ => {
                let mut drew = false;
                for tile in scene
                    .tiles
                    .iter()
                    .filter(|t| Some(&t.source) == layer.source.as_ref())
                {
                    let Some(source_layer) = layer
                        .source_layer
                        .as_deref()
                        .and_then(|n| tile.data.tile.layer(n))
                    else {
                        continue;
                    };
                    let bucket = tile.data.bucket(li, layer, source_layer);
                    if bucket.is_empty() {
                        continue;
                    }
                    let scale = tile.rect.size.width / f64::from(source_layer.extent.max(1));
                    let visible = tile.clip.iter().any(|c| c.intersects(&viewport));
                    if !visible {
                        continue;
                    }
                    d.save();
                    clip_to(d, &tile.clip, &tile.rect);
                    let n = match &layer.kind {
                        LayerKind::Fill(p) => draw_fills(
                            d,
                            &ctx,
                            p,
                            source_layer,
                            &bucket,
                            tile.rect.origin,
                            scale,
                            &viewport,
                            scene.sprite,
                        ),
                        LayerKind::Line(p) => draw_lines(
                            d,
                            &ctx,
                            p,
                            source_layer,
                            &bucket,
                            tile.rect.origin,
                            scale,
                            &viewport,
                        ),
                        LayerKind::Circle(p) => draw_circles(
                            d,
                            &ctx,
                            p,
                            source_layer,
                            &bucket,
                            tile.rect.origin,
                            scale,
                            &viewport,
                        ),
                        _ => 0,
                    };
                    d.restore();
                    stats.features += n;
                    drew |= n > 0;
                }
                if drew {
                    stats.layers += 1;
                }
            }
        }
    }
    d.restore();
    stats
}

fn with_opacity(color: Color, opacity: f64) -> Color {
    color.with_alpha(color.a * opacity.clamp(0.0, 1.0))
}

/// How far a tile's clip reaches past its edge, in points: neighbouring fills overlap by this
/// much, so anti-aliasing at the two clip edges never leaves a hairline seam between them.
const CLIP_OVERLAP: f64 = 0.5;

fn clip_to(d: &mut Draw, rects: &[Rect], own: &Rect) {
    if rects.len() == 1 && rects[0] == *own {
        d.clip(Shape::Rect(grown(own, CLIP_OVERLAP)));
        return;
    }
    let mut path = PathBuilder::new().rule(FillRule::NonZero);
    for r in rects {
        let r = grown(r, CLIP_OVERLAP);
        let (x0, y0) = (r.origin.x, r.origin.y);
        let (x1, y1) = (x0 + r.size.width, y0 + r.size.height);
        path = path
            .move_to(Point::new(x0, y0))
            .line_to(Point::new(x1, y0))
            .line_to(Point::new(x1, y1))
            .line_to(Point::new(x0, y1))
            .line_to(Point::new(x0, y0));
    }
    d.clip(path.build());
}

/// A feature's parts in screen points, decimated to the pixel, with their bounds.
struct Projected {
    parts: Vec<Vec<Point>>,
    bounds: Rect,
}

/// How far, in points, a simplified outline may stray from the tile's geometry: under half a
/// device pixel at 2x, which is invisible, while a road made of near-collinear nodes loses most
/// of its vertices. The painters' cost follows vertices (Core Graphics strokes a thousand
/// 40-segment roads in three times the time of 12-segment ones), so this is the largest single
/// lever on raster time.
const SIMPLIFY_TOLERANCE: f64 = 0.4;

fn project(feature: &Feature, origin: Point, scale: f64, closed: bool) -> Projected {
    let (mut minx, mut miny, mut maxx, mut maxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    let mut parts = Vec::with_capacity(feature.geometry.len());
    for part in &feature.geometry {
        let mut out: Vec<Point> = Vec::with_capacity(part.len());
        for (i, p) in part.iter().enumerate() {
            let q = Point::new(origin.x + p.x * scale, origin.y + p.y * scale);
            let last = i + 1 == part.len();
            if let Some(prev) = out.last()
                && !last
                && (q.x - prev.x).abs() < 0.75
                && (q.y - prev.y).abs() < 0.75
            {
                continue;
            }
            out.push(q);
        }
        if closed && out.len() > 1 && out.first() == out.last() {
            out.pop();
        }
        let out = simplify(&out, SIMPLIFY_TOLERANCE);
        for q in &out {
            minx = minx.min(q.x);
            miny = miny.min(q.y);
            maxx = maxx.max(q.x);
            maxy = maxy.max(q.y);
        }
        if !out.is_empty() {
            parts.push(out);
        }
    }
    let bounds = if parts.is_empty() {
        Rect::ZERO
    } else {
        Rect::new(minx, miny, maxx - minx, maxy - miny)
    };
    Projected { parts, bounds }
}

/// Douglas-Peucker: the points of `pts` with every vertex that lies within `tolerance` of the
/// line between its kept neighbors removed. The ends always stay, so a closed ring keeps its
/// seam and consecutive parts still meet.
fn simplify(pts: &[Point], tolerance: f64) -> Vec<Point> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let mut keep = vec![false; pts.len()];
    keep[0] = true;
    keep[pts.len() - 1] = true;
    let mut stack = vec![(0usize, pts.len() - 1)];
    let tol2 = tolerance * tolerance;
    while let Some((a, b)) = stack.pop() {
        if b <= a + 1 {
            continue;
        }
        let (pa, pb) = (pts[a], pts[b]);
        let (dx, dy) = (pb.x - pa.x, pb.y - pa.y);
        let len2 = dx * dx + dy * dy;
        let mut worst = (0.0f64, a);
        for (i, p) in pts.iter().enumerate().take(b).skip(a + 1) {
            // Squared distance from the segment a-b (from the nearer end when it degenerates).
            let d2 = if len2 <= f64::EPSILON {
                (p.x - pa.x).powi(2) + (p.y - pa.y).powi(2)
            } else {
                let t = (((p.x - pa.x) * dx + (p.y - pa.y) * dy) / len2).clamp(0.0, 1.0);
                (p.x - (pa.x + t * dx)).powi(2) + (p.y - (pa.y + t * dy)).powi(2)
            };
            if d2 > worst.0 {
                worst = (d2, i);
            }
        }
        if worst.0 > tol2 {
            keep[worst.1] = true;
            stack.push((a, worst.1));
            stack.push((worst.1, b));
        }
    }
    pts.iter()
        .zip(keep)
        .filter_map(|(p, k)| k.then_some(*p))
        .collect()
}

/// A feature under a tap ([`crate::MapView::tap`]): the style layer that drew it and what the
/// tile says about it.
#[derive(Clone, Debug, PartialEq)]
pub struct FeatureHit {
    /// The style layer's id (`road_major`, `place_city`).
    pub layer: String,
    /// The tile's source layer (`transportation`, `place`).
    pub source_layer: String,
    pub id: Option<Value>,
    pub kind: GeometryType,
    pub properties: BTreeMap<String, Value>,
}

/// How far from a line, a circle's edge or a point feature a tap still counts, in points.
const TAP_REACH: f64 = 6.0;

/// The features drawn under `point`, topmost first, from the layers shown at this zoom: a
/// polygon that contains it, a line or a circle within reach of it, a point symbol near it.
pub fn features_at(
    style: &Style,
    camera: &Camera,
    tiles: &[RenderTile],
    point: Point,
) -> Vec<FeatureHit> {
    let zoom = camera.zoom;
    // Tiles are laid out north-up; the tap goes back into that frame.
    let p = camera.unturn(point);
    let inside = |r: &Rect, q: Point| {
        q.x >= r.origin.x
            && q.y >= r.origin.y
            && q.x <= r.origin.x + r.size.width
            && q.y <= r.origin.y + r.size.height
    };
    let mut hits = Vec::new();
    for (li, layer) in style.layers.iter().enumerate().rev() {
        if !(layer.visible && zoom >= layer.minzoom && zoom < layer.maxzoom) {
            continue;
        }
        let (Some(source), Some(source_layer_name)) =
            (layer.source.as_ref(), layer.source_layer.as_deref())
        else {
            continue;
        };
        for tile in tiles.iter().filter(|t| &t.source == source) {
            if !tile.clip.iter().any(|c| inside(c, p)) {
                continue;
            }
            let Some(source_layer) = tile.data.tile.layer(source_layer_name) else {
                continue;
            };
            let scale = tile.rect.size.width / f64::from(source_layer.extent.max(1));
            let bucket = tile.data.bucket(li, layer, source_layer);
            for &i in bucket.iter().rev() {
                let feature = &source_layer.features[i];
                let fctx = Context {
                    zoom,
                    feature: Some(feature),
                    ..Context::default()
                };
                let polygon = feature.kind == GeometryType::Polygon;
                let hit = match &layer.kind {
                    LayerKind::Fill(_) => {
                        polygon && {
                            let projected = project(feature, tile.rect.origin, scale, true);
                            inside(&projected.bounds, p) && polygon_contains(&projected.parts, p)
                        }
                    }
                    LayerKind::Line(props) => {
                        let reach = (props.width.number(&fctx).max(0.0) / 2.0).max(TAP_REACH);
                        let projected = project(feature, tile.rect.origin, scale, polygon);
                        inside(&grown(&projected.bounds, reach), p)
                            && within_reach(&projected.parts, p, reach, polygon)
                    }
                    LayerKind::Circle(props) => {
                        let reach = props.radius.number(&fctx).max(0.0) + TAP_REACH;
                        near_a_vertex(feature, tile.rect.origin, scale, p, reach)
                    }
                    LayerKind::Symbol(_) => {
                        feature.kind == GeometryType::Point
                            && near_a_vertex(feature, tile.rect.origin, scale, p, 2.0 * TAP_REACH)
                    }
                    _ => false,
                };
                if hit {
                    hits.push(FeatureHit {
                        layer: layer.id.clone(),
                        source_layer: source_layer_name.to_string(),
                        id: feature.id.clone(),
                        kind: feature.kind,
                        properties: feature.properties.clone(),
                    });
                }
            }
        }
    }
    hits
}

/// Even-odd containment over every ring, so holes count as outside.
fn polygon_contains(parts: &[Vec<Point>], p: Point) -> bool {
    let mut inside = false;
    for ring in parts {
        let n = ring.len();
        if n < 3 {
            continue;
        }
        let mut j = n - 1;
        for i in 0..n {
            let (a, b) = (ring[i], ring[j]);
            if (a.y > p.y) != (b.y > p.y) {
                let x = a.x + (p.y - a.y) / (b.y - a.y) * (b.x - a.x);
                if p.x < x {
                    inside = !inside;
                }
            }
            j = i;
        }
    }
    inside
}

/// Whether `p` is within `reach` of any segment of `parts` (rings closed when `closed`).
fn within_reach(parts: &[Vec<Point>], p: Point, reach: f64, closed: bool) -> bool {
    let reach2 = reach * reach;
    for part in parts {
        let n = part.len();
        if n == 1 {
            let d = (part[0].x - p.x).powi(2) + (part[0].y - p.y).powi(2);
            if d <= reach2 {
                return true;
            }
            continue;
        }
        let segments = if closed && n > 2 {
            n
        } else {
            n.saturating_sub(1)
        };
        for i in 0..segments {
            let (a, b) = (part[i], part[(i + 1) % n]);
            let (dx, dy) = (b.x - a.x, b.y - a.y);
            let len2 = dx * dx + dy * dy;
            let t = if len2 <= f64::EPSILON {
                0.0
            } else {
                (((p.x - a.x) * dx + (p.y - a.y) * dy) / len2).clamp(0.0, 1.0)
            };
            let d = (p.x - (a.x + t * dx)).powi(2) + (p.y - (a.y + t * dy)).powi(2);
            if d <= reach2 {
                return true;
            }
        }
    }
    false
}

/// Whether any vertex of the feature, projected, is within `reach` of `p`.
fn near_a_vertex(feature: &Feature, origin: Point, scale: f64, p: Point, reach: f64) -> bool {
    let reach2 = reach * reach;
    feature.geometry.iter().flatten().any(|v| {
        let q = Point::new(origin.x + v.x * scale, origin.y + v.y * scale);
        (q.x - p.x).powi(2) + (q.y - p.y).powi(2) <= reach2
    })
}

/// A path of `parts`. A closed part ends with a line back to its first point rather than a
/// close segment: a fill closes every subpath on its own, the outline differs only in the join
/// at one vertex, and a close followed by the next part's move is what makes AppKit's
/// `NSBezierPath` eighty times slower per subpath (measured: 4,000 closed subpaths take 42 ms
/// against 0.5 ms for the same points without the closes).
fn path_of(parts: &[Vec<Point>], closed: bool) -> Shape {
    let mut path = PathBuilder::new().rule(FillRule::NonZero);
    for part in parts {
        let mut it = part.iter();
        if let Some(first) = it.next() {
            path = path.move_to(*first);
            for p in it {
                path = path.line_to(*p);
            }
            if closed && part.len() > 2 {
                path = path.line_to(*first);
            }
        }
    }
    path.build()
}

/// The overlap of two rects, if any.
fn intersection(a: &Rect, b: &Rect) -> Option<Rect> {
    let x0 = a.origin.x.max(b.origin.x);
    let y0 = a.origin.y.max(b.origin.y);
    let x1 = (a.origin.x + a.size.width).min(b.origin.x + b.size.width);
    let y1 = (a.origin.y + a.size.height).min(b.origin.y + b.size.height);
    (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
}

/// Whether a feature is smaller than a pixel both ways at this scale: a building at zoom 12,
/// a footpath at zoom 10. Drawing it changes nothing visible and costs a path, and a city tile
/// has thousands of them.
fn too_small(bounds: &Rect) -> bool {
    bounds.size.width < 1.0 && bounds.size.height < 1.0
}

fn grown(r: &Rect, by: f64) -> Rect {
    Rect::new(
        r.origin.x - by,
        r.origin.y - by,
        r.size.width + 2.0 * by,
        r.size.height + 2.0 * by,
    )
}

/// The most pattern copies one polygon is tiled with; past this a pattern fill draws nothing
/// rather than a display list the size of a wallpaper.
const PATTERN_COPIES: usize = 256;

#[allow(clippy::too_many_arguments)]
fn draw_fills(
    d: &mut Draw,
    ctx: &Context<'_>,
    p: &crate::style::FillProps,
    source_layer: &mvt::Layer,
    bucket: &[usize],
    origin: Point,
    scale: f64,
    viewport: &Rect,
    sprite: Option<&Sprite>,
) -> usize {
    let mut n = 0;
    for &i in bucket {
        let feature = &source_layer.features[i];
        if feature.kind != GeometryType::Polygon {
            continue;
        }
        let fctx = Context {
            feature: Some(feature),
            ..*ctx
        };
        let projected = project(feature, origin, scale, true);
        if projected.parts.is_empty()
            || !projected.bounds.intersects(viewport)
            || too_small(&projected.bounds)
        {
            continue;
        }
        let opacity = p.opacity.number(&fctx);
        let shape = path_of(&projected.parts, true);
        // A pattern replaces the color, as in MapLibre: the sprite image is tiled over the
        // polygon, clipped to it.
        let pattern = p.pattern.image(&fctx);
        if let Some(name) = &pattern {
            if let Some((sheet, image)) =
                sprite.and_then(|s| s.sheet.as_ref().map(|sheet| (sheet, s.images.get(name))))
                && let Some(image) = image
            {
                let (w, h) = (
                    image.width / image.pixel_ratio,
                    image.height / image.pixel_ratio,
                );
                let area = intersection(&projected.bounds, viewport);
                if w > 0.0
                    && h > 0.0
                    && let Some(area) = area
                {
                    let cols = (area.size.width / w).ceil() as usize + 1;
                    let rows = (area.size.height / h).ceil() as usize + 1;
                    if cols * rows <= PATTERN_COPIES {
                        let source = Rect::new(image.x, image.y, image.width, image.height);
                        // Anchored to the world, so the pattern does not swim when the map pans.
                        let x0 = (area.origin.x / w).floor() * w;
                        let y0 = (area.origin.y / h).floor() * h;
                        d.save();
                        d.clip(shape.clone());
                        for r in 0..rows {
                            for c in 0..cols {
                                let dest = Rect::new(x0 + c as f64 * w, y0 + r as f64 * h, w, h);
                                d.image_region_with_opacity(
                                    sheet,
                                    source,
                                    dest,
                                    opacity.clamp(0.0, 1.0),
                                );
                            }
                        }
                        d.restore();
                        n += 1;
                    }
                }
            }
            continue;
        }
        let color = with_opacity(p.color.color(&fctx), opacity);
        if color.a > 0.0 {
            d.fill(shape.clone(), color);
        }
        if p.outline_color.is_set() {
            let outline = with_opacity(p.outline_color.color(&fctx), opacity);
            if outline.a > 0.0 {
                d.stroke(shape, outline, 1.0);
            }
        }
        n += 1;
    }
    n
}

/// Under this width, in points, a round join is drawn as a bevel (see `draw_lines`).
const THIN_LINE: f64 = 2.5;

fn cap_of(token: &str) -> LineCap {
    match token {
        "round" => LineCap::Round,
        "square" => LineCap::Square,
        _ => LineCap::Butt,
    }
}

fn join_of(token: &str) -> LineJoin {
    match token {
        "round" => LineJoin::Round,
        "bevel" => LineJoin::Bevel,
        _ => LineJoin::Miter,
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_lines(
    d: &mut Draw,
    ctx: &Context<'_>,
    p: &crate::style::LineProps,
    source_layer: &mvt::Layer,
    bucket: &[usize],
    origin: Point,
    scale: f64,
    viewport: &Rect,
) -> usize {
    let mut n = 0;
    for &i in bucket {
        let feature = &source_layer.features[i];
        if feature.kind == GeometryType::Point {
            continue;
        }
        let fctx = Context {
            feature: Some(feature),
            ..*ctx
        };
        let width = p.width.number(&fctx).max(0.0);
        // A patterned line has no color of its own; patterns along a line are not drawn.
        if width <= 0.0 || p.pattern.image(&fctx).is_some() {
            continue;
        }
        let closed = feature.kind == GeometryType::Polygon;
        let projected = project(feature, origin, scale, closed);
        if projected.parts.is_empty()
            || !grown(&projected.bounds, width).intersects(viewport)
            || too_small(&projected.bounds)
        {
            continue;
        }
        let color = with_opacity(p.color.color(&fctx), p.opacity.number(&fctx));
        if color.a <= 0.0 {
            continue;
        }
        let dash: Vec<f64> = p
            .dasharray
            .numbers(&fctx)
            .iter()
            .map(|len| (len * width).max(0.0))
            .collect();
        let style = StrokeStyle {
            width,
            cap: cap_of(&p.cap.token(&fctx)),
            // A round join on a line under two and a half points is a bevel with a corner the
            // eye cannot see, and the rasterizers price it: Core Graphics strokes round joins a
            // quarter slower than bevels.
            join: match join_of(&p.join.token(&fctx)) {
                LineJoin::Round if width < THIN_LINE => LineJoin::Bevel,
                join => join,
            },
            miter_limit: p.miter_limit.number(&fctx).max(1.0),
            dash: if dash.iter().all(|l| *l == 0.0) {
                Vec::new()
            } else {
                dash
            },
            ..StrokeStyle::default()
        };
        d.stroke_styled(path_of(&projected.parts, closed), color, style);
        n += 1;
    }
    n
}

#[allow(clippy::too_many_arguments)]
fn draw_circles(
    d: &mut Draw,
    ctx: &Context<'_>,
    p: &crate::style::CircleProps,
    source_layer: &mvt::Layer,
    bucket: &[usize],
    origin: Point,
    scale: f64,
    viewport: &Rect,
) -> usize {
    let mut n = 0;
    for &i in bucket {
        let feature = &source_layer.features[i];
        let fctx = Context {
            feature: Some(feature),
            ..*ctx
        };
        let radius = p.radius.number(&fctx).max(0.0);
        let stroke = p.stroke_width.number(&fctx).max(0.0);
        if radius <= 0.0 && stroke <= 0.0 {
            continue;
        }
        let at: Vec<Point> = feature
            .geometry
            .iter()
            .flat_map(|part| part.iter())
            .map(|q| Point::new(origin.x + q.x * scale, origin.y + q.y * scale))
            .filter(|q| grown(&Rect::new(q.x, q.y, 0.0, 0.0), radius + stroke).intersects(viewport))
            .collect();
        if at.is_empty() {
            continue;
        }
        let dot = Shape::Ellipse(Rect::new(-radius, -radius, 2.0 * radius, 2.0 * radius));
        let color = with_opacity(p.color.color(&fctx), p.opacity.number(&fctx));
        if color.a > 0.0 && radius > 0.0 {
            d.stamp(dot.clone(), at.clone(), color);
        }
        if stroke > 0.0 {
            let stroke_color =
                with_opacity(p.stroke_color.color(&fctx), p.stroke_opacity.number(&fctx));
            if stroke_color.a > 0.0 {
                d.stamp_styled(
                    dot,
                    at,
                    stroke_color,
                    StrokeStyle {
                        width: stroke,
                        ..StrokeStyle::default()
                    },
                );
            }
        }
        n += 1;
    }
    n
}

// ---------------------------------------------------------------------------
// Symbols
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct TextBlock {
    lines: Vec<String>,
    size: f64,
    font: CanvasFont,
    color: Color,
    halo_color: Color,
    halo_width: f64,
    line_height: f64,
    width: f64,
    height: f64,
    /// From the anchor to the block's center, before rotation.
    offset: Point,
    padding: f64,
    rotates: bool,
}

#[derive(Clone, Copy, Debug)]
struct IconBlock {
    image: SpriteImage,
    width: f64,
    height: f64,
    offset: Point,
    rotate: f64,
    opacity: f64,
    padding: f64,
    rotates: bool,
}

/// A symbol that found its place: what the draw pass paints.
struct Placed {
    anchor: Anchor,
    text: Option<TextBlock>,
    icon: Option<IconBlock>,
}

#[derive(Clone, Debug)]
struct Candidate {
    anchor: Anchor,
    text: Option<TextBlock>,
    icon: Option<IconBlock>,
    sort_key: Option<f64>,
    order: usize,
    text_allow_overlap: bool,
    text_ignore_placement: bool,
    text_optional: bool,
    icon_allow_overlap: bool,
    icon_ignore_placement: bool,
    icon_optional: bool,
}

/// The canvas font for a style's `text-font` stack: the platform's UI face, at the weight and
/// slant the first font's name asks for. (Fonts are named, not shipped, so Noto Sans Bold is
/// the UI face in bold.)
pub fn font_for(stack: &[String]) -> CanvasFont {
    let name = stack.first().map(String::as_str).unwrap_or("");
    let lower = name.to_ascii_lowercase();
    let weight = if lower.contains("black") || lower.contains("heavy") {
        Some(FontWeight::Black)
    } else if lower.contains("extrabold") || lower.contains("extra bold") {
        Some(FontWeight::Heavy)
    } else if lower.contains("bold") {
        Some(FontWeight::Bold)
    } else if lower.contains("semibold") || lower.contains("semi bold") {
        Some(FontWeight::Semibold)
    } else if lower.contains("medium") {
        Some(FontWeight::Medium)
    } else if lower.contains("light") {
        Some(FontWeight::Light)
    } else if lower.contains("thin") {
        Some(FontWeight::Thin)
    } else {
        None
    };
    CanvasFont {
        family: None,
        weight,
        italic: lower.contains("italic") || lower.contains("oblique"),
    }
}

fn transformed(text: String, how: &str) -> String {
    match how {
        "uppercase" => text.to_uppercase(),
        "lowercase" => text.to_lowercase(),
        _ => text,
    }
}

/// The anchor points of a feature for point placement: each point, or a polygon ring's
/// center (MapLibre uses the pole of inaccessibility; the bounds' center is close for the
/// convex shapes labels sit in).
fn point_anchors(parts: &[Vec<Point>], kind: GeometryType) -> Vec<Point> {
    match kind {
        GeometryType::Point => parts.iter().filter_map(|p| p.first().copied()).collect(),
        GeometryType::Polygon => {
            let mut best: Option<(f64, Point)> = None;
            for ring in parts {
                let (mut minx, mut miny, mut maxx, mut maxy) =
                    (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
                for p in ring {
                    minx = minx.min(p.x);
                    miny = miny.min(p.y);
                    maxx = maxx.max(p.x);
                    maxy = maxy.max(p.y);
                }
                let area = (maxx - minx) * (maxy - miny);
                if best.is_none_or(|(a, _)| area > a) {
                    best = Some((area, Point::new((minx + maxx) / 2.0, (miny + maxy) / 2.0)));
                }
            }
            best.map(|(_, p)| vec![p]).unwrap_or_default()
        }
        _ => parts
            .iter()
            .filter_map(|line| symbol::line_center_anchor(line, 0.0, f64::MAX).map(|a| a.point))
            .collect(),
    }
}

#[allow(clippy::too_many_arguments)]
fn place_symbols(
    scene: &Scene<'_>,
    li: usize,
    layer: &Layer,
    p: &SymbolProps,
    ctx: &Context<'_>,
    index: &mut CollisionIndex,
    stats: &mut Stats,
) -> Vec<Placed> {
    let mut candidates: Vec<Candidate> = Vec::new();
    for tile in scene
        .tiles
        .iter()
        .filter(|t| Some(&t.source) == layer.source.as_ref())
    {
        let Some(source_layer) = layer
            .source_layer
            .as_deref()
            .and_then(|n| tile.data.tile.layer(n))
        else {
            continue;
        };
        let scale = tile.rect.size.width / f64::from(source_layer.extent.max(1));
        // Candidates are cached per quarter zoom in tile units; the quarter step keeps a
        // zoom-dependent text size within a few percent of what this zoom asks for.
        let zq = (scene.camera.zoom.max(0.0) * 4.0).round() as u32;
        let key = (li, zq);
        let set = match tile.data.symbols(key) {
            Some(set) => set,
            None => {
                let set = Arc::new(tile_candidates(
                    scene,
                    li,
                    layer,
                    p,
                    ctx,
                    &tile.data,
                    source_layer,
                    scale,
                ));
                tile.data.remember_symbols(key, set.clone());
                set
            }
        };
        for c in set.iter() {
            let mut c = c.clone();
            c.anchor.point = Point::new(
                tile.rect.origin.x + c.anchor.point.x * scale,
                tile.rect.origin.y + c.anchor.point.y * scale,
            );
            // The tiles are laid out north-up; a turned view turns the anchors with it, and
            // text that follows its line turns too, flipping where that would set it on its
            // head.
            if scene.camera.bearing != 0.0 {
                c.anchor.point = scene.camera.turn(c.anchor.point);
                c.anchor.angle += scene.camera.bearing_radians();
                if c.text.as_ref().is_some_and(|t| t.rotates) {
                    while c.anchor.angle > std::f64::consts::FRAC_PI_2 {
                        c.anchor.angle -= std::f64::consts::PI;
                    }
                    while c.anchor.angle < -std::f64::consts::FRAC_PI_2 {
                        c.anchor.angle += std::f64::consts::PI;
                    }
                }
            }
            c.order = candidates.len();
            candidates.push(c);
        }
    }
    if p.z_order.token(ctx) != "source" {
        candidates.sort_by(|a, b| match (a.sort_key, b.sort_key) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(std::cmp::Ordering::Equal),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.order.cmp(&b.order),
        });
    }
    let mut placed = Vec::new();
    for c in candidates {
        let text_box = c.text.as_ref().map(|t| {
            let angle = if t.rotates { c.anchor.angle } else { 0.0 };
            let center = rotate_offset(c.anchor.point, t.offset, angle);
            symbol::rotated_bounds(center, t.width, t.height, angle, t.padding)
        });
        let icon_box = c.icon.as_ref().map(|i| {
            let angle = if i.rotates { c.anchor.angle } else { 0.0 } + i.rotate;
            let center = rotate_offset(
                c.anchor.point,
                i.offset,
                if i.rotates { c.anchor.angle } else { 0.0 },
            );
            symbol::rotated_bounds(center, i.width, i.height, angle, i.padding)
        });
        let mut text_fits = text_box
            .as_ref()
            .is_some_and(|b| index.is_visible(b) && (c.text_allow_overlap || !index.collides(b)));
        let mut icon_fits = icon_box
            .as_ref()
            .is_some_and(|b| index.is_visible(b) && (c.icon_allow_overlap || !index.collides(b)));
        // MapLibre's rule: a part that does not fit hides the other unless it is optional.
        if text_box.is_some() && !text_fits && !c.text_optional {
            icon_fits = false;
        }
        if icon_box.is_some() && !icon_fits && !c.icon_optional {
            text_fits = false;
        }
        if !text_fits && !icon_fits {
            stats.collided += 1;
            continue;
        }
        let mut symbol = Placed {
            anchor: c.anchor,
            text: None,
            icon: None,
        };
        if text_fits && let (Some(t), Some(b)) = (c.text, text_box) {
            if !c.text_ignore_placement {
                index.insert(b);
            }
            symbol.text = Some(t);
            stats.labels += 1;
        }
        if icon_fits && let (Some(i), Some(b)) = (c.icon, icon_box) {
            if !c.icon_ignore_placement {
                index.insert(b);
            }
            symbol.icon = Some(i);
            stats.icons += 1;
        }
        placed.push(symbol);
    }
    placed
}

/// The symbol candidates of one tile for one layer: every label and icon the features offer,
/// with anchors in tile units (see [`TileData::symbols`]). Nothing here depends on where the
/// tile is on screen, only on the zoom's text sizes and spacing.
#[allow(clippy::too_many_arguments)]
fn tile_candidates(
    scene: &Scene<'_>,
    li: usize,
    layer: &Layer,
    p: &SymbolProps,
    ctx: &Context<'_>,
    data: &TileData,
    source_layer: &mvt::Layer,
    scale: f64,
) -> Vec<Candidate> {
    let placement = p.placement.token(ctx);
    let along_line = placement == "line" || placement == "line-center";
    let spacing = p.spacing.number(ctx);
    let max_angle = p.text_max_angle.number(ctx).to_radians();
    let text_rotates = along_line && p.text_rotation_alignment.token(ctx) != "viewport";
    let bucket = data.bucket(li, layer, source_layer);
    let mut candidates: Vec<Candidate> = Vec::new();
    for &i in bucket.iter() {
        let feature = &source_layer.features[i];
        let fctx = Context {
            feature: Some(feature),
            ..*ctx
        };
        let projected = project(
            feature,
            Point::ZERO,
            scale,
            feature.kind == GeometryType::Polygon,
        );
        if projected.parts.is_empty() {
            continue;
        }
        // Text.
        let text = {
            let raw = p.text_field.text(&fctx);
            let content = transformed(raw.trim().to_string(), &p.text_transform.token(&fctx));
            if content.is_empty() {
                None
            } else {
                let size = p.text_size.number(&fctx).max(1.0);
                let font = font_for(&p.text_font.strings(&fctx));
                let max_width = p.text_max_width.number(&fctx) * size;
                let measure = |s: &str| (scene.measure)(s, size, &font).width;
                let lines = if along_line {
                    vec![content]
                } else {
                    symbol::wrap(&content, max_width, &measure)
                };
                let metrics = (scene.measure)(&lines[0], size, &font);
                let line_height =
                    metrics.height.max(size) * p.text_line_height.number(&fctx).max(0.5) / 1.2;
                let width = lines.iter().map(|l| measure(l)).fold(0.0, f64::max);
                let height = line_height * lines.len() as f64;
                let offset_em = p.text_offset.numbers(&fctx);
                let (ox, oy) = (
                    offset_em.first().copied().unwrap_or(0.0) * size,
                    offset_em.get(1).copied().unwrap_or(0.0) * size,
                );
                let anchor_off = symbol::anchor_offset(&p.text_anchor.token(&fctx), width, height);
                Some(TextBlock {
                    lines,
                    size,
                    font,
                    color: with_opacity(p.text_color.color(&fctx), p.text_opacity.number(&fctx)),
                    halo_color: with_opacity(
                        p.text_halo_color.color(&fctx),
                        p.text_opacity.number(&fctx),
                    ),
                    halo_width: p.text_halo_width.number(&fctx).clamp(0.0, size / 4.0),
                    line_height,
                    width,
                    height,
                    offset: Point::new(anchor_off.x + ox, anchor_off.y + oy),
                    padding: p.text_padding.number(ctx).max(0.0),
                    rotates: text_rotates,
                })
            }
        };
        // Icon.
        let icon = scene.sprite.and_then(|sprite| {
            let name = p.icon_image.image(&fctx)?;
            let image = *sprite.images.get(&name)?;
            let size = p.icon_size.number(&fctx).max(0.0);
            let (width, height) = (
                image.width / image.pixel_ratio * size,
                image.height / image.pixel_ratio * size,
            );
            if width <= 0.0 || height <= 0.0 {
                return None;
            }
            let offset_px = p.icon_offset.numbers(&fctx);
            let anchor_off = symbol::anchor_offset(&p.icon_anchor.token(&fctx), width, height);
            let alignment = p.icon_rotation_alignment.token(&fctx);
            Some(IconBlock {
                image,
                width,
                height,
                offset: Point::new(
                    anchor_off.x + offset_px.first().copied().unwrap_or(0.0) * size,
                    anchor_off.y + offset_px.get(1).copied().unwrap_or(0.0) * size,
                ),
                rotate: p.icon_rotate.number(&fctx).to_radians(),
                opacity: p.icon_opacity.number(&fctx),
                padding: p.icon_padding.number(&fctx).max(0.0),
                rotates: along_line && alignment != "viewport",
            })
        });
        if text.is_none() && icon.is_none() {
            continue;
        }
        let label_length = text
            .as_ref()
            .map(|t| t.width)
            .unwrap_or(0.0)
            .max(icon.as_ref().map(|i| i.width).unwrap_or(0.0));
        let anchors: Vec<Anchor> = if along_line && feature.kind != GeometryType::Point {
            projected
                .parts
                .iter()
                .flat_map(|line| {
                    if placement == "line-center" {
                        symbol::line_center_anchor(line, label_length, max_angle)
                            .into_iter()
                            .collect::<Vec<_>>()
                    } else {
                        symbol::line_anchors(line, spacing, label_length, max_angle)
                    }
                })
                .map(|a| Anchor {
                    point: a.point,
                    angle: symbol::upright(a.angle),
                })
                .collect()
        } else {
            point_anchors(&projected.parts, feature.kind)
                .into_iter()
                .map(|point| Anchor { point, angle: 0.0 })
                .collect()
        };
        let sort_key = p.sort_key.value(&fctx).as_f64();
        for anchor in anchors {
            // Stored in tile units: the placement scales them to the screen.
            let anchor = Anchor {
                point: Point::new(anchor.point.x / scale, anchor.point.y / scale),
                angle: anchor.angle,
            };
            candidates.push(Candidate {
                anchor,
                text: text.as_ref().map(|t| TextBlock {
                    lines: t.lines.clone(),
                    font: t.font.clone(),
                    ..*t
                }),
                icon: icon.as_ref().map(|i| IconBlock { ..*i }),
                sort_key,
                order: candidates.len(),
                text_allow_overlap: p.text_allow_overlap.boolean(ctx),
                text_ignore_placement: p.text_ignore_placement.boolean(ctx),
                text_optional: p.text_optional.boolean(ctx),
                icon_allow_overlap: p.icon_allow_overlap.boolean(ctx),
                icon_ignore_placement: p.icon_ignore_placement.boolean(ctx),
                icon_optional: p.icon_optional.boolean(ctx),
            });
        }
    }
    candidates
}

fn rotate_offset(anchor: Point, offset: Point, angle: f64) -> Point {
    let (s, c) = angle.sin_cos();
    Point::new(
        anchor.x + offset.x * c - offset.y * s,
        anchor.y + offset.x * s + offset.y * c,
    )
}

fn draw_text(d: &mut Draw, anchor: &Anchor, t: &TextBlock) {
    let angle = if t.rotates { anchor.angle } else { 0.0 };
    d.save();
    d.concat(Affine::rotate(angle).then(Affine::translate(anchor.point.x, anchor.point.y)));
    let top = t.offset.y - t.height / 2.0;
    let style = TextStyle {
        size: t.size,
        color: t.color,
        anchor: TextAnchor::CENTERED,
        font: t.font.clone(),
    };
    for (i, line) in t.lines.iter().enumerate() {
        let at = Point::new(t.offset.x, top + (i as f64 + 0.5) * t.line_height);
        if t.halo_width > 0.0 && t.halo_color.a > 0.0 {
            let halo = TextStyle {
                color: t.halo_color,
                font: t.font.clone(),
                ..style
            };
            // Four copies on the diagonals: the text drawn over them leaves a halo that reads
            // as an outline at the widths styles use (one to two points), for half the text ops
            // of the eight-direction form.
            let w = t.halo_width;
            for (dx, dy) in [(w, w), (-w, w), (w, -w), (-w, -w)] {
                d.text(
                    line,
                    Point::new(at.x + dx, at.y + dy),
                    TextStyle {
                        font: t.font.clone(),
                        ..halo
                    },
                );
            }
        }
        d.text(
            line,
            at,
            TextStyle {
                font: t.font.clone(),
                ..style
            },
        );
    }
    d.restore();
}

fn draw_icon(d: &mut Draw, sprite: &Sprite, anchor: &Anchor, i: &IconBlock) {
    let Some(sheet) = &sprite.sheet else { return };
    let angle = if i.rotates { anchor.angle } else { 0.0 } + i.rotate;
    d.save();
    d.concat(Affine::rotate(angle).then(Affine::translate(anchor.point.x, anchor.point.y)));
    let dest = Rect::new(
        i.offset.x - i.width / 2.0,
        i.offset.y - i.height / 2.0,
        i.width,
        i.height,
    );
    let source = Rect::new(i.image.x, i.image.y, i.image.width, i.image.height);
    d.image_region_with_opacity(sheet, source, dest, i.opacity.clamp(0.0, 1.0));
    d.restore();
}

/// The base pass of one raster tile, as ops in the square it covers on screen at
/// `display_zoom`: what a raster of the tile is rendered from. `screen` is the raster tile
/// (at or below the display zoom); `data` the loaded tile of each source that covers it (the
/// tile itself or an ancestor), drawn clipped to the square. Answers the ops and the square's
/// size in points.
pub fn tile_drawing(
    style: &Style,
    screen: TileId,
    display_zoom: f64,
    data: &[(String, Arc<TileData>)],
    sprite: Option<&Sprite>,
) -> (Vec<day_spec::DrawOp>, Size) {
    let n = (1u64 << screen.z) as f64;
    let tile_world = crate::camera::WORLD / n;
    let center = crate::camera::LngLat::from_world(Point::new(
        (f64::from(screen.x) + 0.5) * tile_world,
        (f64::from(screen.y) + 0.5) * tile_world,
    ));
    // The square is the tile's size on screen at the display zoom, and the camera is at that
    // zoom, so widths and sizes come out as the style asks for at the zoom the map shows.
    let side = crate::camera::TILE * 2f64.powf(display_zoom - f64::from(screen.z));
    let camera = Camera::new(center, display_zoom, Size::new(side, side));
    let square = Rect::new(0.0, 0.0, side, side);
    let tiles: Vec<RenderTile> = data
        .iter()
        .map(|(source, data)| RenderTile {
            source: source.clone(),
            rect: camera.tile_rect(data.id, 0),
            clip: vec![square],
            data: data.clone(),
        })
        .collect();
    let mut d = Draw::new();
    render(
        &mut d,
        &Scene {
            style,
            camera: &camera,
            tiles: &tiles,
            sprite,
            measure: &approximate_measure,
            pass: Pass::Base,
        },
    );
    (d.ops().to_vec(), Size::new(side, side))
}

/// A measurer for headless use: the same approximation a toolkit without text measurement
/// answers, so a test's placement is deterministic.
pub fn approximate_measure(text: &str, size: f64, _font: &CanvasFont) -> TextMetrics {
    TextMetrics::approximate(text, size)
}

/// The size a canvas of `size` needs its tiles at: a convenience for the piece and tests.
pub fn viewport(size: Size) -> Rect {
    Rect::from_size(size)
}

#[cfg(test)]
mod tests {
    #[test]
    fn simplify_drops_collinear_points_and_keeps_corners() {
        use day_geometry::Point;
        let road: Vec<Point> = (0..20)
            .map(|i| Point::new(i as f64 * 5.0, 0.1 * (i % 2) as f64))
            .collect();
        let kept = super::simplify(&road, 0.4);
        assert_eq!(kept.len(), 2, "{kept:?}");
        let corner = vec![
            Point::new(0.0, 0.0),
            Point::new(10.0, 0.0),
            Point::new(10.0, 10.0),
        ];
        assert_eq!(super::simplify(&corner, 0.4), corner);
        assert_eq!(super::simplify(&corner[..2], 0.4), corner[..2]);
    }

    use super::*;
    use crate::camera::LngLat;
    use crate::mvt::{Layer as MvtLayer, Tile};
    use crate::{Feature, Value};
    use day_spec::DrawOp;
    use std::collections::BTreeMap;

    fn feature(
        kind: GeometryType,
        parts: Vec<Vec<(f64, f64)>>,
        props: &[(&str, Value)],
    ) -> Feature {
        Feature {
            id: None,
            kind,
            properties: props
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            geometry: parts
                .into_iter()
                .map(|part| part.into_iter().map(|(x, y)| Point::new(x, y)).collect())
                .collect(),
        }
    }

    fn fixture_tile() -> Tile {
        Tile {
            layers: vec![
                MvtLayer {
                    name: "water".into(),
                    version: 2,
                    extent: 4096,
                    features: vec![feature(
                        GeometryType::Polygon,
                        vec![vec![
                            (0.0, 0.0),
                            (4096.0, 0.0),
                            (4096.0, 2048.0),
                            (0.0, 2048.0),
                            (0.0, 0.0),
                        ]],
                        &[("class", Value::String("ocean".into()))],
                    )],
                },
                MvtLayer {
                    name: "transportation".into(),
                    version: 2,
                    extent: 4096,
                    features: vec![
                        feature(
                            GeometryType::LineString,
                            vec![vec![(0.0, 3000.0), (4096.0, 3000.0)]],
                            &[("class", Value::String("primary".into()))],
                        ),
                        feature(
                            GeometryType::LineString,
                            vec![vec![(100.0, 3500.0), (300.0, 3500.0)]],
                            &[("class", Value::String("path".into()))],
                        ),
                    ],
                },
                MvtLayer {
                    name: "place".into(),
                    version: 2,
                    extent: 4096,
                    features: vec![
                        feature(
                            GeometryType::Point,
                            vec![vec![(2048.0, 2048.0)]],
                            &[
                                ("name", Value::String("Middle".into())),
                                ("rank", Value::Number(1.0)),
                            ],
                        ),
                        feature(
                            GeometryType::Point,
                            vec![vec![(2060.0, 2052.0)]],
                            &[
                                ("name", Value::String("Also Middle".into())),
                                ("rank", Value::Number(2.0)),
                            ],
                        ),
                        feature(
                            GeometryType::Point,
                            vec![vec![(500.0, 500.0)]],
                            &[
                                ("name", Value::String("Corner".into())),
                                ("rank", Value::Number(3.0)),
                            ],
                        ),
                    ],
                },
            ],
        }
    }

    const STYLE: &str = r##"{
      "version": 8,
      "sources": { "osm": { "type": "vector", "tiles": ["https://example.test/{z}/{x}/{y}.pbf"] } },
      "layers": [
        { "id": "bg", "type": "background", "paint": { "background-color": "#eee" } },
        { "id": "water", "type": "fill", "source": "osm", "source-layer": "water",
          "paint": { "fill-color": "#9bf", "fill-outline-color": "#58d" } },
        { "id": "roads", "type": "line", "source": "osm", "source-layer": "transportation",
          "filter": ["==", "class", "primary"],
          "layout": { "line-cap": "round", "line-join": "round" },
          "paint": { "line-color": "#fff", "line-width": ["interpolate", ["linear"], ["zoom"], 5, 1, 15, 8],
                     "line-dasharray": [2, 1] } },
        { "id": "hidden", "type": "line", "source": "osm", "source-layer": "transportation",
          "layout": { "visibility": "none" }, "paint": { "line-color": "#000" } },
        { "id": "places", "type": "symbol", "source": "osm", "source-layer": "place",
          "layout": { "text-field": ["get", "name"], "text-size": 14, "symbol-sort-key": ["get", "rank"] },
          "paint": { "text-color": "#222", "text-halo-color": "#fff", "text-halo-width": 1 } }
      ]
    }"##;

    fn scene_tiles(camera: &Camera) -> Vec<RenderTile> {
        let data = Arc::new(TileData::new(TileId::new(0, 0, 0), fixture_tile()));
        let needed = camera.covering_tiles(0);
        resolve(camera, "osm", &needed, &|id| {
            (id == TileId::new(0, 0, 0)).then(|| data.clone())
        })
    }

    #[test]
    fn a_frame_draws_fills_lines_and_labels_in_layer_order() {
        let style = Style::parse_str(STYLE).unwrap();
        let camera = Camera::new(LngLat::new(0.0, 0.0), 0.0, Size::new(512.0, 512.0));
        let tiles = scene_tiles(&camera);
        assert_eq!(tiles.len(), 1);
        let mut d = Draw::new();
        let stats = render(
            &mut d,
            &Scene {
                style: &style,
                camera: &camera,
                tiles: &tiles,
                sprite: None,
                measure: &approximate_measure,
                pass: Pass::All,
            },
        );
        assert_eq!(stats.layers, 4, "{stats:?}");
        assert_eq!(stats.features, 2); // the water polygon and the primary road
        // "Middle" and "Also Middle" overlap: the lower rank wins and the other is dropped.
        assert_eq!(stats.labels, 2);
        assert_eq!(stats.collided, 1);
        let ops = d.ops();
        let fills = ops.iter().filter(|o| matches!(o, DrawOp::Fill(..))).count();
        let strokes = ops
            .iter()
            .filter(|o| matches!(o, DrawOp::Stroke(..)))
            .count();
        let texts: Vec<&String> = ops
            .iter()
            .filter_map(|o| match o {
                DrawOp::Text { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(fills, 2, "background and water");
        assert_eq!(strokes, 2, "the water outline and the road");
        // Each label is drawn five times: four halo copies and the text itself.
        assert_eq!(texts.len(), 10);
        assert!(texts.iter().any(|t| *t == "Middle") && texts.iter().any(|t| *t == "Corner"));
        assert!(!texts.iter().any(|t| *t == "Also Middle"));
        // The road's width follows the zoom ramp: 1 at zoom 0 (below the first stop).
        let road = ops
            .iter()
            .find_map(|o| match o {
                DrawOp::Stroke(_, _, style) if !style.dash.is_empty() => Some(style.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(road.width, 1.0);
        assert_eq!(road.cap, LineCap::Round);
        assert_eq!(road.dash, vec![2.0, 1.0]);
    }

    #[test]
    fn an_ancestor_stands_in_for_missing_tiles_with_a_clip() {
        let camera = Camera::new(LngLat::new(0.0, 0.0), 1.0, Size::new(512.0, 512.0));
        let data = Arc::new(TileData::new(TileId::new(0, 0, 0), fixture_tile()));
        let needed = camera.covering_tiles(1);
        assert_eq!(needed.len(), 4);
        let tiles = resolve(&camera, "osm", &needed, &|id| {
            (id.z == 0).then(|| data.clone())
        });
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].clip.len(), 4);
        assert_eq!(tiles[0].rect, Rect::new(-256.0, -256.0, 1024.0, 1024.0));
        // With one child loaded, the ancestor serves the other three.
        let child = Arc::new(TileData::new(TileId::new(1, 0, 0), fixture_tile()));
        let tiles = resolve(&camera, "osm", &needed, &|id| {
            if id.z == 0 {
                Some(data.clone())
            } else if id == TileId::new(1, 0, 0) {
                Some(child.clone())
            } else {
                None
            }
        });
        assert_eq!(tiles.len(), 2);
        assert_eq!(tiles[0].data.id.z, 0);
        assert_eq!(tiles[0].clip.len(), 3);
        assert_eq!(tiles[1].data.id, TileId::new(1, 0, 0));
        assert_eq!(tiles[1].clip, vec![tiles[1].rect]);
    }

    #[test]
    fn line_labels_follow_the_road() {
        let style = Style::parse_str(
            r##"{ "version": 8,
              "sources": { "osm": { "type": "vector", "tiles": ["x/{z}/{x}/{y}"] } },
              "layers": [ { "id": "road-names", "type": "symbol", "source": "osm",
                "source-layer": "transportation", "filter": ["==", "class", "primary"],
                "layout": { "symbol-placement": "line", "text-field": "High Street", "text-size": 12,
                            "symbol-spacing": 200 } } ] }"##,
        )
        .unwrap();
        let camera = Camera::new(LngLat::new(0.0, 0.0), 1.0, Size::new(1024.0, 1024.0));
        let data = Arc::new(TileData::new(TileId::new(0, 0, 0), fixture_tile()));
        let needed = camera.covering_tiles(1);
        let tiles = resolve(&camera, "osm", &needed, &|id| {
            (id.z == 0).then(|| data.clone())
        });
        let mut d = Draw::new();
        let stats = render(
            &mut d,
            &Scene {
                style: &style,
                camera: &camera,
                tiles: &tiles,
                sprite: None,
                measure: &approximate_measure,
                pass: Pass::All,
            },
        );
        // The road spans the tile (1024 px at zoom 1): 200 px spacing places several labels,
        // each under its own transform.
        assert!(stats.labels >= 3, "{stats:?}");
        let concats = d
            .ops()
            .iter()
            .filter(|o| matches!(o, DrawOp::Concat(_)))
            .count();
        assert_eq!(concats, stats.labels);
    }

    #[test]
    fn sprite_indexes_parse_and_fonts_map_to_weights() {
        let json: serde_json::Value = serde_json::from_str(
            r#"{"airport": {"x": 0, "y": 0, "width": 34, "height": 34, "pixelRatio": 2},
                "broken": {"x": 1}}"#,
        )
        .unwrap();
        let images = Sprite::parse_index(&json).unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images["airport"].pixel_ratio, 2.0);
        let bold = font_for(&["Noto Sans Bold".to_string()]);
        assert_eq!(bold.weight, Some(FontWeight::Bold));
        assert!(!bold.italic);
        let italic = font_for(&["Noto Sans Italic".to_string()]);
        assert_eq!(italic.weight, None);
        assert!(italic.italic);
        let _ = BTreeMap::<String, Value>::new();
    }
}
