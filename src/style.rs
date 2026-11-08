// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! A MapLibre style document (<https://maplibre.org/maplibre-style-spec/>), compiled: its
//! sources, its sprite and its layers, with every filter and property parsed into an
//! [`Expression`] once so that drawing a frame evaluates trees, never JSON.
//!
//! The layer types drawn are `background`, `fill`, `line`, `symbol` and `circle`, which is what
//! a basemap is made of. `raster`, `raster-dem`, `hillshade`, `heatmap`, `fill-extrusion` and
//! `sky` layers are kept in the layer list as [`LayerKind::Unsupported`] so the order is
//! preserved and a caller can see what was skipped; a `fill-extrusion` layer is drawn as a flat
//! fill of its color, which is what MapLibre draws at pitch zero too.
//!
//! The property tables mirror `src/reference/v8.json` in the style spec: each property has a
//! type, a default and whether it may read the feature. Only the properties the renderer uses
//! are tabled; a style may carry any other property and it is ignored.

use crate::expression::{Context, Expression, PropertySpec, Type};
use crate::source::SourceSpec;
use crate::{Error, Value};
use day_geometry::Color;
use std::collections::BTreeMap;

/// A compiled style.
#[derive(Clone, Debug, Default)]
pub struct Style {
    pub name: Option<String>,
    pub sources: BTreeMap<String, SourceSpec>,
    /// The sprite's base URL: `{sprite}.json` and `{sprite}.png` (`@2x` variants at high
    /// density), where the style has one.
    pub sprite: Option<String>,
    pub layers: Vec<Layer>,
}

/// One style layer.
#[derive(Clone, Debug)]
pub struct Layer {
    pub id: String,
    pub source: Option<String>,
    pub source_layer: Option<String>,
    pub minzoom: f64,
    pub maxzoom: f64,
    pub filter: Option<Expression>,
    /// `layout.visibility` is not `none`.
    pub visible: bool,
    pub kind: LayerKind,
}

/// A layer's type and its properties, boxed: a compiled property is a tree and a symbol layer
/// has forty of them, while a layer list is walked by kind every frame.
#[derive(Clone, Debug)]
pub enum LayerKind {
    Background(Box<BackgroundProps>),
    Fill(Box<FillProps>),
    Line(Box<LineProps>),
    Symbol(Box<SymbolProps>),
    Circle(Box<CircleProps>),
    /// A layer type this crate does not draw, by name.
    Unsupported(String),
}

/// One property: its compiled expression and the spec's default, read through the typed
/// getters, which fall back to the default when the expression fails or answers `null`.
#[derive(Clone, Debug)]
pub struct Property {
    expr: Expression,
    default: Value,
    feature_constant: bool,
}

impl Property {
    fn new(expr: Expression, default: Value) -> Property {
        let feature_constant = expr.is_feature_constant();
        Property {
            expr,
            default,
            feature_constant,
        }
    }

    /// Whether the value is the same for every feature at a given zoom, so a renderer can
    /// evaluate it once per layer per frame.
    pub fn is_feature_constant(&self) -> bool {
        self.feature_constant
    }

    /// The expression's value, or the default where it fails or is `null`.
    pub fn value(&self, ctx: &Context<'_>) -> Value {
        match self.expr.evaluate(ctx) {
            Ok(Value::Null) | Err(_) => self.default.clone(),
            Ok(v) => v,
        }
    }

    pub fn number(&self, ctx: &Context<'_>) -> f64 {
        self.value(ctx)
            .as_f64()
            .or_else(|| self.default.as_f64())
            .unwrap_or(0.0)
    }

    pub fn boolean(&self, ctx: &Context<'_>) -> bool {
        self.value(ctx)
            .as_bool()
            .or_else(|| self.default.as_bool())
            .unwrap_or(false)
    }

    pub fn color(&self, ctx: &Context<'_>) -> Color {
        match self.value(ctx) {
            Value::Color(c) => c,
            Value::String(s) => crate::expression::parse_color(&s)
                .or_else(|| self.default.as_color())
                .unwrap_or(Color::rgba(0.0, 0.0, 0.0, 0.0)),
            _ => self
                .default
                .as_color()
                .unwrap_or(Color::rgba(0.0, 0.0, 0.0, 0.0)),
        }
    }

    /// An enum property's token.
    pub fn token(&self, ctx: &Context<'_>) -> String {
        match self.value(ctx) {
            Value::String(s) => s,
            _ => self
                .default
                .as_str()
                .map(str::to_string)
                .unwrap_or_default(),
        }
    }

    /// A numeric array property (`line-dasharray`, `text-offset`).
    pub fn numbers(&self, ctx: &Context<'_>) -> Vec<f64> {
        let items = match self.value(ctx) {
            Value::Array(items) => items,
            _ => match &self.default {
                Value::Array(items) => items.clone(),
                _ => Vec::new(),
            },
        };
        items.iter().filter_map(Value::as_f64).collect()
    }

    /// A string array property (`text-font`).
    pub fn strings(&self, ctx: &Context<'_>) -> Vec<String> {
        let items = match self.value(ctx) {
            Value::Array(items) => items,
            _ => match &self.default {
                Value::Array(items) => items.clone(),
                _ => Vec::new(),
            },
        };
        items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect()
    }

    /// The text of a `formatted` property: its sections joined, with each section's own
    /// overrides dropped.
    pub fn text(&self, ctx: &Context<'_>) -> String {
        match self.value(ctx) {
            Value::String(s) => s,
            Value::Number(n) => format_number(n),
            Value::Formatted(sections) => sections.iter().map(|s| s.text.as_str()).collect(),
            _ => String::new(),
        }
    }

    /// The image name of a `resolvedImage` property, if any.
    pub fn image(&self, ctx: &Context<'_>) -> Option<String> {
        match self.value(ctx) {
            Value::String(s) if !s.is_empty() => Some(s),
            Value::Image { name, .. } => Some(name),
            _ => None,
        }
    }

    /// Whether this property was given by the style (it is not the bare default).
    pub fn is_set(&self) -> bool {
        self.expr.as_constant() != Some(&self.default) || !self.feature_constant
    }
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

#[derive(Clone, Debug)]
pub struct BackgroundProps {
    pub color: Property,
    pub opacity: Property,
}

#[derive(Clone, Debug)]
pub struct FillProps {
    pub color: Property,
    pub opacity: Property,
    pub outline_color: Property,
    pub pattern: Property,
    pub antialias: Property,
}

#[derive(Clone, Debug)]
pub struct LineProps {
    pub color: Property,
    pub width: Property,
    pub gap_width: Property,
    pub opacity: Property,
    pub dasharray: Property,
    pub cap: Property,
    pub join: Property,
    pub miter_limit: Property,
    pub offset: Property,
    pub pattern: Property,
}

#[derive(Clone, Debug)]
pub struct SymbolProps {
    pub placement: Property,
    pub spacing: Property,
    pub sort_key: Property,
    pub z_order: Property,
    pub text_field: Property,
    pub text_size: Property,
    pub text_font: Property,
    pub text_max_width: Property,
    pub text_line_height: Property,
    pub text_letter_spacing: Property,
    pub text_anchor: Property,
    pub text_offset: Property,
    pub text_transform: Property,
    pub text_padding: Property,
    pub text_allow_overlap: Property,
    pub text_ignore_placement: Property,
    pub text_optional: Property,
    pub text_rotation_alignment: Property,
    pub text_max_angle: Property,
    pub text_justify: Property,
    pub text_color: Property,
    pub text_opacity: Property,
    pub text_halo_color: Property,
    pub text_halo_width: Property,
    pub icon_image: Property,
    pub icon_size: Property,
    pub icon_rotate: Property,
    pub icon_anchor: Property,
    pub icon_offset: Property,
    pub icon_padding: Property,
    pub icon_allow_overlap: Property,
    pub icon_ignore_placement: Property,
    pub icon_optional: Property,
    pub icon_rotation_alignment: Property,
    pub icon_opacity: Property,
}

#[derive(Clone, Debug)]
pub struct CircleProps {
    pub radius: Property,
    pub color: Property,
    pub opacity: Property,
    pub stroke_width: Property,
    pub stroke_color: Property,
    pub stroke_opacity: Property,
}

/// The spec's entry for a property: type, default, and whether it is data-driven.
fn spec(name: &str) -> PropertySpec {
    let number = |d: f64, data: bool| PropertySpec {
        kind: Type::Number,
        default: Some(Value::Number(d)),
        data_driven: data,
    };
    let color = |d: &str, data: bool| PropertySpec {
        kind: Type::Color,
        default: crate::expression::parse_color(d).map(Value::Color),
        data_driven: data,
    };
    let boolean = |d: bool, data: bool| PropertySpec {
        kind: Type::Boolean,
        default: Some(Value::Bool(d)),
        data_driven: data,
    };
    let token = |d: &str, values: &[&str], data: bool| PropertySpec {
        kind: Type::Enum(values.iter().map(|v| v.to_string()).collect()),
        default: Some(Value::String(d.to_string())),
        data_driven: data,
    };
    let numbers = |d: &[f64], data: bool| PropertySpec {
        kind: Type::Array(Some(Box::new(Type::Number)), None),
        default: Some(Value::Array(d.iter().map(|n| Value::Number(*n)).collect())),
        data_driven: data,
    };
    let anchors = [
        "center",
        "left",
        "right",
        "top",
        "bottom",
        "top-left",
        "top-right",
        "bottom-left",
        "bottom-right",
    ];
    let alignments = ["map", "viewport", "auto", "viewport-glyph"];
    match name {
        "background-color" => color("#000000", false),
        "background-opacity" => number(1.0, false),
        "fill-antialias" => boolean(true, false),
        "fill-opacity" => number(1.0, true),
        "fill-color" => color("#000000", true),
        "fill-outline-color" => PropertySpec {
            kind: Type::Color,
            default: None,
            data_driven: true,
        },
        "fill-pattern" | "line-pattern" | "icon-image" => PropertySpec {
            kind: Type::ResolvedImage,
            default: None,
            data_driven: true,
        },
        "line-opacity" => number(1.0, true),
        "line-color" => color("#000000", true),
        "line-width" => number(1.0, true),
        "line-gap-width" => number(0.0, true),
        "line-offset" => number(0.0, true),
        "line-dasharray" => PropertySpec {
            kind: Type::Array(Some(Box::new(Type::Number)), None),
            default: None,
            data_driven: true,
        },
        "line-cap" => token("butt", &["butt", "round", "square"], true),
        "line-join" => token("miter", &["bevel", "round", "miter"], true),
        "line-miter-limit" => number(2.0, true),
        "symbol-placement" => token("point", &["point", "line", "line-center"], false),
        "symbol-spacing" => number(250.0, false),
        "symbol-sort-key" => PropertySpec {
            kind: Type::Number,
            default: None,
            data_driven: true,
        },
        "symbol-z-order" => token("auto", &["auto", "viewport-y", "source"], false),
        "text-field" => PropertySpec {
            kind: Type::Formatted,
            default: Some(Value::String(String::new())),
            data_driven: true,
        },
        "text-size" => number(16.0, true),
        "text-font" => PropertySpec {
            kind: Type::Array(Some(Box::new(Type::String)), None),
            default: Some(Value::Array(vec![
                Value::String("Open Sans Regular".into()),
                Value::String("Arial Unicode MS Regular".into()),
            ])),
            data_driven: true,
        },
        "text-max-width" => number(10.0, true),
        "text-line-height" => number(1.2, false),
        "text-letter-spacing" => number(0.0, true),
        "text-anchor" | "icon-anchor" => token("center", &anchors, true),
        "text-offset" | "icon-offset" => numbers(&[0.0, 0.0], true),
        "text-transform" => token("none", &["none", "uppercase", "lowercase"], true),
        "text-padding" => number(2.0, false),
        "icon-padding" => number(2.0, true),
        "text-allow-overlap" | "icon-allow-overlap" => boolean(false, false),
        "text-ignore-placement" | "icon-ignore-placement" => boolean(false, false),
        "text-optional" | "icon-optional" => boolean(false, false),
        "text-rotation-alignment" => token("auto", &alignments, false),
        "icon-rotation-alignment" => token("auto", &alignments, true),
        "text-max-angle" => number(45.0, false),
        "text-justify" => token("center", &["auto", "left", "center", "right"], true),
        "text-color" | "icon-color" => color("#000000", true),
        "text-opacity" | "icon-opacity" => number(1.0, true),
        "text-halo-color" | "icon-halo-color" => color("rgba(0, 0, 0, 0)", true),
        "text-halo-width" | "icon-halo-width" => number(0.0, true),
        "icon-size" => number(1.0, true),
        "icon-rotate" => number(0.0, true),
        "circle-radius" => number(5.0, true),
        "circle-color" => color("#000000", true),
        "circle-opacity" => number(1.0, true),
        "circle-stroke-width" => number(0.0, true),
        "circle-stroke-color" => color("#000000", true),
        "circle-stroke-opacity" => number(1.0, true),
        _ => PropertySpec {
            kind: Type::Value,
            default: None,
            data_driven: true,
        },
    }
}

/// Where a layer's property is read from: `paint` or `layout`.
struct Props<'a> {
    layer: &'a str,
    paint: Option<&'a serde_json::Map<String, serde_json::Value>>,
    layout: Option<&'a serde_json::Map<String, serde_json::Value>>,
}

impl Props<'_> {
    fn get(&self, name: &str) -> Result<Property, Error> {
        let s = spec(name);
        let json = self
            .paint
            .and_then(|p| p.get(name))
            .or_else(|| self.layout.and_then(|l| l.get(name)));
        let default = s.default.clone().unwrap_or(Value::Null);
        let expr = match json {
            Some(json) => Expression::parse_property(json, &s)
                .map_err(|e| Error::Invalid(format!("layer {}: {name}: {e}", self.layer)))?,
            None => Expression::constant(default.clone()),
        };
        Ok(Property::new(expr, default))
    }
}

impl Style {
    /// Compile a style document.
    pub fn parse(json: &serde_json::Value) -> Result<Style, Error> {
        let root = json
            .as_object()
            .ok_or_else(|| Error::Invalid("style is not an object".into()))?;
        if root.get("version").and_then(|v| v.as_u64()) != Some(8) {
            return Err(Error::Invalid("style version must be 8".into()));
        }
        let name = root
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let sprite = match root.get("sprite") {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            // An array of `{id, url}`: the default one is what the icons name without a prefix.
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .find(|i| i.get("id").and_then(|v| v.as_str()) == Some("default"))
                .or(items.first())
                .and_then(|i| i.get("url"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
            _ => None,
        };
        let mut sources = BTreeMap::new();
        if let Some(map) = root.get("sources").and_then(|v| v.as_object()) {
            for (id, source) in map {
                sources.insert(id.clone(), parse_source(id, source)?);
            }
        }
        let mut layers = Vec::new();
        if let Some(list) = root.get("layers").and_then(|v| v.as_array()) {
            for layer in list {
                layers.push(parse_layer(layer)?);
            }
        }
        Ok(Style {
            name,
            sources,
            sprite,
            layers,
        })
    }

    /// Compile a style from its JSON text.
    pub fn parse_str(text: &str) -> Result<Style, Error> {
        let json: serde_json::Value =
            serde_json::from_str(text).map_err(|e| Error::Invalid(format!("style JSON: {e}")))?;
        Style::parse(&json)
    }

    /// The layer with this id.
    pub fn layer(&self, id: &str) -> Option<&Layer> {
        self.layers.iter().find(|l| l.id == id)
    }
}

/// Rewrite a style document so its labels prefer `name:<language>` where a feature has it,
/// falling back to what the style showed: MapLibre's own recipe for localizing a basemap. Every
/// symbol layer's `text-field` becomes `["coalesce", ["get", "name:<language>"], <original>]`,
/// with a legacy token string such as `"{name}"` converted to its expression first; a legacy
/// stop function is left alone, as is a field that names no feature property.
pub fn localize(style: &mut serde_json::Value, language: &str) {
    use serde_json::{Value as J, json};
    let language = language.trim();
    if language.is_empty() {
        return;
    }
    let Some(layers) = style.get_mut("layers").and_then(|l| l.as_array_mut()) else {
        return;
    };
    let key = format!("name:{language}");
    for layer in layers.iter_mut() {
        if layer.get("type").and_then(|t| t.as_str()) != Some("symbol") {
            continue;
        }
        let Some(field) = layer
            .get_mut("layout")
            .and_then(|l| l.as_object_mut())
            .and_then(|l| l.get_mut("text-field"))
        else {
            continue;
        };
        let original = match field {
            J::String(s) => {
                if !s.contains('{') {
                    continue;
                }
                token_expression(s)
            }
            J::Array(_) => field.clone(),
            _ => continue,
        };
        *field = json!(["coalesce", ["get", key], original]);
    }
}

/// A legacy token string (`"{name} ({ref})"`) as the expression it means.
fn token_expression(text: &str) -> serde_json::Value {
    use serde_json::{Value as J, json};
    let mut parts: Vec<J> = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        if let Some(close) = rest[open..].find('}') {
            if open > 0 {
                parts.push(J::String(rest[..open].to_string()));
            }
            let token = &rest[open + 1..open + close];
            parts.push(json!(["to-string", ["coalesce", ["get", token], ""]]));
            rest = &rest[open + close + 1..];
        } else {
            break;
        }
    }
    if !rest.is_empty() {
        parts.push(J::String(rest.to_string()));
    }
    match parts.len() {
        0 => J::String(String::new()),
        1 => parts.remove(0),
        _ => {
            let mut concat = vec![J::String("concat".into())];
            concat.extend(parts);
            J::Array(concat)
        }
    }
}

fn parse_source(id: &str, json: &serde_json::Value) -> Result<SourceSpec, Error> {
    let obj = json
        .as_object()
        .ok_or_else(|| Error::Invalid(format!("source {id} is not an object")))?;
    let kind = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if kind != "vector" {
        return Ok(SourceSpec::Unsupported(kind.to_string()));
    }
    let str_of = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let zoom_of = |k: &str| {
        obj.get(k)
            .and_then(|v| v.as_f64())
            .map(|z| z.clamp(0.0, 30.0) as u8)
    };
    let tiles = obj
        .get("tiles")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let bounds = obj.get("bounds").and_then(|v| v.as_array()).and_then(|b| {
        let n: Vec<f64> = b.iter().filter_map(|v| v.as_f64()).collect();
        (n.len() == 4).then(|| [n[0], n[1], n[2], n[3]])
    });
    Ok(SourceSpec::Vector {
        url: str_of("url"),
        tiles,
        minzoom: zoom_of("minzoom"),
        maxzoom: zoom_of("maxzoom"),
        bounds,
        scheme: str_of("scheme"),
        attribution: str_of("attribution"),
    })
}

fn parse_layer(json: &serde_json::Value) -> Result<Layer, Error> {
    let obj = json
        .as_object()
        .ok_or_else(|| Error::Invalid("layer is not an object".into()))?;
    let id = obj
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Invalid("layer without an id".into()))?
        .to_string();
    let kind = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let str_of = |k: &str| obj.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let paint = obj.get("paint").and_then(|v| v.as_object());
    let layout = obj.get("layout").and_then(|v| v.as_object());
    let visible = layout
        .and_then(|l| l.get("visibility"))
        .and_then(|v| v.as_str())
        != Some("none");
    let filter = match obj.get("filter") {
        Some(f) => Some(
            Expression::parse_filter(f)
                .map_err(|e| Error::Invalid(format!("layer {id}: filter: {e}")))?,
        ),
        None => None,
    };
    let props = Props {
        layer: &id,
        paint,
        layout,
    };
    let p = |name: &str| props.get(name);
    let kind = match kind {
        "background" => LayerKind::Background(Box::new(BackgroundProps {
            color: p("background-color")?,
            opacity: p("background-opacity")?,
        })),
        "fill" => LayerKind::Fill(Box::new(FillProps {
            color: p("fill-color")?,
            opacity: p("fill-opacity")?,
            outline_color: p("fill-outline-color")?,
            pattern: p("fill-pattern")?,
            antialias: p("fill-antialias")?,
        })),
        // Drawn flat, as MapLibre does at pitch zero: the extrusion color as a fill.
        "fill-extrusion" => {
            let extrusion = |name: &str| -> Result<Property, Error> {
                let s = spec(name);
                let json = paint.and_then(|p| p.get(&name.replace("fill-", "fill-extrusion-")));
                let default = s.default.clone().unwrap_or(Value::Null);
                let expr = match json {
                    Some(json) => Expression::parse_property(json, &s)
                        .map_err(|e| Error::Invalid(format!("layer {id}: {name}: {e}")))?,
                    None => Expression::constant(default.clone()),
                };
                Ok(Property::new(expr, default))
            };
            LayerKind::Fill(Box::new(FillProps {
                color: extrusion("fill-color")?,
                opacity: extrusion("fill-opacity")?,
                outline_color: p("fill-outline-color")?,
                pattern: p("fill-pattern")?,
                antialias: p("fill-antialias")?,
            }))
        }
        "line" => LayerKind::Line(Box::new(LineProps {
            color: p("line-color")?,
            width: p("line-width")?,
            gap_width: p("line-gap-width")?,
            opacity: p("line-opacity")?,
            dasharray: p("line-dasharray")?,
            cap: p("line-cap")?,
            join: p("line-join")?,
            miter_limit: p("line-miter-limit")?,
            offset: p("line-offset")?,
            pattern: p("line-pattern")?,
        })),
        "symbol" => LayerKind::Symbol(Box::new(SymbolProps {
            placement: p("symbol-placement")?,
            spacing: p("symbol-spacing")?,
            sort_key: p("symbol-sort-key")?,
            z_order: p("symbol-z-order")?,
            text_field: p("text-field")?,
            text_size: p("text-size")?,
            text_font: p("text-font")?,
            text_max_width: p("text-max-width")?,
            text_line_height: p("text-line-height")?,
            text_letter_spacing: p("text-letter-spacing")?,
            text_anchor: p("text-anchor")?,
            text_offset: p("text-offset")?,
            text_transform: p("text-transform")?,
            text_padding: p("text-padding")?,
            text_allow_overlap: p("text-allow-overlap")?,
            text_ignore_placement: p("text-ignore-placement")?,
            text_optional: p("text-optional")?,
            text_rotation_alignment: p("text-rotation-alignment")?,
            text_max_angle: p("text-max-angle")?,
            text_justify: p("text-justify")?,
            text_color: p("text-color")?,
            text_opacity: p("text-opacity")?,
            text_halo_color: p("text-halo-color")?,
            text_halo_width: p("text-halo-width")?,
            icon_image: p("icon-image")?,
            icon_size: p("icon-size")?,
            icon_rotate: p("icon-rotate")?,
            icon_anchor: p("icon-anchor")?,
            icon_offset: p("icon-offset")?,
            icon_padding: p("icon-padding")?,
            icon_allow_overlap: p("icon-allow-overlap")?,
            icon_ignore_placement: p("icon-ignore-placement")?,
            icon_optional: p("icon-optional")?,
            icon_rotation_alignment: p("icon-rotation-alignment")?,
            icon_opacity: p("icon-opacity")?,
        })),
        "circle" => LayerKind::Circle(Box::new(CircleProps {
            radius: p("circle-radius")?,
            color: p("circle-color")?,
            opacity: p("circle-opacity")?,
            stroke_width: p("circle-stroke-width")?,
            stroke_color: p("circle-stroke-color")?,
            stroke_opacity: p("circle-stroke-opacity")?,
        })),
        other => LayerKind::Unsupported(other.to_string()),
    };
    Ok(Layer {
        id,
        source: str_of("source"),
        source_layer: str_of("source-layer"),
        minzoom: obj.get("minzoom").and_then(|v| v.as_f64()).unwrap_or(0.0),
        maxzoom: obj.get("maxzoom").and_then(|v| v.as_f64()).unwrap_or(24.0),
        filter,
        visible,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Feature;

    const STYLE: &str = r##"{
      "version": 8,
      "name": "Fixture",
      "sprite": "https://example.test/sprite",
      "glyphs": "https://example.test/{fontstack}/{range}.pbf",
      "sources": {
        "osm": { "type": "vector", "url": "https://example.test/tiles.json" },
        "shade": { "type": "raster", "tiles": ["https://example.test/{z}/{x}/{y}.png"] }
      },
      "layers": [
        { "id": "bg", "type": "background", "paint": { "background-color": "#f8f4f0" } },
        { "id": "water", "type": "fill", "source": "osm", "source-layer": "water",
          "filter": ["==", "$type", "Polygon"],
          "paint": { "fill-color": "rgb(158,189,255)", "fill-opacity": 0.8 } },
        { "id": "roads", "type": "line", "source": "osm", "source-layer": "transportation",
          "layout": { "line-cap": "round", "visibility": "none" },
          "paint": { "line-color": "#fff", "line-width": { "base": 1.4, "stops": [[6, 0.5], [20, 30]] } } },
        { "id": "hills", "type": "hillshade", "source": "shade" }
      ]
    }"##;

    #[test]
    fn a_style_compiles_its_sources_and_layers() {
        let style = Style::parse_str(STYLE).unwrap();
        assert_eq!(style.name.as_deref(), Some("Fixture"));
        assert_eq!(style.sprite.as_deref(), Some("https://example.test/sprite"));
        assert_eq!(style.sources.len(), 2);
        assert!(matches!(style.sources["osm"], SourceSpec::Vector { .. }));
        assert_eq!(
            style.sources["shade"],
            SourceSpec::Unsupported("raster".into())
        );
        assert_eq!(style.layers.len(), 4);
        assert!(matches!(style.layers[0].kind, LayerKind::Background(_)));
        assert!(!style.layers[2].visible);
        assert!(matches!(
            style.layers[3].kind,
            LayerKind::Unsupported(ref k) if k == "hillshade"
        ));
        let water = style.layer("water").unwrap();
        assert_eq!(water.source_layer.as_deref(), Some("water"));
        assert!(water.filter.is_some());
    }

    #[test]
    fn properties_answer_typed_values_with_defaults() {
        let style = Style::parse_str(STYLE).unwrap();
        let ctx = Context {
            zoom: 10.0,
            ..Context::default()
        };
        let LayerKind::Fill(fill) = &style.layer("water").unwrap().kind else {
            panic!("water is a fill")
        };
        let c = fill.color.color(&ctx);
        assert!((c.r - 158.0 / 255.0).abs() < 1e-9 && (c.b - 1.0).abs() < 1e-9);
        assert_eq!(fill.opacity.number(&ctx), 0.8);
        assert!(!fill.antialias.is_set());
        assert!(fill.antialias.boolean(&ctx));
        assert!(fill.pattern.image(&ctx).is_none());
        let LayerKind::Line(line) = &style.layer("roads").unwrap().kind else {
            panic!("roads is a line")
        };
        assert_eq!(line.cap.token(&ctx), "round");
        assert_eq!(line.join.token(&ctx), "miter");
        assert!(line.width.is_feature_constant());
        assert!(line.dasharray.numbers(&ctx).is_empty());
        assert_eq!(line.gap_width.number(&ctx), 0.0);
        let feature = Feature {
            id: None,
            kind: crate::GeometryType::Polygon,
            properties: BTreeMap::new(),
            geometry: Vec::new(),
        };
        let with = Context {
            feature: Some(&feature),
            ..ctx
        };
        assert_eq!(fill.opacity.number(&with), 0.8);
    }

    #[test]
    fn localizing_prefers_the_language_and_keeps_the_fallback() {
        let mut json: serde_json::Value = serde_json::from_str(
            r##"{"version": 8, "sources": {}, "layers": [
              {"id": "a", "type": "symbol", "layout": {"text-field": ["get", "name"]}},
              {"id": "b", "type": "symbol", "layout": {"text-field": "{name} {ref}"}},
              {"id": "c", "type": "symbol", "layout": {"text-field": "Fixed"}},
              {"id": "d", "type": "line"}
            ]}"##,
        )
        .unwrap();
        localize(&mut json, "fr");
        let field = |i: usize| json["layers"][i]["layout"]["text-field"].clone();
        assert_eq!(field(0)[0], "coalesce");
        assert_eq!(field(0)[1], serde_json::json!(["get", "name:fr"]));
        assert_eq!(field(0)[2], serde_json::json!(["get", "name"]));
        assert_eq!(field(1)[2][0], "concat");
        assert_eq!(field(2), "Fixed");
        let style = Style::parse(&json).unwrap();
        let mut props = BTreeMap::new();
        props.insert("name".to_string(), Value::String("London".into()));
        props.insert("name:fr".to_string(), Value::String("Londres".into()));
        let feature = Feature {
            id: None,
            kind: crate::GeometryType::Point,
            properties: props,
            geometry: Vec::new(),
        };
        let ctx = Context {
            zoom: 10.0,
            feature: Some(&feature),
            ..Context::default()
        };
        let LayerKind::Symbol(p) = &style.layer("a").unwrap().kind else {
            panic!("a is a symbol layer")
        };
        assert_eq!(p.text_field.text(&ctx), "Londres");
    }

    #[test]
    fn bad_documents_are_rejected_with_a_reason() {
        assert!(matches!(
            Style::parse_str("[]"),
            Err(Error::Invalid(ref m)) if m.contains("object")
        ));
        assert!(matches!(
            Style::parse_str(r#"{"version": 7}"#),
            Err(Error::Invalid(ref m)) if m.contains("version")
        ));
        assert!(matches!(
            Style::parse_str(r#"{"version": 8, "layers": [{"type": "fill"}]}"#),
            Err(Error::Invalid(ref m)) if m.contains("id")
        ));
    }
}
