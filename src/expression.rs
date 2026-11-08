// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! MapLibre style expressions (<https://maplibre.org/maplibre-style-spec/expressions/>): parsed
//! from their JSON array form (or a legacy function or filter) into a tree, then evaluated
//! against a zoom and a feature.
//!
//! The design follows `maplibre-style-spec/src/expression`: parse checks types and folds
//! constants, so a layer's filter and properties are compiled once when the style loads and
//! evaluated per feature per frame with no JSON in the loop. The test fixtures under
//! `tests/data/expression` are MapLibre's own, run by `tests/expression.rs`.
//!
//! Not implemented (they parse to [`Error::Unsupported`]): `within` and `distance` (GeoJSON
//! geometry against tile geometry), `heatmap-density`, `elevation`, `line-progress` and
//! `accumulated` (globals this crate has no source for), `is-supported-script` and
//! `resolved-locale` (environment lookups). `collator` and `number-format` are hand-rolled
//! approximations of ICU: Latin diacritics and case folding, `en`-style digit grouping with a
//! small currency and unit table.

use crate::{Error, Feature, FormattedSection, Value};
use day_geometry::Color;
use serde_json::Value as Json;
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// The type an expression produces, as the style spec names them.
#[derive(Clone, Debug, PartialEq)]
pub enum Type {
    /// The type of the `null` literal.
    Null,
    /// The type of the `error` expression, which never produces a value: a subtype of every
    /// type.
    Error,
    Number,
    String,
    Boolean,
    Color,
    Object,
    Value,
    Formatted,
    ResolvedImage,
    Collator,
    /// `array`, `array<T>` or `array<T, N>`. `None` for the item type means `value`.
    Array(Option<Box<Type>>, Option<usize>),
    /// A number or a string, in the enum sense the spec uses for layout properties.
    Enum(Vec<String>),
}

impl Type {
    /// `array<item, length>`.
    pub fn array(item: Type, length: Option<usize>) -> Type {
        Type::Array(Some(Box::new(item)), length)
    }

    /// The spec's spelling of the type: `number`, `array<string, 2>`, …
    pub fn name(&self) -> String {
        match self {
            Type::Null => "null".into(),
            Type::Error => "error".into(),
            Type::Number => "number".into(),
            Type::String | Type::Enum(_) => "string".into(),
            Type::Boolean => "boolean".into(),
            Type::Color => "color".into(),
            Type::Object => "object".into(),
            Type::Value => "value".into(),
            Type::Formatted => "formatted".into(),
            Type::ResolvedImage => "resolvedImage".into(),
            Type::Collator => "collator".into(),
            Type::Array(_, length) => {
                let item = self.item();
                match length {
                    Some(n) => format!("array<{}, {n}>", item.name()),
                    None if *item == Type::Value => "array".into(),
                    None => format!("array<{}>", item.name()),
                }
            }
        }
    }

    fn item(&self) -> &Type {
        match self {
            Type::Array(Some(item), _) => item,
            _ => &Type::Value,
        }
    }

    fn length(&self) -> Option<usize> {
        match self {
            Type::Array(_, n) => *n,
            _ => None,
        }
    }

    fn tag(&self) -> u8 {
        match self {
            Type::Null => 0,
            Type::Error => 11,
            Type::Number => 1,
            Type::String | Type::Enum(_) => 2,
            Type::Boolean => 3,
            Type::Color => 4,
            Type::Object => 5,
            Type::Value => 6,
            Type::Formatted => 7,
            Type::ResolvedImage => 8,
            Type::Collator => 9,
            Type::Array(..) => 10,
        }
    }

    /// Same kind, ignoring array parameters (an enum is a string).
    fn same_kind(&self, other: &Type) -> bool {
        self.tag() == other.tag()
    }
}

impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name())
    }
}

/// `None` if `actual` is a subtype of `expected`; otherwise the spec's error message.
fn check_subtype(expected: &Type, actual: &Type) -> Option<String> {
    let ok = match expected {
        _ if *actual == Type::Error => true,
        Type::Array(_, expected_len) => match actual {
            Type::Array(_, actual_len) => {
                let items_ok = (*actual_len == Some(0) && *actual.item() == Type::Value)
                    || check_subtype(expected.item(), actual.item()).is_none();
                items_ok && (expected_len.is_none() || expected_len == actual_len)
            }
            _ => false,
        },
        _ if expected.same_kind(actual) => true,
        Type::Value => value_members()
            .iter()
            .any(|member| check_subtype(member, actual).is_none()),
        _ => false,
    };
    if ok {
        None
    } else {
        Some(format!("Expected {expected} but found {actual} instead."))
    }
}

/// The types that are members of `value`.
fn value_members() -> [Type; 9] {
    [
        Type::Null,
        Type::Number,
        Type::String,
        Type::Boolean,
        Type::Color,
        Type::Formatted,
        Type::Object,
        Type::array(Type::Value, None),
        Type::ResolvedImage,
    ]
}

/// What a style property accepts, from the spec's reference: used to parse legacy functions
/// (which need the output type and the default), and to type-check an expression.
#[derive(Clone, Debug)]
pub struct PropertySpec {
    /// The property's type; `Type::Value` leaves the expression untyped.
    pub kind: Type,
    pub default: Option<Value>,
    /// Whether the property may read the feature (`data-driven`); a `zoom`-only property
    /// rejects feature expressions at parse time.
    pub data_driven: bool,
}

impl PropertySpec {
    /// The legacy-function hints the reference derives from the property: numbers, colors and
    /// number arrays interpolate, formatted and image properties carry tokens. Pass explicit [`FunctionHints`] to
    /// [`Expression::parse_property_with`] where the reference says otherwise.
    pub fn function_hints(&self) -> FunctionHints {
        let interpolated = match &self.kind {
            Type::Number | Type::Color => true,
            Type::Array(..) => *self.kind.item() == Type::Number,
            _ => false,
        };
        FunctionHints {
            interpolated,
            // `text-field`, `icon-image`, `fill-pattern`, `line-pattern`: the string-typed
            // properties whose legacy strings carry `{name}` tokens.
            tokens: matches!(self.kind, Type::Formatted | Type::ResolvedImage),
        }
    }
}

/// What a legacy function needs to know about its property beyond the [`PropertySpec`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FunctionHints {
    /// Whether the property interpolates between stops (`expression.interpolated` in the
    /// reference); a function without a `type` is `exponential` when it does and `interval`
    /// when it does not.
    pub interpolated: bool,
    /// Whether the property's strings carry `{name}` tokens (`text-field`, `icon-image`);
    /// zoom functions on such properties turn them into `concat`/`get` expressions.
    pub tokens: bool,
}

/// What an expression is evaluated against.
#[derive(Clone, Copy, Debug, Default)]
pub struct Context<'a> {
    pub zoom: f64,
    pub feature: Option<&'a Feature>,
    pub feature_state: Option<&'a BTreeMap<String, Value>>,
    pub global_state: Option<&'a BTreeMap<String, Value>>,
    /// The tile the feature came from, for `distance` and `within`.
    pub tile: Option<crate::TileId>,
    /// The sprite's image names, for `image` to say whether one is available.
    pub available_images: Option<&'a [String]>,
}

// ---------------------------------------------------------------------------------------------
// Values: typing, JavaScript-compatible string forms, equality
// ---------------------------------------------------------------------------------------------

/// The type of a runtime value, as `typeof` reports it.
fn type_of(value: &Value) -> Type {
    match value {
        Value::Null => Type::Null,
        Value::Bool(_) => Type::Boolean,
        Value::Number(_) => Type::Number,
        Value::String(_) => Type::String,
        Value::Color(_) => Type::Color,
        Value::Object(_) => Type::Object,
        Value::Formatted(_) => Type::Formatted,
        Value::Image { .. } => Type::ResolvedImage,
        Value::Collator { .. } => Type::Collator,
        Value::Array(items) => {
            // The reference compares item types by object identity, so two array-typed items
            // never agree with each other.
            let mut item_type: Option<Type> = None;
            for item in items {
                let t = type_of(item);
                match &item_type {
                    None => item_type = Some(t),
                    Some(current) if *current == t && !matches!(t, Type::Array(..)) => {}
                    Some(_) => {
                        item_type = Some(Type::Value);
                        break;
                    }
                }
            }
            Type::array(item_type.unwrap_or(Type::Value), Some(items.len()))
        }
    }
}

/// A number as JavaScript's `String(n)` spells it: integers without a fraction, exponent form
/// below 1e-6 and from 1e21.
fn js_number(n: f64) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if n == 0.0 {
        return "0".into();
    }
    let abs = n.abs();
    if n.fract() == 0.0 && abs < 1e21 {
        return format!("{n:.0}");
    }
    if (1e-6..1e21).contains(&abs) {
        return format!("{n}");
    }
    let s = format!("{n:e}");
    match s.split_once('e') {
        Some((mantissa, exponent)) if !exponent.starts_with('-') => {
            format!("{mantissa}e+{exponent}")
        }
        _ => s,
    }
}

/// The straight (not premultiplied) components as `to-string` prints a color:
/// `rgba(r,g,b,a)` with 0..255 channels.
fn color_to_string(c: Color) -> String {
    let channel = |x: f64| ((x * 255.0) + 0.5).floor();
    format!(
        "rgba({},{},{},{})",
        js_number(channel(c.r)),
        js_number(channel(c.g)),
        js_number(channel(c.b)),
        js_number(c.a)
    )
}

/// `to-string`'s rendering of a value (`null` is the empty string).
fn value_to_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => js_number(*n),
        Value::String(s) => s.clone(),
        Value::Color(c) => color_to_string(*c),
        Value::Formatted(sections) => sections.iter().map(|s| s.text.as_str()).collect(),
        Value::Image { name, .. } => name.clone(),
        Value::Collator { .. } | Value::Array(_) | Value::Object(_) => json_text(value),
    }
}

/// JavaScript's `String(v)`, which differs from `to-string` only for `null`.
fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        other => value_to_string(other),
    }
}

/// `JSON.stringify(value)`.
fn json_text(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) if n.is_finite() => js_number(*n),
        Value::Number(_) => "null".into(),
        Value::String(s) => serde_json::to_string(s).unwrap_or_default(),
        Value::Color(c) => format!(
            "{{\"r\":{},\"g\":{},\"b\":{},\"a\":{}}}",
            js_number(c.r * c.a),
            js_number(c.g * c.a),
            js_number(c.b * c.a),
            js_number(c.a)
        ),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(json_text).collect();
            format!("[{}]", inner.join(","))
        }
        Value::Object(map) => {
            let inner: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap_or_default(),
                        json_text(v)
                    )
                })
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        Value::Formatted(_) => serde_json::to_string(&value_to_string(value)).unwrap_or_default(),
        Value::Image { name, available } => format!(
            "{{\"name\":{},\"available\":{available}}}",
            serde_json::to_string(name).unwrap_or_default()
        ),
        Value::Collator { .. } => "{}".into(),
    }
}

/// JavaScript strict equality: primitives by value, everything else by identity (never equal).
fn strict_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Number(x), Value::Number(y)) => x == y,
        (Value::String(x), Value::String(y)) => x == y,
        _ => false,
    }
}

/// JavaScript truthiness.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => *n != 0.0 && !n.is_nan(),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// JavaScript's `Number(value)` (`NaN` where it cannot convert).
fn js_to_number(value: &Value) -> f64 {
    match value {
        Value::Null => 0.0,
        Value::Bool(b) => f64::from(*b),
        Value::Number(n) => *n,
        Value::String(s) => js_parse_number(s),
        Value::Array(items) => match items.as_slice() {
            [] => 0.0,
            [item] => js_parse_number(&js_string(item)),
            _ => f64::NAN,
        },
        _ => f64::NAN,
    }
}

fn js_parse_number(text: &str) -> f64 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return 0.0;
    }
    match trimmed {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    if trimmed.eq_ignore_ascii_case("nan")
        || trimmed.eq_ignore_ascii_case("inf")
        || trimmed.eq_ignore_ascii_case("infinity")
    {
        return f64::NAN;
    }
    if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        return u64::from_str_radix(hex, 16).map_or(f64::NAN, |v| v as f64);
    }
    trimmed.parse::<f64>().unwrap_or(f64::NAN)
}

fn is_primitive(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

/// JavaScript's `typeof` of a JSON value, for error messages.
fn js_typeof(json: &Json) -> &'static str {
    match json {
        Json::Bool(_) => "boolean",
        Json::Number(_) => "number",
        Json::String(_) => "string",
        _ => "object",
    }
}

/// An evaluation failure: the message and the index path of the failing node (`[2][1]`).
#[derive(Clone, Debug)]
struct RuntimeError {
    key: String,
    message: String,
}

impl RuntimeError {
    fn new(key: &str, message: impl Into<String>) -> RuntimeError {
        RuntimeError {
            key: key.into(),
            message: message.into(),
        }
    }
}

/// A parse failure at an index path.
#[derive(Clone, Debug)]
struct ParseError {
    key: String,
    message: String,
    unsupported: bool,
}

// ---------------------------------------------------------------------------------------------
// The expression tree
// ---------------------------------------------------------------------------------------------

/// A compiled node: its static type, its index path (for error messages) and what it does.
#[derive(Clone, Debug)]
struct Expr {
    ty: Type,
    key: String,
    kind: Kind,
}

/// The curve of an `interpolate`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Interpolation {
    Linear,
    Exponential(f64),
    CubicBezier(f64, f64, f64, f64),
}

/// Which color space an `interpolate` blends colors in.
#[derive(Clone, Copy, Debug, PartialEq)]
enum ColorSpace {
    Rgb,
    Hcl,
    Lab,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum CmpOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

impl CmpOp {
    fn symbol(self) -> &'static str {
        match self {
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Gt => ">",
            CmpOp::Le => "<=",
            CmpOp::Ge => ">=",
        }
    }

    fn is_order(self) -> bool {
        !matches!(self, CmpOp::Eq | CmpOp::Ne)
    }

    fn holds(self, ordering: Ordering) -> bool {
        match self {
            CmpOp::Eq => ordering == Ordering::Equal,
            CmpOp::Ne => ordering != Ordering::Equal,
            CmpOp::Lt => ordering == Ordering::Less,
            CmpOp::Gt => ordering == Ordering::Greater,
            CmpOp::Le => ordering != Ordering::Greater,
            CmpOp::Ge => ordering != Ordering::Less,
        }
    }
}

/// One section of a `format` expression.
#[derive(Clone, Debug)]
struct FormatSection {
    content: Expr,
    scale: Option<Expr>,
    font: Option<Expr>,
    text_color: Option<Expr>,
    vertical_align: Option<Expr>,
}

/// The arguments of a `number-format` expression.
#[derive(Clone, Debug)]
struct NumberFormatArgs {
    number: Expr,
    currency: Option<Expr>,
    unit: Option<Expr>,
    min_fraction_digits: Option<Expr>,
    max_fraction_digits: Option<Expr>,
}

/// The "compound" operators: a fixed signature (or overloads) and a plain evaluation function.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Op {
    /// `error`: fails with its argument as the message.
    Error,
    Typeof,
    ToRgba,
    Rgb,
    Rgba,
    Has,
    Get,
    FeatureState,
    Properties,
    GeometryType,
    Id,
    Zoom,
    Add,
    Mul,
    Sub,
    Div,
    Mod,
    Ln2,
    Pi,
    E,
    Pow,
    Sqrt,
    Log10,
    Ln,
    Log2,
    Sin,
    Cos,
    Tan,
    Asin,
    Acos,
    Atan,
    Min,
    Max,
    Abs,
    Round,
    Floor,
    Ceil,
    All,
    Any,
    Not,
    Upcase,
    Downcase,
    Concat,
    Split,
    Join,
}

#[derive(Clone, Debug)]
enum Kind {
    Literal(Value),
    /// `array`/`boolean`/`number`/`string`/`object`: the first argument of the node's type.
    Assert(Vec<Expr>),
    /// `to-*`: the first argument convertible to the node's type.
    Coerce(Vec<Expr>),
    Compound(Op, Vec<Expr>),
    Let(Vec<(String, Expr)>, Box<Expr>),
    /// A `var`: the expression bound to the name in the enclosing `let`.
    Var(Box<Expr>),
    At(Box<Expr>, Box<Expr>),
    In(Box<Expr>, Box<Expr>),
    IndexOf(Box<Expr>, Box<Expr>, Option<Box<Expr>>),
    Slice(Box<Expr>, Box<Expr>, Option<Box<Expr>>),
    Length(Box<Expr>),
    Case(Vec<(Expr, Expr)>, Box<Expr>),
    Coalesce(Vec<Expr>),
    Match {
        input: Box<Expr>,
        input_type: Type,
        cases: BTreeMap<String, usize>,
        outputs: Vec<Expr>,
        otherwise: Box<Expr>,
    },
    Step {
        input: Box<Expr>,
        labels: Vec<f64>,
        outputs: Vec<Expr>,
    },
    Interpolate {
        space: ColorSpace,
        interpolation: Interpolation,
        input: Box<Expr>,
        labels: Vec<f64>,
        outputs: Vec<Expr>,
    },
    Compare {
        op: CmpOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        collator: Option<Box<Expr>>,
        /// Either side was statically a `value`, so the types are checked at run time.
        untyped: bool,
    },
    Collator {
        case_sensitive: Box<Expr>,
        diacritic_sensitive: Box<Expr>,
        locale: Option<Box<Expr>>,
    },
    NumberFormat(Box<NumberFormatArgs>),
    Format(Vec<FormatSection>),
    Image(Box<Expr>),
    GlobalState(String),
    Semiliteral(Vec<Expr>),
}

impl Expr {
    fn literal(ty: Type, key: &str, value: Value) -> Expr {
        Expr {
            ty,
            key: key.into(),
            kind: Kind::Literal(value),
        }
    }

    fn is_literal(&self) -> bool {
        matches!(self.kind, Kind::Literal(_))
    }

    /// The direct children, in the reference's `eachChild` order (a `var` has none).
    fn children(&self) -> Vec<&Expr> {
        match &self.kind {
            Kind::Literal(_) | Kind::Var(..) | Kind::GlobalState(_) => Vec::new(),
            Kind::Assert(args)
            | Kind::Coerce(args)
            | Kind::Compound(_, args)
            | Kind::Coalesce(args)
            | Kind::Semiliteral(args) => args.iter().collect(),
            Kind::Let(bindings, result) => bindings
                .iter()
                .map(|(_, e)| e)
                .chain(std::iter::once(&**result))
                .collect(),
            Kind::At(a, b) | Kind::In(a, b) => vec![a, b],
            Kind::IndexOf(a, b, c) | Kind::Slice(a, b, c) => {
                let mut out: Vec<&Expr> = vec![a, b];
                out.extend(c.as_deref());
                out
            }
            Kind::Length(a) | Kind::Image(a) => vec![a],
            Kind::Case(branches, otherwise) => branches
                .iter()
                .flat_map(|(t, r)| [t, r])
                .chain(std::iter::once(&**otherwise))
                .collect(),
            Kind::Match {
                input,
                outputs,
                otherwise,
                ..
            } => std::iter::once(&**input)
                .chain(outputs.iter())
                .chain(std::iter::once(&**otherwise))
                .collect(),
            Kind::Step { input, outputs, .. } | Kind::Interpolate { input, outputs, .. } => {
                std::iter::once(&**input).chain(outputs.iter()).collect()
            }
            Kind::Compare {
                lhs, rhs, collator, ..
            } => {
                let mut out: Vec<&Expr> = vec![lhs, rhs];
                out.extend(collator.as_deref());
                out
            }
            Kind::Collator {
                case_sensitive,
                diacritic_sensitive,
                locale,
            } => {
                let mut out: Vec<&Expr> = vec![case_sensitive, diacritic_sensitive];
                out.extend(locale.as_deref());
                out
            }
            Kind::NumberFormat(args) => {
                let mut out = vec![&args.number];
                out.extend(args.currency.iter());
                out.extend(args.unit.iter());
                out.extend(args.min_fraction_digits.iter());
                out.extend(args.max_fraction_digits.iter());
                out
            }
            Kind::Format(sections) => sections
                .iter()
                .flat_map(|s| {
                    std::iter::once(&s.content)
                        .chain(s.scale.iter())
                        .chain(s.font.iter())
                        .chain(s.text_color.iter())
                        .chain(s.vertical_align.iter())
                })
                .collect(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Static analysis: constant folding, feature and zoom dependence, the zoom curve rule
// ---------------------------------------------------------------------------------------------

/// Whether the node can be evaluated at parse time and replaced by its value.
fn is_constant(e: &Expr) -> bool {
    match &e.kind {
        Kind::Var(bound) => return is_constant(bound),
        // A collator's results depend on the environment; global state changes at run time;
        // an error is for the evaluation that reaches it.
        Kind::Collator { .. } | Kind::GlobalState(_) | Kind::Compound(Op::Error, _) => {
            return false;
        }
        _ => {}
    }
    // Children of an op were folded already if they were constant, so a non-literal child is a
    // non-constant one; type annotations are added after their child was parsed, so recurse.
    let annotation = matches!(e.kind, Kind::Assert(_) | Kind::Coerce(_));
    let children_constant = e.children().into_iter().all(|child| {
        if annotation {
            is_constant(child)
        } else {
            child.is_literal()
        }
    });
    children_constant && is_feature_constant(e) && is_zoom_constant(e)
}

fn is_feature_constant(e: &Expr) -> bool {
    if let Kind::Compound(op, args) = &e.kind {
        match op {
            Op::Get | Op::Has if args.len() == 1 => return false,
            Op::FeatureState | Op::Properties | Op::GeometryType | Op::Id => return false,
            _ => {}
        }
    }
    e.children().into_iter().all(is_feature_constant)
}

fn is_zoom_constant(e: &Expr) -> bool {
    if matches!(e.kind, Kind::Compound(Op::Zoom, _)) {
        return false;
    }
    e.children().into_iter().all(is_zoom_constant)
}

const ZOOM_TOP_LEVEL: &str = "\"zoom\" expression may only be used as input to a top-level \"step\" or \"interpolate\" expression.";
const ZOOM_ONLY_ONE: &str =
    "Only one zoom-based \"step\" or \"interpolate\" subexpression may be used in an expression.";

/// The one `step`/`interpolate` over `["zoom"]` an expression may have, at its top level or
/// through `let` and `coalesce`; an error where zoom is used anywhere else.
fn find_zoom_curve(e: &Expr) -> Result<Option<&Expr>, String> {
    let direct = match &e.kind {
        Kind::Let(_, result) => find_zoom_curve(result)?,
        Kind::Coalesce(args) => {
            let mut found = None;
            for arg in args {
                found = find_zoom_curve(arg)?;
                if found.is_some() {
                    break;
                }
            }
            found
        }
        Kind::Step { input, .. } | Kind::Interpolate { input, .. }
            if matches!(input.kind, Kind::Compound(Op::Zoom, _)) =>
        {
            Some(e)
        }
        _ => None,
    };
    let mut state: Result<Option<&Expr>, String> = Ok(direct);
    for child in e.children() {
        match (&state, find_zoom_curve(child)) {
            (_, Err(err)) => state = Err(err),
            (Ok(None), Ok(Some(_))) => state = Err(ZOOM_TOP_LEVEL.into()),
            (Ok(Some(found)), Ok(Some(child_found))) if !std::ptr::eq(*found, child_found) => {
                state = Err(ZOOM_ONLY_ONE.into())
            }
            (Err(_), Ok(Some(_))) => state = Err(ZOOM_ONLY_ONE.into()),
            _ => {}
        }
    }
    state
}

// ---------------------------------------------------------------------------------------------
// Compound operator signatures
// ---------------------------------------------------------------------------------------------

enum Sig {
    Fixed(Vec<Type>),
    Varargs(Type),
}

impl Sig {
    fn describe(&self) -> String {
        match self {
            Sig::Fixed(params) => {
                let names: Vec<String> = params.iter().map(Type::name).collect();
                format!("({})", names.join(", "))
            }
            Sig::Varargs(t) => format!("({}...)", t.name()),
        }
    }

    fn param(&self, i: usize) -> &Type {
        match self {
            Sig::Fixed(params) => &params[i],
            Sig::Varargs(t) => t,
        }
    }
}

struct Def {
    ty: Type,
    overloads: Vec<(Sig, Op)>,
}

fn definition(name: &str) -> Option<Def> {
    use Type::{Boolean, Number, Object, String, Value};
    let fixed = |params: &[Type]| Sig::Fixed(params.to_vec());
    let one = |ty: Type, sig: Sig, op: Op| Def {
        ty,
        overloads: vec![(sig, op)],
    };
    let math1 = |op: Op| one(Number, fixed(&[Number]), op);
    let math2 = |op: Op| one(Number, fixed(&[Number, Number]), op);
    let math0 = |op: Op| one(Number, fixed(&[]), op);
    Some(match name {
        "error" => one(Type::Error, fixed(&[String]), Op::Error),
        "typeof" => one(String, fixed(&[Value]), Op::Typeof),
        "to-rgba" => one(
            Type::array(Number, Some(4)),
            fixed(&[Type::Color]),
            Op::ToRgba,
        ),
        "rgb" => one(Type::Color, fixed(&[Number, Number, Number]), Op::Rgb),
        "rgba" => one(
            Type::Color,
            fixed(&[Number, Number, Number, Number]),
            Op::Rgba,
        ),
        "has" => Def {
            ty: Boolean,
            overloads: vec![
                (fixed(&[String]), Op::Has),
                (fixed(&[String, Object]), Op::Has),
            ],
        },
        "get" => Def {
            ty: Value,
            overloads: vec![
                (fixed(&[String]), Op::Get),
                (fixed(&[String, Object]), Op::Get),
            ],
        },
        "feature-state" => one(Value, fixed(&[String]), Op::FeatureState),
        "properties" => one(Object, fixed(&[]), Op::Properties),
        "geometry-type" => one(String, fixed(&[]), Op::GeometryType),
        "id" => one(Value, fixed(&[]), Op::Id),
        "zoom" => math0(Op::Zoom),
        "+" => one(Number, Sig::Varargs(Number), Op::Add),
        "*" => one(Number, Sig::Varargs(Number), Op::Mul),
        "-" => Def {
            ty: Number,
            overloads: vec![
                (fixed(&[Number, Number]), Op::Sub),
                (fixed(&[Number]), Op::Sub),
            ],
        },
        "/" => math2(Op::Div),
        "%" => math2(Op::Mod),
        "ln2" => math0(Op::Ln2),
        "pi" => math0(Op::Pi),
        "e" => math0(Op::E),
        "^" => math2(Op::Pow),
        "sqrt" => math1(Op::Sqrt),
        "log10" => math1(Op::Log10),
        "ln" => math1(Op::Ln),
        "log2" => math1(Op::Log2),
        "sin" => math1(Op::Sin),
        "cos" => math1(Op::Cos),
        "tan" => math1(Op::Tan),
        "asin" => math1(Op::Asin),
        "acos" => math1(Op::Acos),
        "atan" => math1(Op::Atan),
        "min" => one(Number, Sig::Varargs(Number), Op::Min),
        "max" => one(Number, Sig::Varargs(Number), Op::Max),
        "abs" => math1(Op::Abs),
        "round" => math1(Op::Round),
        "floor" => math1(Op::Floor),
        "ceil" => math1(Op::Ceil),
        "all" => Def {
            ty: Boolean,
            overloads: vec![
                (fixed(&[Boolean, Boolean]), Op::All),
                (Sig::Varargs(Boolean), Op::All),
            ],
        },
        "any" => Def {
            ty: Boolean,
            overloads: vec![
                (fixed(&[Boolean, Boolean]), Op::Any),
                (Sig::Varargs(Boolean), Op::Any),
            ],
        },
        "!" => one(Boolean, fixed(&[Boolean]), Op::Not),
        "upcase" => one(String, fixed(&[String]), Op::Upcase),
        "downcase" => one(String, fixed(&[String]), Op::Downcase),
        "concat" => one(String, Sig::Varargs(Value), Op::Concat),
        "split" => one(
            Type::array(String, None),
            fixed(&[String, String]),
            Op::Split,
        ),
        "join" => one(
            String,
            fixed(&[Type::array(String, None), String]),
            Op::Join,
        ),
        _ => return None,
    })
}

/// The special forms the parser handles outside the compound table.
const SPECIAL_FORMS: &[&str] = &[
    "literal",
    "array",
    "boolean",
    "number",
    "string",
    "object",
    "to-boolean",
    "to-color",
    "to-number",
    "to-string",
    "==",
    "!=",
    "<",
    ">",
    "<=",
    ">=",
    "at",
    "in",
    "index-of",
    "slice",
    "length",
    "let",
    "var",
    "case",
    "coalesce",
    "match",
    "step",
    "interpolate",
    "interpolate-hcl",
    "interpolate-lab",
    "collator",
    "number-format",
    "format",
    "image",
    "global-state",
    "semiliteral",
];

/// Whether `json` is written as an expression: an array whose first element names an operator
/// (as opposed to a constant array such as `["Open Sans"]`).
pub fn is_expression(json: &Json) -> bool {
    json.as_array()
        .and_then(|a| a.first())
        .and_then(Json::as_str)
        .is_some_and(|name| {
            SPECIAL_FORMS.contains(&name)
                || UNSUPPORTED.contains(&name)
                || definition(name).is_some()
        })
}

const UNSUPPORTED: &[&str] = &[
    "within",
    "distance",
    "heatmap-density",
    "elevation",
    "line-progress",
    "accumulated",
    "is-supported-script",
    "resolved-locale",
];

// ---------------------------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------------------------

/// How a parsed node is adapted to the expected type when it is statically only a `value`.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Annotation {
    Assert,
    Coerce,
    Omit,
}

struct Parser {
    path: Vec<usize>,
    /// `let` bindings in scope, innermost last.
    scope: Vec<(String, Expr)>,
    errors: Vec<ParseError>,
}

impl Parser {
    fn key(&self) -> String {
        self.path.iter().map(|i| format!("[{i}]")).collect()
    }

    fn error(&mut self, message: impl Into<String>, extra: &[usize]) {
        let key: String = self.key() + &extra.iter().map(|i| format!("[{i}]")).collect::<String>();
        self.errors.push(ParseError {
            key,
            message: message.into(),
            unsupported: false,
        });
    }

    fn unsupported(&mut self, what: &str) {
        self.errors.push(ParseError {
            key: self.key(),
            message: format!("{what} expression"),
            unsupported: true,
        });
    }

    /// Parse `json` as the child at `index` (none for the root) that must produce `expected`.
    fn parse(
        &mut self,
        json: &Json,
        index: Option<usize>,
        expected: Option<&Type>,
        annotation: Annotation,
    ) -> Option<Expr> {
        if let Some(i) = index {
            self.path.push(i);
        }
        let result = self.parse_inner(json, expected, annotation);
        if index.is_some() {
            self.path.pop();
        }
        result
    }

    fn parse_inner(
        &mut self,
        json: &Json,
        expected: Option<&Type>,
        annotation: Annotation,
    ) -> Option<Expr> {
        let wrapped;
        let json = match json {
            Json::Null | Json::Bool(_) | Json::Number(_) | Json::String(_) => {
                wrapped = Json::Array(vec![Json::String("literal".into()), json.clone()]);
                &wrapped
            }
            other => other,
        };
        let args = match json {
            Json::Array(args) => args,
            Json::Object(_) => {
                self.error(
                    "Bare objects invalid. Use [\"literal\", {...}] instead.",
                    &[],
                );
                return None;
            }
            _ => {
                self.error(
                    format!("Expected an array, but found {} instead.", js_typeof(json)),
                    &[],
                );
                return None;
            }
        };
        let Some(first) = args.first() else {
            self.error(
                "Expected an array with at least one element. If you wanted a literal array, use [\"literal\", []].",
                &[],
            );
            return None;
        };
        let Some(op) = first.as_str() else {
            self.error(
                format!(
                    "Expression name must be a string, but found {} instead. If you wanted a literal array, use [\"literal\", [...]].",
                    js_typeof(first)
                ),
                &[0],
            );
            return None;
        };
        let mut parsed = self.parse_op(op, args, expected)?;

        if let Some(expected) = expected {
            let actual = &parsed.ty;
            let expects_plain = matches!(
                expected,
                Type::String
                    | Type::Enum(_)
                    | Type::Number
                    | Type::Boolean
                    | Type::Object
                    | Type::Array(..)
            );
            let expects_coercible = matches!(
                expected,
                Type::Color | Type::Formatted | Type::ResolvedImage
            );
            let wrap = |parsed: Expr, kind: fn(Vec<Expr>) -> Kind| Expr {
                ty: expected.clone(),
                key: parsed.key.clone(),
                kind: kind(vec![parsed]),
            };
            if expects_plain && *actual == Type::Value {
                parsed = match annotation {
                    Annotation::Assert => wrap(parsed, Kind::Assert),
                    Annotation::Coerce => wrap(parsed, Kind::Coerce),
                    Annotation::Omit => parsed,
                };
            } else if expects_coercible && matches!(actual, Type::Value | Type::String) {
                parsed = match annotation {
                    Annotation::Assert | Annotation::Coerce => wrap(parsed, Kind::Coerce),
                    Annotation::Omit => parsed,
                };
            } else if let Some(err) = check_subtype(expected, actual) {
                self.error(err, &[]);
                return None;
            }
        }

        // Fold constants. Image expressions stay, so the available images can be consulted.
        if !parsed.is_literal() && parsed.ty != Type::ResolvedImage && is_constant(&parsed) {
            match parsed.eval(&Context::default()) {
                Ok(value) => parsed = Expr::literal(parsed.ty, &parsed.key, value),
                Err(err) => {
                    self.error(err.message, &[]);
                    return None;
                }
            }
        }
        Some(parsed)
    }

    fn parse_op(&mut self, op: &str, args: &[Json], expected: Option<&Type>) -> Option<Expr> {
        match op {
            "literal" => self.parse_literal(args, expected),
            "array" | "boolean" | "number" | "string" | "object" => self.parse_assertion(op, args),
            "to-boolean" | "to-color" | "to-number" | "to-string" => self.parse_coercion(op, args),
            "==" | "!=" | "<" | ">" | "<=" | ">=" => self.parse_comparison(op, args),
            "at" => self.parse_at(args, expected),
            "in" => self.parse_in(args),
            "index-of" => self.parse_index_of(args),
            "slice" => self.parse_slice(args),
            "length" => self.parse_length(args),
            "let" => self.parse_let(args, expected),
            "var" => self.parse_var(args),
            "case" => self.parse_case(args, expected),
            "coalesce" => self.parse_coalesce(args, expected),
            "match" => self.parse_match(args, expected),
            "step" => self.parse_step(args, expected),
            "interpolate" | "interpolate-hcl" | "interpolate-lab" => {
                self.parse_interpolate(op, args, expected)
            }
            "collator" => self.parse_collator(args),
            "number-format" => self.parse_number_format(args),
            "format" => self.parse_format(args),
            "image" => self.parse_image(args),
            "global-state" => self.parse_global_state(args),
            "semiliteral" => self.parse_semiliteral(args),
            _ if UNSUPPORTED.contains(&op) => {
                self.unsupported(op);
                None
            }
            _ => self.parse_compound(op, args),
        }
    }

    fn parse_literal(&mut self, args: &[Json], expected: Option<&Type>) -> Option<Expr> {
        if args.len() != 2 {
            self.error(
                format!(
                    "'literal' expression requires exactly one argument, but found {} instead.",
                    args.len() - 1
                ),
                &[],
            );
            return None;
        }
        let value = Value::from_json(&args[1]);
        let mut ty = type_of(&value);
        // Infer the item type of an empty array from what is expected.
        if let (Type::Array(_, Some(0)), Some(expected @ Type::Array(_, len))) = (&ty, expected)
            && matches!(len, None | Some(0))
        {
            ty = expected.clone();
        }
        Some(Expr::literal(ty, &self.key(), value))
    }

    fn parse_assertion(&mut self, name: &str, args: &[Json]) -> Option<Expr> {
        if args.len() < 2 {
            self.error("Expected at least one argument.", &[]);
            return None;
        }
        let mut i = 1;
        let ty = if name == "array" {
            let item = if args.len() > 2 {
                let item = match args[1].as_str() {
                    Some("string") => Type::String,
                    Some("number") => Type::Number,
                    Some("boolean") => Type::Boolean,
                    _ => {
                        self.error(
                            "The item type argument of \"array\" must be one of string, number, boolean",
                            &[1],
                        );
                        return None;
                    }
                };
                i += 1;
                item
            } else {
                Type::Value
            };
            let length = if args.len() > 3 {
                let n = match &args[2] {
                    Json::Null => None,
                    Json::Number(n) => match n.as_f64() {
                        Some(f) if f >= 0.0 && f.fract() == 0.0 => Some(f as usize),
                        _ => {
                            self.error(
                                "The length argument to \"array\" must be a positive integer literal",
                                &[2],
                            );
                            return None;
                        }
                    },
                    _ => {
                        self.error(
                            "The length argument to \"array\" must be a positive integer literal",
                            &[2],
                        );
                        return None;
                    }
                };
                i += 1;
                n
            } else {
                None
            };
            Type::array(item, length)
        } else {
            match name {
                "string" => Type::String,
                "number" => Type::Number,
                "boolean" => Type::Boolean,
                _ => Type::Object,
            }
        };
        let mut parsed = Vec::new();
        while i < args.len() {
            parsed.push(self.parse(&args[i], Some(i), Some(&Type::Value), Annotation::Assert)?);
            i += 1;
        }
        Some(Expr {
            ty,
            key: self.key(),
            kind: Kind::Assert(parsed),
        })
    }

    fn parse_coercion(&mut self, name: &str, args: &[Json]) -> Option<Expr> {
        if args.len() < 2 {
            self.error("Expected at least one argument.", &[]);
            return None;
        }
        let ty = match name {
            "to-boolean" => Type::Boolean,
            "to-color" => Type::Color,
            "to-number" => Type::Number,
            _ => Type::String,
        };
        if matches!(ty, Type::Boolean | Type::String) && args.len() != 2 {
            self.error("Expected one argument.", &[]);
            return None;
        }
        let mut parsed = Vec::new();
        for (i, arg) in args.iter().enumerate().skip(1) {
            parsed.push(self.parse(arg, Some(i), Some(&Type::Value), Annotation::Assert)?);
        }
        Some(Expr {
            ty,
            key: self.key(),
            kind: Kind::Coerce(parsed),
        })
    }

    fn parse_comparison(&mut self, name: &str, args: &[Json]) -> Option<Expr> {
        let op = match name {
            "==" => CmpOp::Eq,
            "!=" => CmpOp::Ne,
            "<" => CmpOp::Lt,
            ">" => CmpOp::Gt,
            "<=" => CmpOp::Le,
            _ => CmpOp::Ge,
        };
        if args.len() != 3 && args.len() != 4 {
            self.error("Expected two or three arguments.", &[]);
            return None;
        }
        let comparable = |t: &Type| {
            if op.is_order() {
                matches!(t, Type::String | Type::Number | Type::Value)
            } else {
                matches!(
                    t,
                    Type::Boolean | Type::String | Type::Number | Type::Null | Type::Value
                )
            }
        };
        let mut lhs = self.parse(&args[1], Some(1), Some(&Type::Value), Annotation::Assert)?;
        if !comparable(&lhs.ty) {
            self.error(
                format!(
                    "\"{name}\" comparisons are not supported for type '{}'.",
                    lhs.ty
                ),
                &[1],
            );
            return None;
        }
        let mut rhs = self.parse(&args[2], Some(2), Some(&Type::Value), Annotation::Assert)?;
        if !comparable(&rhs.ty) {
            self.error(
                format!(
                    "\"{name}\" comparisons are not supported for type '{}'.",
                    rhs.ty
                ),
                &[2],
            );
            return None;
        }
        if !lhs.ty.same_kind(&rhs.ty) && lhs.ty != Type::Value && rhs.ty != Type::Value {
            self.error(
                format!("Cannot compare types '{}' and '{}'.", lhs.ty, rhs.ty),
                &[],
            );
            return None;
        }
        let key = self.key();
        if op.is_order() {
            if lhs.ty == Type::Value && rhs.ty != Type::Value {
                lhs = Expr {
                    ty: rhs.ty.clone(),
                    key: key.clone(),
                    kind: Kind::Assert(vec![lhs]),
                };
            } else if lhs.ty != Type::Value && rhs.ty == Type::Value {
                rhs = Expr {
                    ty: lhs.ty.clone(),
                    key: key.clone(),
                    kind: Kind::Assert(vec![rhs]),
                };
            }
        }
        let untyped = lhs.ty == Type::Value || rhs.ty == Type::Value;
        let mut collator = None;
        if args.len() == 4 {
            if !matches!(lhs.ty, Type::String | Type::Value)
                && !matches!(rhs.ty, Type::String | Type::Value)
            {
                self.error("Cannot use collator to compare non-string types.", &[]);
                return None;
            }
            collator = Some(Box::new(self.parse(
                &args[3],
                Some(3),
                Some(&Type::Collator),
                Annotation::Assert,
            )?));
        }
        Some(Expr {
            ty: Type::Boolean,
            key,
            kind: Kind::Compare {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                collator,
                untyped,
            },
        })
    }

    fn parse_at(&mut self, args: &[Json], expected: Option<&Type>) -> Option<Expr> {
        if args.len() != 3 {
            self.error(
                format!(
                    "Expected 2 arguments, but found {} instead.",
                    args.len() - 1
                ),
                &[],
            );
            return None;
        }
        let index = self.parse(&args[1], Some(1), Some(&Type::Number), Annotation::Assert);
        let wanted = Type::array(expected.cloned().unwrap_or(Type::Value), None);
        let input = self.parse(&args[2], Some(2), Some(&wanted), Annotation::Assert);
        let (index, input) = (index?, input?);
        Some(Expr {
            ty: input.ty.item().clone(),
            key: self.key(),
            kind: Kind::At(Box::new(index), Box::new(input)),
        })
    }

    fn needle_ok(&mut self, needle: &Expr) -> bool {
        if matches!(
            needle.ty,
            Type::Boolean | Type::String | Type::Number | Type::Null | Type::Value
        ) {
            return true;
        }
        self.error(
            format!(
                "Expected first argument to be of type boolean, string, number or null, but found {} instead",
                needle.ty
            ),
            &[],
        );
        false
    }

    fn parse_in(&mut self, args: &[Json]) -> Option<Expr> {
        if args.len() != 3 {
            self.error(
                format!(
                    "Expected 2 arguments, but found {} instead.",
                    args.len() - 1
                ),
                &[],
            );
            return None;
        }
        let needle = self.parse(&args[1], Some(1), Some(&Type::Value), Annotation::Assert);
        let haystack = self.parse(&args[2], Some(2), Some(&Type::Value), Annotation::Assert);
        let (needle, haystack) = (needle?, haystack?);
        if !self.needle_ok(&needle) {
            return None;
        }
        Some(Expr {
            ty: Type::Boolean,
            key: self.key(),
            kind: Kind::In(Box::new(needle), Box::new(haystack)),
        })
    }

    fn parse_index_of(&mut self, args: &[Json]) -> Option<Expr> {
        if args.len() <= 2 || args.len() >= 5 {
            self.error(
                format!(
                    "Expected 2 or 3 arguments, but found {} instead.",
                    args.len() - 1
                ),
                &[],
            );
            return None;
        }
        let needle = self.parse(&args[1], Some(1), Some(&Type::Value), Annotation::Assert);
        let haystack = self.parse(&args[2], Some(2), Some(&Type::Value), Annotation::Assert);
        let (needle, haystack) = (needle?, haystack?);
        if !self.needle_ok(&needle) {
            return None;
        }
        let from = match args.get(3) {
            Some(arg) => Some(Box::new(self.parse(
                arg,
                Some(3),
                Some(&Type::Number),
                Annotation::Assert,
            )?)),
            None => None,
        };
        Some(Expr {
            ty: Type::Number,
            key: self.key(),
            kind: Kind::IndexOf(Box::new(needle), Box::new(haystack), from),
        })
    }

    fn parse_slice(&mut self, args: &[Json]) -> Option<Expr> {
        if args.len() <= 2 || args.len() >= 5 {
            self.error(
                format!(
                    "Expected 2 or 3 arguments, but found {} instead.",
                    args.len() - 1
                ),
                &[],
            );
            return None;
        }
        let input = self.parse(&args[1], Some(1), Some(&Type::Value), Annotation::Assert);
        let begin = self.parse(&args[2], Some(2), Some(&Type::Number), Annotation::Assert);
        let (input, begin) = (input?, begin?);
        if !matches!(input.ty, Type::Array(..) | Type::String | Type::Value) {
            self.error(
                format!(
                    "Expected first argument to be of type array or string, but found {} instead",
                    input.ty
                ),
                &[],
            );
            return None;
        }
        let end = match args.get(3) {
            Some(arg) => Some(Box::new(self.parse(
                arg,
                Some(3),
                Some(&Type::Number),
                Annotation::Assert,
            )?)),
            None => None,
        };
        Some(Expr {
            ty: input.ty.clone(),
            key: self.key(),
            kind: Kind::Slice(Box::new(input), Box::new(begin), end),
        })
    }

    fn parse_length(&mut self, args: &[Json]) -> Option<Expr> {
        if args.len() != 2 {
            self.error(
                format!("Expected 1 argument, but found {} instead.", args.len() - 1),
                &[],
            );
            return None;
        }
        let input = self.parse(&args[1], Some(1), None, Annotation::Assert)?;
        if !matches!(input.ty, Type::Array(..) | Type::String | Type::Value) {
            self.error(
                format!(
                    "Expected argument of type string or array, but found {} instead.",
                    input.ty
                ),
                &[],
            );
            return None;
        }
        Some(Expr {
            ty: Type::Number,
            key: self.key(),
            kind: Kind::Length(Box::new(input)),
        })
    }

    fn parse_let(&mut self, args: &[Json], expected: Option<&Type>) -> Option<Expr> {
        if args.len() < 4 {
            self.error(
                format!(
                    "Expected at least 3 arguments, but found {} instead.",
                    args.len() - 1
                ),
                &[],
            );
            return None;
        }
        let mut bindings = Vec::new();
        let mut i = 1;
        while i < args.len() - 1 {
            let Some(name) = args[i].as_str() else {
                self.error(
                    format!(
                        "Expected string, but found {} instead.",
                        js_typeof(&args[i])
                    ),
                    &[i],
                );
                return None;
            };
            if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                self.error(
                    "Variable names must contain only alphanumeric characters or '_'.",
                    &[i],
                );
                return None;
            }
            let value = self.parse(&args[i + 1], Some(i + 1), None, Annotation::Assert)?;
            bindings.push((name.to_string(), value));
            i += 2;
        }
        let saved = self.scope.len();
        self.scope.extend(bindings.iter().cloned());
        let result = self.parse(
            &args[args.len() - 1],
            Some(args.len() - 1),
            expected,
            Annotation::Assert,
        );
        self.scope.truncate(saved);
        let result = result?;
        Some(Expr {
            ty: result.ty.clone(),
            key: self.key(),
            kind: Kind::Let(bindings, Box::new(result)),
        })
    }

    fn parse_var(&mut self, args: &[Json]) -> Option<Expr> {
        let name = match args.get(1).and_then(Json::as_str) {
            Some(name) if args.len() == 2 => name,
            _ => {
                self.error(
                    "'var' expression requires exactly one string literal argument.",
                    &[],
                );
                return None;
            }
        };
        let Some((_, bound)) = self.scope.iter().rev().find(|(n, _)| n == name) else {
            self.error(
                format!(
                    "Unknown variable \"{name}\". Make sure \"{name}\" has been bound in an enclosing \"let\" expression before using it."
                ),
                &[1],
            );
            return None;
        };
        let bound = bound.clone();
        Some(Expr {
            ty: bound.ty.clone(),
            key: self.key(),
            kind: Kind::Var(Box::new(bound)),
        })
    }

    fn parse_case(&mut self, args: &[Json], expected: Option<&Type>) -> Option<Expr> {
        if args.len() < 4 {
            self.error(
                format!(
                    "Expected at least 3 arguments, but found only {}.",
                    args.len() - 1
                ),
                &[],
            );
            return None;
        }
        if !args.len().is_multiple_of(2) {
            self.error("Expected an odd number of arguments.", &[]);
            return None;
        }
        let mut output_type = expected.filter(|t| **t != Type::Value).cloned();
        let mut branches = Vec::new();
        let mut i = 1;
        while i < args.len() - 1 {
            let test = self.parse(&args[i], Some(i), Some(&Type::Boolean), Annotation::Assert)?;
            let result = self.parse(
                &args[i + 1],
                Some(i + 1),
                output_type.as_ref(),
                Annotation::Assert,
            )?;
            output_type.get_or_insert_with(|| result.ty.clone());
            branches.push((test, result));
            i += 2;
        }
        let otherwise = self.parse(
            &args[args.len() - 1],
            Some(args.len() - 1),
            output_type.as_ref(),
            Annotation::Assert,
        )?;
        Some(Expr {
            ty: output_type.unwrap_or_else(|| otherwise.ty.clone()),
            key: self.key(),
            kind: Kind::Case(branches, Box::new(otherwise)),
        })
    }

    fn parse_coalesce(&mut self, args: &[Json], expected: Option<&Type>) -> Option<Expr> {
        if args.len() < 2 {
            self.error("Expected at least one argument.", &[]);
            return None;
        }
        let mut output_type = expected.filter(|t| **t != Type::Value).cloned();
        let mut parsed = Vec::new();
        for arg in &args[1..] {
            // Parsed without an inferred annotation so a null argument does not raise an error
            // before the coalescing can happen; the enclosing node is annotated instead.
            let e = self.parse(
                arg,
                Some(1 + parsed.len()),
                output_type.as_ref(),
                Annotation::Omit,
            )?;
            output_type.get_or_insert_with(|| e.ty.clone());
            parsed.push(e);
        }
        let needs_annotation = expected.is_some_and(|expected| {
            parsed
                .iter()
                .any(|arg| check_subtype(expected, &arg.ty).is_some())
        });
        Some(Expr {
            ty: if needs_annotation {
                Type::Value
            } else {
                output_type.unwrap_or(Type::Value)
            },
            key: self.key(),
            kind: Kind::Coalesce(parsed),
        })
    }

    fn parse_match(&mut self, args: &[Json], expected: Option<&Type>) -> Option<Expr> {
        if args.len() < 5 {
            self.error(
                format!(
                    "Expected at least 4 arguments, but found only {}.",
                    args.len() - 1
                ),
                &[],
            );
            return None;
        }
        if args.len() % 2 != 1 {
            self.error("Expected an even number of arguments.", &[]);
            return None;
        }
        let mut input_type: Option<Type> = None;
        let mut output_type = expected.filter(|t| **t != Type::Value).cloned();
        let mut cases = BTreeMap::new();
        let mut outputs = Vec::new();
        let mut i = 2;
        while i < args.len() - 1 {
            let labels: Vec<&Json> = match &args[i] {
                Json::Array(labels) => labels.iter().collect(),
                other => vec![other],
            };
            if labels.is_empty() {
                self.error("Expected at least one branch label.", &[i]);
                return None;
            }
            for label in labels {
                let (label_key, label_type) = match label {
                    Json::Number(n) => {
                        let n = n.as_f64().unwrap_or(f64::NAN);
                        if n.abs() > 9007199254740991.0 {
                            self.error(
                                "Branch labels must be integers no larger than 9007199254740991.",
                                &[i],
                            );
                            return None;
                        }
                        if n.fract() != 0.0 {
                            self.error("Numeric branch labels must be integer values.", &[i]);
                            return None;
                        }
                        (js_number(n), Type::Number)
                    }
                    Json::String(s) => (s.clone(), Type::String),
                    _ => {
                        self.error("Branch labels must be numbers or strings.", &[i]);
                        return None;
                    }
                };
                match &input_type {
                    None => input_type = Some(label_type),
                    Some(current) => {
                        if let Some(err) = check_subtype(current, &label_type) {
                            self.error(err, &[i]);
                            return None;
                        }
                    }
                }
                if cases.contains_key(&label_key) {
                    self.error("Branch labels must be unique.", &[i]);
                    return None;
                }
                cases.insert(label_key, outputs.len());
            }
            let result = self.parse(
                &args[i + 1],
                Some(i),
                output_type.as_ref(),
                Annotation::Assert,
            )?;
            output_type.get_or_insert_with(|| result.ty.clone());
            outputs.push(result);
            i += 2;
        }
        let input = self.parse(&args[1], Some(1), Some(&Type::Value), Annotation::Assert)?;
        let otherwise = self.parse(
            &args[args.len() - 1],
            Some(args.len() - 1),
            output_type.as_ref(),
            Annotation::Assert,
        )?;
        let input_type = input_type.unwrap_or(Type::Value);
        if input.ty != Type::Value
            && let Some(err) = check_subtype(&input_type, &input.ty)
        {
            self.error(err, &[1]);
            return None;
        }
        Some(Expr {
            ty: output_type.unwrap_or_else(|| otherwise.ty.clone()),
            key: self.key(),
            kind: Kind::Match {
                input: Box::new(input),
                input_type,
                cases,
                outputs,
                otherwise: Box::new(otherwise),
            },
        })
    }

    /// The `label, output` pairs of a `step` or `interpolate` from `args[first..]`.
    fn parse_stops(
        &mut self,
        name: &str,
        args: &[Json],
        first: usize,
        output_type: &mut Option<Type>,
    ) -> Option<(Vec<f64>, Vec<Expr>)> {
        let mut labels = Vec::new();
        let mut outputs = Vec::new();
        let mut i = first;
        while i < args.len() {
            let label = if name == "step" && i == 1 {
                f64::NEG_INFINITY
            } else {
                match args[i].as_f64() {
                    Some(n) => n,
                    None => {
                        self.error(
                            format!(
                                "Input/output pairs for \"{name}\" expressions must be defined using literal numeric values (not computed expressions) for the input values."
                            ),
                            &[i],
                        );
                        return None;
                    }
                }
            };
            if labels.last().is_some_and(|last| *last >= label) {
                self.error(
                    format!(
                        "Input/output pairs for \"{name}\" expressions must be arranged with input values in strictly ascending order."
                    ),
                    &[i],
                );
                return None;
            }
            let parsed = self.parse(
                &args[i + 1],
                Some(i + 1),
                output_type.as_ref(),
                Annotation::Assert,
            )?;
            output_type.get_or_insert_with(|| parsed.ty.clone());
            labels.push(label);
            outputs.push(parsed);
            i += 2;
        }
        Some((labels, outputs))
    }

    fn parse_step(&mut self, args: &[Json], expected: Option<&Type>) -> Option<Expr> {
        if args.len() - 1 < 4 {
            self.error(
                format!(
                    "Expected at least 4 arguments, but found only {}.",
                    args.len() - 1
                ),
                &[],
            );
            return None;
        }
        if !(args.len() - 1).is_multiple_of(2) {
            self.error("Expected an even number of arguments.", &[]);
            return None;
        }
        let input = self.parse(&args[1], Some(1), Some(&Type::Number), Annotation::Assert)?;
        let mut output_type = expected.filter(|t| **t != Type::Value).cloned();
        let (labels, outputs) = self.parse_stops("step", args, 1, &mut output_type)?;
        Some(Expr {
            ty: output_type.unwrap_or(Type::Value),
            key: self.key(),
            kind: Kind::Step {
                input: Box::new(input),
                labels,
                outputs,
            },
        })
    }

    fn parse_interpolate(
        &mut self,
        name: &str,
        args: &[Json],
        expected: Option<&Type>,
    ) -> Option<Expr> {
        let Some(spec) = args
            .get(1)
            .and_then(Json::as_array)
            .filter(|a| !a.is_empty())
        else {
            self.error("Expected an interpolation type expression.", &[1]);
            return None;
        };
        let interpolation = match spec[0].as_str() {
            Some("linear") => Interpolation::Linear,
            Some("exponential") => match spec.get(1).and_then(Json::as_f64) {
                Some(base) => Interpolation::Exponential(base),
                None => {
                    self.error(
                        "Exponential interpolation requires a numeric base.",
                        &[1, 1],
                    );
                    return None;
                }
            },
            Some("cubic-bezier") => {
                let points: Vec<f64> = spec[1..].iter().filter_map(Json::as_f64).collect();
                if points.len() != 4
                    || spec.len() != 5
                    || points.iter().any(|t| !(0.0..=1.0).contains(t))
                {
                    self.error(
                        "Cubic bezier interpolation requires four numeric arguments with values between 0 and 1.",
                        &[1],
                    );
                    return None;
                }
                Interpolation::CubicBezier(points[0], points[1], points[2], points[3])
            }
            _ => {
                self.error(
                    format!(
                        "Unknown interpolation type {}",
                        spec[0].as_str().unwrap_or("undefined")
                    ),
                    &[1, 0],
                );
                return None;
            }
        };
        if args.len() - 1 < 4 {
            self.error(
                format!(
                    "Expected at least 4 arguments, but found only {}.",
                    args.len() - 1
                ),
                &[],
            );
            return None;
        }
        if !(args.len() - 1).is_multiple_of(2) {
            self.error("Expected an even number of arguments.", &[]);
            return None;
        }
        let input = self.parse(&args[2], Some(2), Some(&Type::Number), Annotation::Assert)?;
        let space = match name {
            "interpolate-hcl" => ColorSpace::Hcl,
            "interpolate-lab" => ColorSpace::Lab,
            _ => ColorSpace::Rgb,
        };
        let mut output_type = if space != ColorSpace::Rgb {
            Some(Type::Color)
        } else {
            expected.filter(|t| **t != Type::Value).cloned()
        };
        let (labels, outputs) = self.parse_stops("interpolate", args, 3, &mut output_type)?;
        let ty = output_type.unwrap_or(Type::Value);
        let interpolatable = match &ty {
            Type::Number | Type::Color => true,
            Type::Array(..) => *ty.item() == Type::Number && ty.length().is_some(),
            _ => false,
        };
        if !interpolatable {
            self.error(format!("Type {ty} is not interpolatable."), &[]);
            return None;
        }
        Some(Expr {
            ty,
            key: self.key(),
            kind: Kind::Interpolate {
                space,
                interpolation,
                input: Box::new(input),
                labels,
                outputs,
            },
        })
    }

    fn parse_collator(&mut self, args: &[Json]) -> Option<Expr> {
        if args.len() != 2 {
            self.error("Expected one argument.", &[]);
            return None;
        }
        let Some(options) = args[1].as_object() else {
            self.error("Collator options argument must be an object.", &[]);
            return None;
        };
        let case_sensitive = self.parse(
            options.get("case-sensitive").unwrap_or(&Json::Bool(false)),
            Some(1),
            Some(&Type::Boolean),
            Annotation::Assert,
        )?;
        let diacritic_sensitive = self.parse(
            options
                .get("diacritic-sensitive")
                .unwrap_or(&Json::Bool(false)),
            Some(1),
            Some(&Type::Boolean),
            Annotation::Assert,
        )?;
        let locale = match options.get("locale").filter(|l| json_truthy(l)) {
            Some(locale) => Some(Box::new(self.parse(
                locale,
                Some(1),
                Some(&Type::String),
                Annotation::Assert,
            )?)),
            None => None,
        };
        Some(Expr {
            ty: Type::Collator,
            key: self.key(),
            kind: Kind::Collator {
                case_sensitive: Box::new(case_sensitive),
                diacritic_sensitive: Box::new(diacritic_sensitive),
                locale,
            },
        })
    }

    fn parse_number_format(&mut self, args: &[Json]) -> Option<Expr> {
        if args.len() != 3 {
            self.error("Expected two arguments.", &[]);
            return None;
        }
        let number = self.parse(&args[1], Some(1), Some(&Type::Number), Annotation::Assert)?;
        let Some(options) = args[2].as_object() else {
            self.error("NumberFormat options argument must be an object.", &[]);
            return None;
        };
        // The reference only reads options that are truthy, so `0` and `""` are ignored.
        let option = |this: &mut Parser, name: &str, ty: &Type| -> Option<Option<Expr>> {
            match options.get(name).filter(|v| json_truthy(v)) {
                Some(value) => Some(Some(this.parse(
                    value,
                    Some(1),
                    Some(ty),
                    Annotation::Assert,
                )?)),
                None => Some(None),
            }
        };
        // The locale is parsed for its errors but not kept: formatting is `en` only.
        option(self, "locale", &Type::String)?;
        let currency = option(self, "currency", &Type::String)?;
        let unit = option(self, "unit", &Type::String)?;
        if currency.is_some() && unit.is_some() {
            self.error(
                "NumberFormat options `currency` and `unit` are mutually exclusive",
                &[],
            );
            return None;
        }
        let min_fraction_digits = option(self, "min-fraction-digits", &Type::Number)?;
        let max_fraction_digits = option(self, "max-fraction-digits", &Type::Number)?;
        Some(Expr {
            ty: Type::String,
            key: self.key(),
            kind: Kind::NumberFormat(Box::new(NumberFormatArgs {
                number,
                currency,
                unit,
                min_fraction_digits,
                max_fraction_digits,
            })),
        })
    }

    fn parse_format(&mut self, args: &[Json]) -> Option<Expr> {
        if args.len() < 2 {
            self.error("Expected at least one argument.", &[]);
            return None;
        }
        if args[1].is_object() {
            self.error("First argument must be an image or text section.", &[]);
            return None;
        }
        let mut sections: Vec<FormatSection> = Vec::new();
        let mut next_may_be_options = false;
        for arg in &args[1..] {
            if let (true, Some(options)) = (next_may_be_options, arg.as_object()) {
                next_may_be_options = false;
                let option = |this: &mut Parser, name: &str, ty: &Type| -> Option<Option<Expr>> {
                    match options.get(name).filter(|v| json_truthy(v)) {
                        Some(value) => Some(Some(this.parse(
                            value,
                            Some(1),
                            Some(ty),
                            Annotation::Assert,
                        )?)),
                        None => Some(None),
                    }
                };
                let scale = option(self, "font-scale", &Type::Number)?;
                let font = option(self, "text-font", &Type::array(Type::String, None))?;
                let text_color = option(self, "text-color", &Type::Color)?;
                if let Some(Json::String(align)) = options.get("vertical-align")
                    && !["bottom", "center", "top"].contains(&align.as_str())
                {
                    self.error(
                        format!(
                            "'vertical-align' must be one of: 'bottom', 'center', 'top' but found '{align}' instead."
                        ),
                        &[],
                    );
                    return None;
                }
                let vertical_align = option(self, "vertical-align", &Type::String)?;
                if let Some(last) = sections.last_mut() {
                    last.scale = scale;
                    last.font = font;
                    last.text_color = text_color;
                    last.vertical_align = vertical_align;
                }
            } else {
                let content = self.parse(arg, Some(1), Some(&Type::Value), Annotation::Assert)?;
                if !matches!(
                    content.ty,
                    Type::String | Type::Value | Type::Null | Type::ResolvedImage
                ) {
                    self.error(
                        "Formatted text type must be 'string', 'value', 'image' or 'null'.",
                        &[],
                    );
                    return None;
                }
                next_may_be_options = true;
                sections.push(FormatSection {
                    content,
                    scale: None,
                    font: None,
                    text_color: None,
                    vertical_align: None,
                });
            }
        }
        Some(Expr {
            ty: Type::Formatted,
            key: self.key(),
            kind: Kind::Format(sections),
        })
    }

    fn parse_image(&mut self, args: &[Json]) -> Option<Expr> {
        if args.len() != 2 {
            self.error("Expected two arguments.", &[]);
            return None;
        }
        let Some(name) = self.parse(&args[1], Some(1), Some(&Type::String), Annotation::Assert)
        else {
            self.error("No image name provided.", &[]);
            return None;
        };
        Some(Expr {
            ty: Type::ResolvedImage,
            key: self.key(),
            kind: Kind::Image(Box::new(name)),
        })
    }

    fn parse_global_state(&mut self, args: &[Json]) -> Option<Expr> {
        if args.len() != 2 {
            self.error(
                format!("Expected 1 argument, but found {} instead.", args.len() - 1),
                &[],
            );
            return None;
        }
        match &args[1] {
            Json::Null => {
                self.error("Global state property must be defined.", &[]);
                None
            }
            Json::String(key) => Some(Expr {
                ty: Type::Value,
                key: self.key(),
                kind: Kind::GlobalState(key.clone()),
            }),
            other => {
                self.error(
                    format!(
                        "Global state property must be string, but found {} instead.",
                        js_typeof(other)
                    ),
                    &[],
                );
                None
            }
        }
    }

    fn parse_semiliteral(&mut self, args: &[Json]) -> Option<Expr> {
        if args.len() != 2 {
            self.error(
                format!(
                    "'semiliteral' expression requires exactly one argument, but found {} instead.",
                    args.len() - 1
                ),
                &[],
            );
            return None;
        }
        let Json::Array(items) = &args[1] else {
            let value = Value::from_json(&args[1]);
            return Some(Expr::literal(type_of(&value), &self.key(), value));
        };
        self.path.push(1);
        let mut parsed = Vec::new();
        for (i, item) in items.iter().enumerate() {
            match self.parse(item, Some(i), Some(&Type::Value), Annotation::Assert) {
                Some(e) => parsed.push(e),
                None => {
                    self.path.pop();
                    return None;
                }
            }
        }
        self.path.pop();
        let mut element_type: Option<Type> = None;
        for e in &parsed {
            match &element_type {
                None => element_type = Some(e.ty.clone()),
                Some(current) if *current == e.ty && !matches!(e.ty, Type::Array(..)) => {}
                Some(_) => {
                    element_type = Some(Type::Value);
                    break;
                }
            }
        }
        Some(Expr {
            ty: Type::array(element_type.unwrap_or(Type::Value), Some(parsed.len())),
            key: self.key(),
            kind: Kind::Semiliteral(parsed),
        })
    }

    fn parse_compound(&mut self, name: &str, args: &[Json]) -> Option<Expr> {
        let Some(def) = definition(name) else {
            self.error(
                format!(
                    "Unknown expression \"{name}\". If you wanted a literal array, use [\"literal\", [...]]."
                ),
                &[0],
            );
            return None;
        };
        let overloads: Vec<&(Sig, Op)> = def
            .overloads
            .iter()
            .filter(|(sig, _)| match sig {
                Sig::Varargs(_) => true,
                Sig::Fixed(params) => params.len() == args.len() - 1,
            })
            .collect();
        // Each signature is tried with its own error list, so a late success leaves no trace of
        // the earlier failures.
        let mut last_errors = Vec::new();
        for (sig, op) in &overloads {
            let saved = self.errors.len();
            let mut parsed = Vec::new();
            let mut failed = false;
            for arg in &args[1..] {
                let expected = sig.param(parsed.len());
                match self.parse(
                    arg,
                    Some(1 + parsed.len()),
                    Some(expected),
                    Annotation::Assert,
                ) {
                    Some(e) => parsed.push(e),
                    None => {
                        failed = true;
                        break;
                    }
                }
            }
            if !failed {
                for (i, arg) in parsed.iter().enumerate() {
                    if let Some(err) = check_subtype(sig.param(i), &arg.ty) {
                        self.error(err, &[i + 1]);
                    }
                }
                if self.errors.len() == saved {
                    return Some(Expr {
                        ty: def.ty.clone(),
                        key: self.key(),
                        kind: Kind::Compound(*op, parsed),
                    });
                }
            }
            last_errors = self.errors.drain(saved..).collect();
        }
        if overloads.len() == 1 {
            self.errors.extend(last_errors);
        } else {
            let candidates: Vec<&(Sig, Op)> = if overloads.is_empty() {
                def.overloads.iter().collect()
            } else {
                overloads
            };
            let signatures: Vec<String> =
                candidates.iter().map(|(sig, _)| sig.describe()).collect();
            // For the message, parse the arguments again without any coercion.
            let mut actual = Vec::new();
            for (i, arg) in args[1..].iter().enumerate() {
                let parsed = self.parse(arg, Some(1 + i), None, Annotation::Assert)?;
                actual.push(parsed.ty.name());
            }
            self.error(
                format!(
                    "Expected arguments of type {}, but found ({}) instead.",
                    signatures.join(" | "),
                    actual.join(", ")
                ),
                &[],
            );
        }
        None
    }
}

/// JavaScript truthiness of a JSON value, for options the reference reads with `if (x)`.
fn json_truthy(json: &Json) -> bool {
    match json {
        Json::Null => false,
        Json::Bool(b) => *b,
        Json::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Json::String(s) => !s.is_empty(),
        _ => true,
    }
}

// ---------------------------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------------------------

fn as_num(value: &Value) -> f64 {
    value.as_f64().unwrap_or(f64::NAN)
}

/// JavaScript's `ToIntegerOrInfinity`.
fn js_int(n: f64) -> f64 {
    if n.is_nan() { 0.0 } else { n.trunc() }
}

/// `start..end` of a JavaScript `slice(begin, end)` on `len` items.
fn slice_bounds(len: usize, begin: f64, end: Option<f64>) -> (usize, usize) {
    let len_f = len as f64;
    let clamp = |i: f64| {
        let i = js_int(i);
        if i < 0.0 {
            (len_f + i).max(0.0) as usize
        } else {
            i.min(len_f) as usize
        }
    };
    let begin = clamp(begin);
    let end = end.map_or(len, clamp);
    (begin, end.max(begin))
}

fn validate_rgba(r: f64, g: f64, b: f64, a: Option<f64>) -> Option<String> {
    let in_range = |x: f64| (0.0..=255.0).contains(&x);
    let list = |with_alpha: bool| {
        let mut parts = vec![js_number(r), js_number(g), js_number(b)];
        if let (true, Some(a)) = (with_alpha, a) {
            parts.push(js_number(a));
        }
        parts.join(", ")
    };
    if !(in_range(r) && in_range(g) && in_range(b)) {
        return Some(format!(
            "Invalid rgba value [{}]: 'r', 'g', and 'b' must be between 0 and 255.",
            list(true)
        ));
    }
    if let Some(a) = a
        && !(0.0..=1.0).contains(&a)
    {
        return Some(format!(
            "Invalid rgba value [{}]: 'a' must be between 0 and 1.",
            list(true)
        ));
    }
    None
}

/// The index of the last stop `<= input`, or 0 when there is none.
fn find_stop(stops: &[f64], input: f64, key: &str) -> Result<usize, RuntimeError> {
    let last = stops.len().saturating_sub(1);
    let (mut lower, mut upper) = (0i64, last as i64);
    while lower <= upper {
        let current = ((lower + upper) / 2) as usize;
        let value = stops[current];
        if value <= input {
            if current == last || input < stops[current + 1] {
                return Ok(current);
            }
            lower = current as i64 + 1;
        } else if value > input {
            upper = current as i64 - 1;
        } else {
            return Err(RuntimeError::new(key, "Input is not a number."));
        }
    }
    Ok(0)
}

fn exponential_interpolation(input: f64, base: f64, lower: f64, upper: f64) -> f64 {
    let difference = upper - lower;
    let progress = input - lower;
    if difference == 0.0 {
        0.0
    } else if base == 1.0 {
        progress / difference
    } else {
        (base.powf(progress) - 1.0) / (base.powf(difference) - 1.0)
    }
}

/// The interpolation factor between two stops.
fn interpolation_factor(interpolation: Interpolation, input: f64, lower: f64, upper: f64) -> f64 {
    match interpolation {
        Interpolation::Linear => exponential_interpolation(input, 1.0, lower, upper),
        Interpolation::Exponential(base) => exponential_interpolation(input, base, lower, upper),
        Interpolation::CubicBezier(p1x, p1y, p2x, p2y) => unit_bezier(
            p1x,
            p1y,
            p2x,
            p2y,
            exponential_interpolation(input, 1.0, lower, upper),
        ),
    }
}

/// `y` at `x` on the cubic bezier through (0,0), (p1x,p1y), (p2x,p2y), (1,1).
fn unit_bezier(p1x: f64, p1y: f64, p2x: f64, p2y: f64, x: f64) -> f64 {
    let cx = 3.0 * p1x;
    let bx = 3.0 * (p2x - p1x) - cx;
    let ax = 1.0 - cx - bx;
    let cy = 3.0 * p1y;
    let by = 3.0 * (p2y - p1y) - cy;
    let ay = 1.0 - cy - by;
    let sample_x = |t: f64| ((ax * t + bx) * t + cx) * t;
    let sample_y = |t: f64| ((ay * t + by) * t + cy) * t;
    let derivative_x = |t: f64| (3.0 * ax * t + 2.0 * bx) * t + cx;
    let epsilon = 1e-6;
    // Newton's method first, then bisection where it does not converge.
    let mut t = x;
    for _ in 0..8 {
        let dx = sample_x(t) - x;
        if dx.abs() < epsilon {
            return sample_y(t);
        }
        let d = derivative_x(t);
        if d.abs() < 1e-6 {
            break;
        }
        t -= dx / d;
    }
    let (mut t0, mut t1) = (0.0, 1.0);
    t = x;
    if t < t0 {
        return sample_y(t0);
    }
    if t > t1 {
        return sample_y(t1);
    }
    while t0 < t1 {
        let sx = sample_x(t);
        if (sx - x).abs() < epsilon {
            return sample_y(t);
        }
        if x > sx {
            t0 = t;
        } else {
            t1 = t;
        }
        t = (t1 - t0) * 0.5 + t0;
    }
    sample_y(t)
}

fn lerp(from: f64, to: f64, t: f64) -> f64 {
    from + t * (to - from)
}

// Color spaces, after https://observablehq.com/@mbostock/lab-and-rgb (D50).
const XN: f64 = 0.96422;
const YN: f64 = 1.0;
const ZN: f64 = 0.82521;
const T0: f64 = 4.0 / 29.0;
const T1: f64 = 6.0 / 29.0;
const T2: f64 = 3.0 * T1 * T1;
const T3: f64 = T1 * T1 * T1;

fn constrain_angle(angle: f64) -> f64 {
    let angle = angle % 360.0;
    if angle < 0.0 { angle + 360.0 } else { angle }
}

fn rgb_to_xyz(x: f64) -> f64 {
    if x <= 0.04045 {
        x / 12.92
    } else {
        ((x + 0.055) / 1.055).powf(2.4)
    }
}

fn xyz_to_lab(t: f64) -> f64 {
    if t > T3 { t.cbrt() } else { t / T2 + T0 }
}

fn lab_to_xyz(t: f64) -> f64 {
    if t > T1 { t * t * t } else { T2 * (t - T0) }
}

fn xyz_to_rgb(x: f64) -> f64 {
    let x = if x <= 0.00304 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    };
    x.clamp(0.0, 1.0)
}

/// `[l, a, b, alpha]` of a straight-alpha color.
fn rgb_to_lab(c: Color) -> [f64; 4] {
    let (r, g, b) = (rgb_to_xyz(c.r), rgb_to_xyz(c.g), rgb_to_xyz(c.b));
    let y = xyz_to_lab((0.2225045 * r + 0.7168786 * g + 0.0606169 * b) / YN);
    let (x, z) = if r == g && g == b {
        (y, y)
    } else {
        (
            xyz_to_lab((0.4360747 * r + 0.3850649 * g + 0.1430804 * b) / XN),
            xyz_to_lab((0.0139322 * r + 0.0971045 * g + 0.7141733 * b) / ZN),
        )
    };
    let l = 116.0 * y - 16.0;
    [l.max(0.0), 500.0 * (x - y), 200.0 * (y - z), c.a]
}

fn lab_to_rgb([l, a, b, alpha]: [f64; 4]) -> Color {
    let y = (l + 16.0) / 116.0;
    let x = if a.is_nan() { y } else { y + a / 500.0 };
    let z = if b.is_nan() { y } else { y - b / 200.0 };
    let y = YN * lab_to_xyz(y);
    let x = XN * lab_to_xyz(x);
    let z = ZN * lab_to_xyz(z);
    Color::rgba(
        xyz_to_rgb(3.1338561 * x - 1.6168667 * y - 0.4906146 * z),
        xyz_to_rgb(-0.9787684 * x + 1.9161415 * y + 0.033454 * z),
        xyz_to_rgb(0.0719453 * x - 0.2289914 * y + 1.4052427 * z),
        alpha,
    )
}

/// `[h, c, l, alpha]`; the hue is `NaN` for grays.
fn rgb_to_hcl(c: Color) -> [f64; 4] {
    let [l, a, b, alpha] = rgb_to_lab(c);
    let chroma = (a * a + b * b).sqrt();
    let h = if (chroma * 10000.0).round() != 0.0 {
        constrain_angle(b.atan2(a).to_degrees())
    } else {
        f64::NAN
    };
    [h, chroma, l, alpha]
}

fn hcl_to_rgb([h, c, l, alpha]: [f64; 4]) -> Color {
    let h = if h.is_nan() { 0.0 } else { h.to_radians() };
    lab_to_rgb([l, h.cos() * c, h.sin() * c, alpha])
}

/// Blend two straight-alpha colors.
fn interpolate_color(from: Color, to: Color, t: f64, space: ColorSpace) -> Color {
    match space {
        ColorSpace::Rgb => Color::rgba(
            lerp(from.r, to.r, t),
            lerp(from.g, to.g, t),
            lerp(from.b, to.b, t),
            lerp(from.a, to.a, t),
        ),
        ColorSpace::Hcl => {
            let [hue0, chroma0, light0, alpha0] = rgb_to_hcl(from);
            let [hue1, chroma1, light1, alpha1] = rgb_to_hcl(to);
            let mut chroma = None;
            let hue = if !hue0.is_nan() && !hue1.is_nan() {
                let mut dh = hue1 - hue0;
                if hue1 > hue0 && dh > 180.0 {
                    dh -= 360.0;
                } else if hue1 < hue0 && hue0 - hue1 > 180.0 {
                    dh += 360.0;
                }
                hue0 + t * dh
            } else if !hue0.is_nan() {
                if light1 == 1.0 || light1 == 0.0 {
                    chroma = Some(chroma0);
                }
                hue0
            } else if !hue1.is_nan() {
                if light0 == 1.0 || light0 == 0.0 {
                    chroma = Some(chroma1);
                }
                hue1
            } else {
                f64::NAN
            };
            hcl_to_rgb([
                hue,
                chroma.unwrap_or_else(|| lerp(chroma0, chroma1, t)),
                lerp(light0, light1, t),
                lerp(alpha0, alpha1, t),
            ])
        }
        ColorSpace::Lab => {
            let a = rgb_to_lab(from);
            let b = rgb_to_lab(to);
            lab_to_rgb([
                lerp(a[0], b[0], t),
                lerp(a[1], b[1], t),
                lerp(a[2], b[2], t),
                lerp(a[3], b[3], t),
            ])
        }
    }
}

// Collation: a small stand-in for ICU, covering Latin letters with diacritics.

const DIACRITICS: &[(&str, char)] = &[
    ("àáâãäåāăą", 'a'),
    ("çćĉċč", 'c'),
    ("ďđ", 'd'),
    ("èéêëēĕėęě", 'e'),
    ("ĝğġģ", 'g'),
    ("ĥħ", 'h'),
    ("ìíîïĩīĭįı", 'i'),
    ("ĵ", 'j'),
    ("ķ", 'k'),
    ("ĺļľŀł", 'l'),
    ("ñńņňŉ", 'n'),
    ("òóôõöøōŏő", 'o'),
    ("ŕŗř", 'r'),
    ("śŝşšș", 's'),
    ("ţťŧț", 't'),
    ("ùúûüũūŭůűų", 'u'),
    ("ŵ", 'w'),
    ("ýÿŷ", 'y'),
    ("źżž", 'z'),
];

/// `(base letter, has a diacritic, is upper case)` of a character.
fn collation_key(c: char) -> (char, bool, bool) {
    let upper = c.is_uppercase();
    let lower = c.to_lowercase().next().unwrap_or(c);
    for (accented, base) in DIACRITICS {
        if accented.contains(lower) {
            return (*base, true, upper);
        }
    }
    (lower, false, upper)
}

/// Compare as a collator with the given sensitivities would: base letters first, then (if
/// diacritic-sensitive) accents, then (if case-sensitive) case, lower case first.
fn collate(a: &str, b: &str, case_sensitive: bool, diacritic_sensitive: bool) -> Ordering {
    let ka: Vec<(char, bool, bool)> = a.chars().map(collation_key).collect();
    let kb: Vec<(char, bool, bool)> = b.chars().map(collation_key).collect();
    let primary = ka.iter().map(|k| k.0).cmp(kb.iter().map(|k| k.0));
    if primary != Ordering::Equal {
        return primary;
    }
    if diacritic_sensitive {
        let secondary = ka.iter().map(|k| k.1).cmp(kb.iter().map(|k| k.1));
        if secondary != Ordering::Equal {
            return secondary;
        }
    }
    if case_sensitive {
        return ka.iter().map(|k| k.2).cmp(kb.iter().map(|k| k.2));
    }
    Ordering::Equal
}

// Number formatting: a small stand-in for `Intl.NumberFormat` in the `en` locale.

/// `(symbol, fraction digits)` of a currency; unknown codes print as the code itself.
fn currency_style(code: &str) -> (String, usize) {
    match code {
        "USD" => ("$".into(), 2),
        "EUR" => ("€".into(), 2),
        "GBP" => ("£".into(), 2),
        "JPY" => ("¥".into(), 0),
        "CNY" => ("CN¥".into(), 2),
        "INR" => ("₹".into(), 2),
        "KRW" => ("₩".into(), 0),
        "CAD" => ("CA$".into(), 2),
        "AUD" => ("A$".into(), 2),
        "CHF" => ("CHF\u{a0}".into(), 2),
        other => (format!("{other}\u{a0}"), 2),
    }
}

/// The short `en` suffix of a unit.
fn unit_suffix(unit: &str) -> String {
    match unit {
        "meter" => " m".into(),
        "kilometer" => " km".into(),
        "centimeter" => " cm".into(),
        "millimeter" => " mm".into(),
        "mile" => " mi".into(),
        "foot" => " ft".into(),
        "inch" => " in".into(),
        "yard" => " yd".into(),
        "celsius" => "°C".into(),
        "fahrenheit" => "°F".into(),
        "kilogram" => " kg".into(),
        "gram" => " g".into(),
        "pound" => " lb".into(),
        "liter" => " L".into(),
        "milliliter" => " mL".into(),
        "second" => " sec".into(),
        "minute" => " min".into(),
        "hour" => " hr".into(),
        "day" => " days".into(),
        "percent" => "%".into(),
        "byte" => " byte".into(),
        "kilobyte" => " kB".into(),
        "megabyte" => " MB".into(),
        "gigabyte" => " GB".into(),
        "kilometer-per-hour" => " km/h".into(),
        "mile-per-hour" => " mph".into(),
        "meter-per-second" => " m/s".into(),
        "degree" => " deg".into(),
        other => format!(" {other}"),
    }
}

/// `n` with digit grouping and `min..=max` fraction digits, rounding half away from zero on
/// the shortest decimal form of the number.
fn format_decimal(n: f64, min: usize, max: usize) -> String {
    if !n.is_finite() {
        return js_number(n);
    }
    let negative = n < 0.0;
    let text = js_number(n.abs());
    let plain = if text.contains('e') {
        format!("{:.*}", max, n.abs())
    } else {
        text
    };
    let (int_part, frac_part) = plain.split_once('.').unwrap_or((&plain, ""));
    let mut digits: Vec<u8> = int_part.bytes().map(|b| b - b'0').collect();
    let int_len = digits.len();
    let mut frac: Vec<u8> = frac_part.bytes().map(|b| b - b'0').collect();
    let round_up = frac.len() > max && frac[max] >= 5;
    frac.truncate(max);
    digits.extend(frac);
    let mut int_len = int_len;
    if round_up {
        let mut i = digits.len();
        loop {
            if i == 0 {
                digits.insert(0, 1);
                int_len += 1;
                break;
            }
            i -= 1;
            if digits[i] == 9 {
                digits[i] = 0;
            } else {
                digits[i] += 1;
                break;
            }
        }
    }
    let mut fraction: Vec<u8> = digits.split_off(int_len);
    while fraction.len() > min && fraction.last() == Some(&0) {
        fraction.pop();
    }
    while fraction.len() < min {
        fraction.push(0);
    }
    let mut grouped = String::new();
    for (i, d) in digits.iter().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push((b'0' + d) as char);
    }
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&grouped);
    if !fraction.is_empty() {
        out.push('.');
        out.extend(fraction.iter().map(|d| (b'0' + d) as char));
    }
    out
}

/// `Intl.NumberFormat`-style formatting with the `en` conventions.
fn format_number(
    n: f64,
    currency: Option<&str>,
    unit: Option<&str>,
    min_fraction: Option<f64>,
    max_fraction: Option<f64>,
) -> String {
    let (prefix, suffix, default_min, default_max) = match (currency, unit) {
        (Some(code), _) => {
            let (symbol, digits) = currency_style(code);
            (symbol, String::new(), digits, digits)
        }
        (None, Some(unit)) => (String::new(), unit_suffix(unit), 0, 3),
        (None, None) => (String::new(), String::new(), 0, 3),
    };
    let to_digits = |d: f64| js_int(d).clamp(0.0, 20.0) as usize;
    let (min, max) = match (min_fraction.map(to_digits), max_fraction.map(to_digits)) {
        (Some(min), Some(max)) => (min, max.max(min)),
        (Some(min), None) => (min, default_max.max(min)),
        (None, Some(max)) => (default_min.min(max), max),
        (None, None) => (default_min, default_max),
    };
    let body = format_decimal(n.abs(), min, max);
    let sign = if n < 0.0 { "-" } else { "" };
    format!("{sign}{prefix}{body}{suffix}")
}

impl Expr {
    fn eval(&self, ctx: &Context<'_>) -> Result<Value, RuntimeError> {
        match &self.kind {
            Kind::Literal(value) => Ok(value.clone()),
            Kind::Assert(args) => {
                for (i, arg) in args.iter().enumerate() {
                    let value = arg.eval(ctx)?;
                    let actual = type_of(&value);
                    if check_subtype(&self.ty, &actual).is_none() {
                        return Ok(value);
                    } else if i == args.len() - 1 {
                        return Err(RuntimeError::new(
                            &self.key,
                            format!(
                                "Expected value to be of type {}, but found {actual} instead.",
                                self.ty
                            ),
                        ));
                    }
                }
                Err(RuntimeError::new(&self.key, "Expected a value."))
            }
            Kind::Coerce(args) => self.eval_coercion(args, ctx),
            Kind::Compound(op, args) => self.eval_compound(*op, args, ctx),
            Kind::Let(_, result) => result.eval(ctx),
            Kind::Var(bound) => bound.eval(ctx),
            Kind::At(index, input) => {
                let index = as_num(&index.eval(ctx)?);
                let items = match input.eval(ctx)? {
                    Value::Array(items) => items,
                    other => {
                        return Err(RuntimeError::new(
                            &self.key,
                            format!(
                                "Expected value to be of type array, but found {} instead.",
                                type_of(&other)
                            ),
                        ));
                    }
                };
                if index < 0.0 {
                    return Err(RuntimeError::new(
                        &self.key,
                        format!("Array index out of bounds: {} < 0.", js_number(index)),
                    ));
                }
                if index >= items.len() as f64 {
                    return Err(RuntimeError::new(
                        &self.key,
                        format!(
                            "Array index out of bounds: {} > {}.",
                            js_number(index),
                            items.len() as f64 - 1.0
                        ),
                    ));
                }
                if index.fract() != 0.0 {
                    return Err(RuntimeError::new(
                        &self.key,
                        format!(
                            "Array index must be an integer, but found {} instead.",
                            js_number(index)
                        ),
                    ));
                }
                Ok(items[index as usize].clone())
            }
            Kind::In(needle, haystack) => {
                let needle = needle.eval(ctx)?;
                let haystack = haystack.eval(ctx)?;
                if !truthy(&haystack) {
                    return Ok(Value::Bool(false));
                }
                if !is_primitive(&needle) {
                    return Err(RuntimeError::new(
                        &self.key,
                        format!(
                            "Expected first argument to be of type boolean, string, number or null, but found {} instead.",
                            type_of(&needle)
                        ),
                    ));
                }
                match &haystack {
                    Value::String(s) => Ok(Value::Bool(s.contains(&js_string(&needle)))),
                    Value::Array(items) => {
                        Ok(Value::Bool(items.iter().any(|i| strict_eq(i, &needle))))
                    }
                    other => Err(RuntimeError::new(
                        &self.key,
                        format!(
                            "Expected second argument to be of type array or string, but found {} instead.",
                            type_of(other)
                        ),
                    )),
                }
            }
            Kind::IndexOf(needle, haystack, from) => {
                let needle = needle.eval(ctx)?;
                let haystack = haystack.eval(ctx)?;
                if !is_primitive(&needle) {
                    return Err(RuntimeError::new(
                        &self.key,
                        format!(
                            "Expected first argument to be of type boolean, string, number or null, but found {} instead.",
                            type_of(&needle)
                        ),
                    ));
                }
                let from = match from {
                    Some(from) => Some(js_int(as_num(&from.eval(ctx)?))),
                    None => None,
                };
                match &haystack {
                    Value::String(s) => {
                        let chars: Vec<char> = s.chars().collect();
                        let needle: Vec<char> = js_string(&needle).chars().collect();
                        let start = from.map_or(0, |f| f.max(0.0) as usize).min(chars.len());
                        let found = (start..=chars.len())
                            .find(|&i| chars[i..].starts_with(&needle))
                            .map_or(-1.0, |i| i as f64);
                        Ok(Value::Number(found))
                    }
                    Value::Array(items) => {
                        let start = match from {
                            Some(f) if f < 0.0 => (items.len() as f64 + f).max(0.0) as usize,
                            Some(f) => f as usize,
                            None => 0,
                        };
                        let found = items
                            .iter()
                            .enumerate()
                            .skip(start)
                            .find(|(_, item)| strict_eq(item, &needle))
                            .map_or(-1.0, |(i, _)| i as f64);
                        Ok(Value::Number(found))
                    }
                    other => Err(RuntimeError::new(
                        &self.key,
                        format!(
                            "Expected second argument to be of type array or string, but found {} instead.",
                            type_of(other)
                        ),
                    )),
                }
            }
            Kind::Slice(input, begin, end) => {
                let input = input.eval(ctx)?;
                let begin = as_num(&begin.eval(ctx)?);
                let end = match end {
                    Some(end) => Some(as_num(&end.eval(ctx)?)),
                    None => None,
                };
                match input {
                    Value::String(s) => {
                        let chars: Vec<char> = s.chars().collect();
                        let (b, e) = slice_bounds(chars.len(), begin, end);
                        Ok(Value::String(chars[b..e].iter().collect()))
                    }
                    Value::Array(items) => {
                        let (b, e) = slice_bounds(items.len(), begin, end);
                        Ok(Value::Array(items[b..e].to_vec()))
                    }
                    other => Err(RuntimeError::new(
                        &self.key,
                        format!(
                            "Expected first argument to be of type array or string, but found {} instead.",
                            type_of(&other)
                        ),
                    )),
                }
            }
            Kind::Length(input) => match input.eval(ctx)? {
                Value::String(s) => Ok(Value::Number(s.chars().count() as f64)),
                Value::Array(items) => Ok(Value::Number(items.len() as f64)),
                other => Err(RuntimeError::new(
                    &self.key,
                    format!(
                        "Expected value to be of type string or array, but found {} instead.",
                        type_of(&other)
                    ),
                )),
            },
            Kind::Case(branches, otherwise) => {
                for (test, result) in branches {
                    if truthy(&test.eval(ctx)?) {
                        return result.eval(ctx);
                    }
                }
                otherwise.eval(ctx)
            }
            Kind::Coalesce(args) => {
                let mut result = Value::Null;
                let mut requested_image: Option<String> = None;
                for (i, arg) in args.iter().enumerate() {
                    result = arg.eval(ctx)?;
                    // An unavailable image does not count; if none is available the first
                    // requested name is the result, so a missing-image handler can fetch it.
                    if let Value::Image {
                        name,
                        available: false,
                    } = &result
                    {
                        requested_image.get_or_insert_with(|| name.clone());
                        result = Value::Null;
                        if i == args.len() - 1
                            && let Some(name) = requested_image.take()
                        {
                            result = Value::String(name);
                        }
                    }
                    if result != Value::Null {
                        break;
                    }
                }
                Ok(result)
            }
            Kind::Match {
                input,
                input_type,
                cases,
                outputs,
                otherwise,
            } => {
                let input = input.eval(ctx)?;
                let output = if type_of(&input).same_kind(input_type) {
                    let label = match &input {
                        Value::Number(n) => js_number(*n),
                        Value::String(s) => s.clone(),
                        _ => String::new(),
                    };
                    cases.get(&label).and_then(|i| outputs.get(*i))
                } else {
                    None
                };
                output.unwrap_or(otherwise).eval(ctx)
            }
            Kind::Step {
                input,
                labels,
                outputs,
            } => {
                if labels.len() == 1 {
                    return outputs[0].eval(ctx);
                }
                let value = as_num(&input.eval(ctx)?);
                if value <= labels[0] {
                    return outputs[0].eval(ctx);
                }
                if value >= labels[labels.len() - 1] {
                    return outputs[outputs.len() - 1].eval(ctx);
                }
                let index = find_stop(labels, value, &self.key)?;
                outputs[index].eval(ctx)
            }
            Kind::Interpolate {
                space,
                interpolation,
                input,
                labels,
                outputs,
            } => {
                if labels.len() == 1 {
                    return outputs[0].eval(ctx);
                }
                let value = as_num(&input.eval(ctx)?);
                if value <= labels[0] {
                    return outputs[0].eval(ctx);
                }
                if value >= labels[labels.len() - 1] {
                    return outputs[outputs.len() - 1].eval(ctx);
                }
                let index = find_stop(labels, value, &self.key)?;
                let t =
                    interpolation_factor(*interpolation, value, labels[index], labels[index + 1]);
                let lower = outputs[index].eval(ctx)?;
                let upper = outputs[index + 1].eval(ctx)?;
                Ok(match (lower, upper) {
                    (Value::Number(a), Value::Number(b)) => Value::Number(lerp(a, b, t)),
                    (Value::Color(a), Value::Color(b)) => {
                        Value::Color(interpolate_color(a, b, t, *space))
                    }
                    (Value::Array(a), Value::Array(b)) => Value::Array(
                        a.iter()
                            .zip(b.iter())
                            .map(|(x, y)| Value::Number(lerp(as_num(x), as_num(y), t)))
                            .collect(),
                    ),
                    (lower, _) => lower,
                })
            }
            Kind::Compare {
                op,
                lhs,
                rhs,
                collator,
                untyped,
            } => {
                let lhs = lhs.eval(ctx)?;
                let rhs = rhs.eval(ctx)?;
                let (lt, rt) = (type_of(&lhs), type_of(&rhs));
                if op.is_order()
                    && *untyped
                    && (!lt.same_kind(&rt) || !matches!(lt, Type::String | Type::Number))
                {
                    return Err(RuntimeError::new(
                        &self.key,
                        format!(
                            "Expected arguments for \"{}\" to be (string, string) or (number, number), but found ({lt}, {rt}) instead.",
                            op.symbol()
                        ),
                    ));
                }
                let basic = |op: CmpOp| match op {
                    CmpOp::Eq => strict_eq(&lhs, &rhs),
                    CmpOp::Ne => !strict_eq(&lhs, &rhs),
                    _ => match (&lhs, &rhs) {
                        (Value::Number(a), Value::Number(b)) => {
                            a.partial_cmp(b).is_some_and(|o| op.holds(o))
                        }
                        (Value::String(a), Value::String(b)) => op.holds(a.cmp(b)),
                        _ => false,
                    },
                };
                let Some(collator) = collator else {
                    return Ok(Value::Bool(basic(*op)));
                };
                let (Value::String(a), Value::String(b)) = (&lhs, &rhs) else {
                    return Ok(Value::Bool(basic(*op)));
                };
                match collator.eval(ctx)? {
                    Value::Collator {
                        case_sensitive,
                        diacritic_sensitive,
                        ..
                    } => Ok(Value::Bool(op.holds(collate(
                        a,
                        b,
                        case_sensitive,
                        diacritic_sensitive,
                    )))),
                    _ => Ok(Value::Bool(basic(*op))),
                }
            }
            Kind::Collator {
                case_sensitive,
                diacritic_sensitive,
                locale,
            } => Ok(Value::Collator {
                case_sensitive: truthy(&case_sensitive.eval(ctx)?),
                diacritic_sensitive: truthy(&diacritic_sensitive.eval(ctx)?),
                locale: match locale {
                    Some(locale) => Some(value_to_string(&locale.eval(ctx)?)),
                    None => None,
                },
            }),
            Kind::NumberFormat(args) => {
                let n = as_num(&args.number.eval(ctx)?);
                let text = |e: &Option<Expr>| -> Result<Option<String>, RuntimeError> {
                    match e {
                        Some(e) => Ok(Some(value_to_string(&e.eval(ctx)?))),
                        None => Ok(None),
                    }
                };
                let number = |e: &Option<Expr>| -> Result<Option<f64>, RuntimeError> {
                    match e {
                        Some(e) => Ok(Some(as_num(&e.eval(ctx)?))),
                        None => Ok(None),
                    }
                };
                let currency = text(&args.currency)?;
                let unit = text(&args.unit)?;
                Ok(Value::String(format_number(
                    n,
                    currency.as_deref(),
                    unit.as_deref(),
                    number(&args.min_fraction_digits)?,
                    number(&args.max_fraction_digits)?,
                )))
            }
            Kind::Format(sections) => {
                let mut out = Vec::with_capacity(sections.len());
                for section in sections {
                    let content = section.content.eval(ctx)?;
                    let vertical_align = match &section.vertical_align {
                        Some(e) => Some(value_to_string(&e.eval(ctx)?)),
                        None => None,
                    };
                    if let Value::Image { name, .. } = content {
                        out.push(FormattedSection {
                            text: String::new(),
                            image: Some(name),
                            scale: None,
                            font_stack: None,
                            text_color: None,
                            vertical_align,
                        });
                        continue;
                    }
                    let scale = match &section.scale {
                        Some(e) => Some(as_num(&e.eval(ctx)?)),
                        None => None,
                    };
                    let font_stack = match &section.font {
                        Some(e) => match e.eval(ctx)? {
                            Value::Array(items) => {
                                Some(items.iter().map(value_to_string).collect())
                            }
                            other => Some(vec![value_to_string(&other)]),
                        },
                        None => None,
                    };
                    let text_color = match &section.text_color {
                        Some(e) => e.eval(ctx)?.as_color(),
                        None => None,
                    };
                    out.push(FormattedSection {
                        text: value_to_string(&content),
                        image: None,
                        scale,
                        font_stack,
                        text_color,
                        vertical_align,
                    });
                }
                Ok(Value::Formatted(out))
            }
            Kind::Image(name) => {
                let name = value_to_string(&name.eval(ctx)?);
                if name.is_empty() {
                    return Ok(Value::Null);
                }
                let available = ctx
                    .available_images
                    .is_some_and(|images| images.contains(&name));
                Ok(Value::Image { name, available })
            }
            Kind::GlobalState(key) => Ok(ctx
                .global_state
                .filter(|state| !state.is_empty())
                .and_then(|state| state.get(key))
                .cloned()
                .unwrap_or(Value::Null)),
            Kind::Semiliteral(items) => Ok(Value::Array(
                items
                    .iter()
                    .map(|item| item.eval(ctx))
                    .collect::<Result<Vec<Value>, RuntimeError>>()?,
            )),
        }
    }

    fn eval_coercion(&self, args: &[Expr], ctx: &Context<'_>) -> Result<Value, RuntimeError> {
        match &self.ty {
            Type::Boolean => Ok(Value::Bool(truthy(&args[0].eval(ctx)?))),
            Type::Color => {
                let mut error = None;
                let mut input = Value::Null;
                for arg in args {
                    input = arg.eval(ctx)?;
                    error = None;
                    match &input {
                        Value::Color(c) => return Ok(Value::Color(*c)),
                        Value::String(s) => {
                            if let Some(c) = parse_color(s) {
                                return Ok(Value::Color(c));
                            }
                        }
                        Value::Array(items) => {
                            if items.len() < 3 || items.len() > 4 {
                                error = Some(format!(
                                    "Invalid rgba value {}: expected an array containing either three or four numeric values.",
                                    json_text(&input)
                                ));
                            } else {
                                let n = |i: usize| items.get(i).map(as_num);
                                let (r, g, b) = (
                                    n(0).unwrap_or(f64::NAN),
                                    n(1).unwrap_or(f64::NAN),
                                    n(2).unwrap_or(f64::NAN),
                                );
                                error = validate_rgba(r, g, b, n(3));
                                if error.is_none() {
                                    return Ok(Value::Color(Color::rgba(
                                        r / 255.0,
                                        g / 255.0,
                                        b / 255.0,
                                        n(3).unwrap_or(1.0),
                                    )));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                Err(RuntimeError::new(
                    &self.key,
                    error.unwrap_or_else(|| {
                        format!(
                            "Could not parse color from value '{}'",
                            match &input {
                                Value::String(s) => s.clone(),
                                other => json_text(other),
                            }
                        )
                    }),
                ))
            }
            Type::Number => {
                let mut value = Value::Null;
                for arg in args {
                    value = arg.eval(ctx)?;
                    if value == Value::Null {
                        return Ok(Value::Number(0.0));
                    }
                    let n = js_to_number(&value);
                    if !n.is_nan() {
                        return Ok(Value::Number(n));
                    }
                }
                Err(RuntimeError::new(
                    &self.key,
                    format!("Could not convert {} to number.", json_text(&value)),
                ))
            }
            Type::Formatted => Ok(Value::Formatted(vec![FormattedSection {
                text: value_to_string(&args[0].eval(ctx)?),
                image: None,
                scale: None,
                font_stack: None,
                text_color: None,
                vertical_align: None,
            }])),
            Type::ResolvedImage => {
                let name = value_to_string(&args[0].eval(ctx)?);
                if name.is_empty() {
                    Ok(Value::Null)
                } else {
                    Ok(Value::Image {
                        name,
                        available: false,
                    })
                }
            }
            _ => Ok(Value::String(value_to_string(&args[0].eval(ctx)?))),
        }
    }

    fn eval_compound(
        &self,
        op: Op,
        args: &[Expr],
        ctx: &Context<'_>,
    ) -> Result<Value, RuntimeError> {
        let num = |i: usize| -> Result<f64, RuntimeError> { Ok(as_num(&args[i].eval(ctx)?)) };
        let text =
            |i: usize| -> Result<String, RuntimeError> { Ok(value_to_string(&args[i].eval(ctx)?)) };
        let numbers = || -> Result<Vec<f64>, RuntimeError> {
            args.iter().map(|a| Ok(as_num(&a.eval(ctx)?))).collect()
        };
        let properties = || ctx.feature.map(|f| &f.properties);
        Ok(match op {
            Op::Error => return Err(RuntimeError::new(&self.key, text(0)?)),
            Op::Typeof => Value::String(type_of(&args[0].eval(ctx)?).name()),
            Op::ToRgba => {
                let c = args[0].eval(ctx)?.as_color().unwrap_or(Color::CLEAR);
                Value::Array(vec![
                    Value::Number(c.r * 255.0),
                    Value::Number(c.g * 255.0),
                    Value::Number(c.b * 255.0),
                    Value::Number(c.a),
                ])
            }
            Op::Rgb | Op::Rgba => {
                let (r, g, b) = (num(0)?, num(1)?, num(2)?);
                let a = if args.len() > 3 { num(3)? } else { 1.0 };
                if let Some(err) = validate_rgba(r, g, b, Some(a)) {
                    return Err(RuntimeError::new(&self.key, err));
                }
                Value::Color(Color::rgba(r / 255.0, g / 255.0, b / 255.0, a))
            }
            Op::Has | Op::Get => {
                let key = text(0)?;
                let object;
                let map: Option<&BTreeMap<String, Value>> = if args.len() == 2 {
                    object = args[1].eval(ctx)?;
                    match &object {
                        Value::Object(map) => Some(map),
                        _ => None,
                    }
                } else {
                    properties()
                };
                if op == Op::Has {
                    Value::Bool(map.is_some_and(|m| m.contains_key(&key)))
                } else {
                    map.and_then(|m| m.get(&key))
                        .cloned()
                        .unwrap_or(Value::Null)
                }
            }
            Op::FeatureState => {
                let key = text(0)?;
                ctx.feature_state
                    .and_then(|s| s.get(&key))
                    .cloned()
                    .unwrap_or(Value::Null)
            }
            Op::Properties => Value::Object(properties().cloned().unwrap_or_default()),
            Op::GeometryType => match ctx.feature {
                Some(f) => Value::String(f.kind.name().into()),
                None => Value::Null,
            },
            Op::Id => ctx
                .feature
                .and_then(|f| f.id.clone())
                .unwrap_or(Value::Null),
            Op::Zoom => Value::Number(ctx.zoom),
            Op::Add => Value::Number(numbers()?.iter().sum()),
            Op::Mul => Value::Number(numbers()?.iter().product()),
            Op::Sub => Value::Number(if args.len() == 2 {
                num(0)? - num(1)?
            } else {
                -num(0)?
            }),
            Op::Div => Value::Number(num(0)? / num(1)?),
            Op::Mod => Value::Number(num(0)? % num(1)?),
            Op::Ln2 => Value::Number(std::f64::consts::LN_2),
            Op::Pi => Value::Number(std::f64::consts::PI),
            Op::E => Value::Number(std::f64::consts::E),
            Op::Pow => Value::Number(num(0)?.powf(num(1)?)),
            Op::Sqrt => Value::Number(num(0)?.sqrt()),
            Op::Log10 => Value::Number(num(0)?.ln() / std::f64::consts::LN_10),
            Op::Ln => Value::Number(num(0)?.ln()),
            Op::Log2 => Value::Number(num(0)?.ln() / std::f64::consts::LN_2),
            Op::Sin => Value::Number(num(0)?.sin()),
            Op::Cos => Value::Number(num(0)?.cos()),
            Op::Tan => Value::Number(num(0)?.tan()),
            Op::Asin => Value::Number(num(0)?.asin()),
            Op::Acos => Value::Number(num(0)?.acos()),
            Op::Atan => Value::Number(num(0)?.atan()),
            Op::Min => Value::Number(numbers()?.iter().fold(f64::INFINITY, |a, b| a.min(*b))),
            Op::Max => Value::Number(numbers()?.iter().fold(f64::NEG_INFINITY, |a, b| a.max(*b))),
            Op::Abs => Value::Number(num(0)?.abs()),
            Op::Round => Value::Number(num(0)?.round()),
            Op::Floor => Value::Number(num(0)?.floor()),
            Op::Ceil => Value::Number(num(0)?.ceil()),
            Op::All => {
                for arg in args {
                    if !truthy(&arg.eval(ctx)?) {
                        return Ok(Value::Bool(false));
                    }
                }
                Value::Bool(true)
            }
            Op::Any => {
                for arg in args {
                    if truthy(&arg.eval(ctx)?) {
                        return Ok(Value::Bool(true));
                    }
                }
                Value::Bool(false)
            }
            Op::Not => Value::Bool(!truthy(&args[0].eval(ctx)?)),
            Op::Upcase => Value::String(text(0)?.to_uppercase()),
            Op::Downcase => Value::String(text(0)?.to_lowercase()),
            Op::Concat => {
                let mut out = String::new();
                for arg in args {
                    out.push_str(&value_to_string(&arg.eval(ctx)?));
                }
                Value::String(out)
            }
            Op::Split => {
                let s = text(0)?;
                let delimiter = text(1)?;
                let parts: Vec<Value> = if delimiter.is_empty() {
                    s.chars().map(|c| Value::String(c.to_string())).collect()
                } else {
                    s.split(delimiter.as_str())
                        .map(|p| Value::String(p.to_string()))
                        .collect()
                };
                Value::Array(parts)
            }
            Op::Join => {
                let items = match args[0].eval(ctx)? {
                    Value::Array(items) => items,
                    _ => Vec::new(),
                };
                let delimiter = text(1)?;
                let parts: Vec<String> = items.iter().map(js_string).collect();
                Value::String(parts.join(&delimiter))
            }
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The public expression
// ---------------------------------------------------------------------------------------------

/// A compiled expression.
#[derive(Clone, Debug)]
pub struct Expression {
    root: Expr,
    feature_constant: bool,
    zoom_constant: bool,
    /// The value a failed evaluation produces instead of an error: `false` for filters, which
    /// the reference evaluates with its warning-and-default path.
    fallback: Option<Value>,
}

impl Expression {
    fn from_root(root: Expr) -> Expression {
        Expression {
            feature_constant: is_feature_constant(&root),
            zoom_constant: is_zoom_constant(&root),
            root,
            fallback: None,
        }
    }

    fn run_parser(json: &Json, expected: Option<&Type>) -> Result<Expr, Error> {
        // String-valued properties coerce at the top level rather than asserting; enum
        // properties are typed as strings but keep the assertion.
        let annotation = if expected == Some(&Type::String) {
            Annotation::Coerce
        } else {
            Annotation::Assert
        };
        let expected = match expected {
            Some(Type::Enum(_)) => Some(&Type::String),
            Some(Type::Value) => None,
            other => other,
        };
        let mut parser = Parser {
            path: Vec::new(),
            scope: Vec::new(),
            errors: Vec::new(),
        };
        match parser.parse(json, None, expected, annotation) {
            Some(root) if parser.errors.is_empty() => Ok(root),
            _ => {
                if let Some(err) = parser.errors.iter().find(|e| e.unsupported) {
                    return Err(Error::Unsupported(err.message.clone()));
                }
                let messages: Vec<String> = parser
                    .errors
                    .iter()
                    .map(|e| format!("expression{}: {}", e.key, e.message))
                    .collect();
                Err(Error::Invalid(if messages.is_empty() {
                    "expression: could not be parsed".into()
                } else {
                    messages.join("; ")
                }))
            }
        }
    }

    /// Parse an expression (`["get", "name"]`, a literal, or a legacy function object when
    /// `spec` is given) that must produce `expected`, where known.
    pub fn parse(json: &Json, expected: Option<&Type>) -> Result<Expression, Error> {
        Ok(Expression::from_root(Expression::run_parser(
            json, expected,
        )?))
    }

    /// Parse a layer property's value against its spec: a constant (a scalar, or an array that
    /// names no operator), an expression, or a legacy function (`{"stops": …}`), which is
    /// converted to the equivalent expression.
    ///
    /// Beyond [`Expression::parse`], this enforces the property rules: a feature-dependent
    /// expression on a property that is not data-driven is an error, and `["zoom"]` may only
    /// feed a top-level `step` or `interpolate` (through `let` and `coalesce`).
    pub fn parse_property(json: &Json, spec: &PropertySpec) -> Result<Expression, Error> {
        Expression::parse_property_with(json, spec, spec.function_hints())
    }

    /// [`Expression::parse_property`] with explicit hints for a legacy function.
    pub fn parse_property_with(
        json: &Json,
        spec: &PropertySpec,
        hints: FunctionHints,
    ) -> Result<Expression, Error> {
        let converted;
        let json = if json.is_object() {
            converted = convert_function(json, spec, hints)?;
            &converted
        } else if json.is_array() && !is_expression(json) {
            // A bare array that names no operator is a constant (`"text-font": ["Open Sans"]`,
            // `"line-dasharray": [2, 1]`), type-checked against the property like a literal.
            converted = serde_json::json!(["literal", json]);
            &converted
        } else {
            json
        };
        let expression = Expression::parse(json, Some(&spec.kind))?;
        if !expression.feature_constant && !spec.data_driven {
            return Err(Error::Invalid("data expressions not supported".into()));
        }
        match find_zoom_curve(&expression.root) {
            Err(message) => return Err(Error::Invalid(message)),
            Ok(None) if !expression.zoom_constant => {
                return Err(Error::Invalid(ZOOM_TOP_LEVEL.into()));
            }
            Ok(_) => {}
        }
        Ok(expression)
    }

    /// Parse a layer `filter`: an expression, or the legacy filter syntax (`["==", "class",
    /// "street"]`, `["all", …]`, `["in", "k", …]`, `["has", "k"]`, `["!in"]`, …), which is
    /// converted to the equivalent boolean expression.
    ///
    /// A filter never fails to evaluate: a runtime error (a type mismatch in a comparison, say)
    /// makes it `false`, as the legacy syntax always was for mismatched types.
    pub fn parse_filter(json: &Json) -> Result<Expression, Error> {
        let converted;
        let json = if json.is_null() {
            &Json::Bool(true)
        } else if is_expression_filter(json) {
            json
        } else {
            converted = convert_filter(json, &mut Vec::new());
            &converted
        };
        let mut expression = Expression::parse(json, Some(&Type::Boolean))?;
        expression.fallback = Some(Value::Bool(false));
        Ok(expression)
    }

    /// An expression that always produces `value`.
    pub fn constant(value: Value) -> Expression {
        Expression::from_root(Expr::literal(type_of(&value), "", value))
    }

    /// Evaluate against `ctx`.
    pub fn evaluate(&self, ctx: &Context<'_>) -> Result<Value, Error> {
        match self.root.eval(ctx) {
            Ok(value) => Ok(value),
            Err(err) => match &self.fallback {
                Some(fallback) => Ok(fallback.clone()),
                None => Err(Error::Invalid(format!(
                    "expression{}: {}",
                    err.key, err.message
                ))),
            },
        }
    }

    /// Whether the result never depends on the zoom.
    pub fn is_zoom_constant(&self) -> bool {
        self.zoom_constant
    }

    /// Whether the result never depends on the feature.
    pub fn is_feature_constant(&self) -> bool {
        self.feature_constant
    }

    /// The constant this expression is, if it is one.
    pub fn as_constant(&self) -> Option<&Value> {
        match &self.root.kind {
            Kind::Literal(value) => Some(value),
            _ => None,
        }
    }

    /// The static type of the result.
    pub fn result_type(&self) -> &Type {
        &self.root.ty
    }
}

// ---------------------------------------------------------------------------------------------
// Legacy filters (`feature_filter/convert.ts`)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum FilterSyntax {
    Expression,
    Legacy,
    /// Reads the same both ways (`["has", "x"]`, a bare boolean), so it decides nothing.
    Neutral,
}

fn classify_filter(filter: &Json) -> FilterSyntax {
    if filter.is_boolean() {
        return FilterSyntax::Neutral;
    }
    let Some(items) = filter.as_array().filter(|a| !a.is_empty()) else {
        return FilterSyntax::Legacy;
    };
    let special = |v: &Json| v == "$id" || v == "$type";
    match items[0].as_str() {
        Some("has") => {
            if items.len() < 2 || special(&items[1]) {
                FilterSyntax::Legacy
            } else if items.len() == 2 {
                FilterSyntax::Neutral
            } else {
                FilterSyntax::Expression
            }
        }
        // Legacy `in` tests a property against values; the expression tests for a substring, so
        // the scalar form stays legacy.
        Some("in") => {
            if items.len() >= 3 && (!items[1].is_string() || items[2].is_array()) {
                FilterSyntax::Expression
            } else {
                FilterSyntax::Legacy
            }
        }
        Some("!in" | "!has" | "none") => FilterSyntax::Legacy,
        Some("==" | "!=" | ">" | ">=" | "<" | "<=") => {
            if items.len() != 3 || items[1].is_array() || items[2].is_array() {
                FilterSyntax::Expression
            } else {
                FilterSyntax::Legacy
            }
        }
        Some("any" | "all") => {
            let mut saw_legacy = false;
            for child in &items[1..] {
                match classify_filter(child) {
                    FilterSyntax::Expression => return FilterSyntax::Expression,
                    FilterSyntax::Legacy => saw_legacy = true,
                    FilterSyntax::Neutral => {}
                }
            }
            if saw_legacy {
                FilterSyntax::Legacy
            } else {
                FilterSyntax::Neutral
            }
        }
        _ => FilterSyntax::Expression,
    }
}

/// Whether a filter is written in expression syntax (as opposed to the deprecated one).
pub fn is_expression_filter(filter: &Json) -> bool {
    classify_filter(filter) != FilterSyntax::Legacy
}

/// The expected JavaScript type of a property, for the preflight type checks `any` needs.
type ExpectedTypes = Vec<(String, String)>;

fn note_expected_type(types: &mut ExpectedTypes, property: &str, value: &Json) {
    let kind = js_typeof(value).to_string();
    match types.iter_mut().find(|(p, _)| p == property) {
        Some(entry) => entry.1 = kind,
        None => types.push((property.to_string(), kind)),
    }
}

fn property_expression(property: &str) -> Json {
    match property {
        "$type" => serde_json::json!(["geometry-type"]),
        "$id" => serde_json::json!(["id"]),
        _ => serde_json::json!(["get", property]),
    }
}

/// Convert a legacy filter to an expression, recording the type each property is compared
/// against in `expected_types`.
///
/// Legacy comparisons on a property of the wrong type are simply false, where the expression
/// raises an error and fails the whole filter; inside an `any` that would hide the other terms,
/// so each `any` term is guarded by a `typeof` check of the properties it reads.
pub fn convert_filter(filter: &Json, expected_types: &mut ExpectedTypes) -> Json {
    if is_expression_filter(filter) {
        return filter.clone();
    }
    let Some(items) = filter.as_array() else {
        return Json::Bool(true);
    };
    let op = items.first().and_then(Json::as_str).unwrap_or("");
    if items.len() <= 1 {
        return Json::Bool(op != "any");
    }
    let property = || items[1].as_str().unwrap_or("");
    match op {
        "==" | "!=" | "<" | ">" | "<=" | ">=" => {
            let value = items.get(2).unwrap_or(&Json::Null);
            convert_comparison(property(), value, op, Some(expected_types))
        }
        "any" => {
            let children: Vec<Json> = items[1..]
                .iter()
                .map(|child| {
                    let mut types = Vec::new();
                    let converted = convert_filter(child, &mut types);
                    match runtime_type_checks(&types) {
                        Json::Bool(true) => converted,
                        checks => serde_json::json!(["case", checks, converted, false]),
                    }
                })
                .collect();
            let mut out = vec![Json::String("any".into())];
            out.extend(children);
            Json::Array(out)
        }
        "all" => {
            let children: Vec<Json> = items[1..]
                .iter()
                .map(|child| convert_filter(child, expected_types))
                .collect();
            if children.len() > 1 {
                let mut out = vec![Json::String("all".into())];
                out.extend(children);
                Json::Array(out)
            } else {
                children.into_iter().next().unwrap_or(Json::Bool(true))
            }
        }
        "none" => {
            let mut any = vec![Json::String("any".into())];
            any.extend(items[1..].iter().cloned());
            serde_json::json!(["!", convert_filter(&Json::Array(any), &mut Vec::new())])
        }
        "in" => convert_in(property(), &items[2..], false),
        "!in" => convert_in(property(), &items[2..], true),
        "has" => convert_has(property()),
        "!has" => serde_json::json!(["!", convert_has(property())]),
        _ => Json::Bool(true),
    }
}

fn runtime_type_checks(types: &ExpectedTypes) -> Json {
    let mut conditions: Vec<Json> = types
        .iter()
        .map(|(property, kind)| {
            let get = if property == "$id" {
                serde_json::json!(["id"])
            } else {
                serde_json::json!(["get", property])
            };
            serde_json::json!(["==", ["typeof", get], kind])
        })
        .collect();
    match conditions.len() {
        0 => Json::Bool(true),
        1 => conditions.remove(0),
        _ => {
            let mut out = vec![Json::String("all".into())];
            out.extend(conditions);
            Json::Array(out)
        }
    }
}

fn convert_comparison(
    property: &str,
    value: &Json,
    op: &str,
    expected_types: Option<&mut ExpectedTypes>,
) -> Json {
    if property == "$type" {
        return serde_json::json!([op, ["geometry-type"], value]);
    }
    let get = property_expression(property);
    if let (Some(types), false) = (expected_types, value.is_null()) {
        note_expected_type(types, property, value);
    }
    if property != "$id" && value.is_null() {
        // A missing property is not null for legacy filters.
        if op == "==" {
            return serde_json::json!(["all", ["has", property], ["==", get, null]]);
        } else if op == "!=" {
            return serde_json::json!(["any", ["!", ["has", property]], ["!=", get, null]]);
        }
    }
    serde_json::json!([op, get, value])
}

fn convert_in(property: &str, values: &[Json], negate: bool) -> Json {
    if values.is_empty() {
        return Json::Bool(negate);
    }
    let get = property_expression(property);
    let kind = js_typeof(&values[0]);
    let uniform = values.iter().all(|v| js_typeof(v) == kind);
    if uniform && (kind == "string" || kind == "number") {
        // A match needs unique labels; JavaScript's default sort orders by string form.
        let mut sorted: Vec<Json> = values.to_vec();
        sorted.sort_by_cached_key(|v| js_string(&Value::from_json(v)));
        sorted.dedup();
        return serde_json::json!(["match", get, sorted, !negate, negate]);
    }
    let op = if negate { "!=" } else { "==" };
    let mut out = vec![Json::String(if negate { "all" } else { "any" }.into())];
    out.extend(values.iter().map(|v| serde_json::json!([op, get, v])));
    Json::Array(out)
}

fn convert_has(property: &str) -> Json {
    match property {
        "$type" => Json::Bool(true),
        "$id" => serde_json::json!(["!=", ["id"], null]),
        _ => serde_json::json!(["has", property]),
    }
}

// ---------------------------------------------------------------------------------------------
// Legacy functions (`function/convert.ts`)
// ---------------------------------------------------------------------------------------------

/// `["literal", v]` for arrays and objects (and null), the value itself otherwise.
fn convert_literal(value: &Json) -> Json {
    match value {
        Json::Array(_) | Json::Object(_) | Json::Null => serde_json::json!(["literal", value]),
        other => other.clone(),
    }
}

/// The expression operator a spec type's assertion/coercion is spelled with.
fn spec_type_name(spec: &PropertySpec) -> &'static str {
    match &spec.kind {
        Type::Color => "to-color",
        Type::Number => "number",
        Type::Boolean => "boolean",
        Type::String | Type::Enum(_) => "string",
        Type::Array(..) => "array",
        Type::Object => "object",
        Type::Formatted => "formatted",
        Type::ResolvedImage => "resolvedImage",
        _ => "value",
    }
}

fn spec_default(spec: &PropertySpec) -> Option<Json> {
    spec.default.as_ref().map(value_to_json)
}

fn value_to_json(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Number(n) => serde_json::Number::from_f64(*n).map_or(Json::Null, Json::Number),
        Value::String(s) => Json::String(s.clone()),
        Value::Array(items) => Json::Array(items.iter().map(value_to_json).collect()),
        Value::Object(map) => Json::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect(),
        ),
        Value::Color(c) => Json::String(color_to_string(*c)),
        other => Json::String(value_to_string(other)),
    }
}

/// Convert a legacy function object to the equivalent expression.
pub fn convert_function(
    parameters: &Json,
    spec: &PropertySpec,
    hints: FunctionHints,
) -> Result<Json, Error> {
    let Some(stops) = parameters.get("stops").and_then(Json::as_array) else {
        return Ok(convert_identity_function(parameters, spec));
    };
    let zoom_and_feature = stops
        .first()
        .and_then(|s| s.get(0))
        .is_some_and(Json::is_object);
    let feature_dependent = zoom_and_feature || parameters.get("property").is_some();
    let zoom_dependent = zoom_and_feature || !feature_dependent;
    let stops: Vec<(Json, Json)> = stops
        .iter()
        .map(|stop| {
            let input = stop.get(0).cloned().unwrap_or(Json::Null);
            let output = stop.get(1).unwrap_or(&Json::Null);
            let output = match output.as_str() {
                Some(s) if !feature_dependent && hints.tokens => convert_token_string(s),
                _ => convert_literal(output),
            };
            (input, output)
        })
        .collect();
    if zoom_and_feature {
        convert_zoom_and_property_function(parameters, spec, hints, &stops)
    } else if zoom_dependent {
        convert_zoom_function(parameters, spec, hints, &stops)
    } else {
        convert_property_function(parameters, spec, hints, &stops)
    }
}

fn convert_identity_function(parameters: &Json, spec: &PropertySpec) -> Json {
    let get = serde_json::json!([
        "get",
        parameters.get("property").cloned().unwrap_or(Json::Null)
    ]);
    let Some(default) = parameters.get("default") else {
        // String-valued properties would be coerced; legacy functions assert instead.
        return if matches!(spec.kind, Type::String) {
            serde_json::json!(["string", get])
        } else {
            get
        };
    };
    if let Type::Enum(values) = &spec.kind {
        return serde_json::json!(["match", get, values, get, default]);
    }
    if let Type::Array(_, length) = &spec.kind {
        let item = spec.kind.item().name();
        return serde_json::json!(["array", item, length, get, convert_literal(default)]);
    }
    serde_json::json!([spec_type_name(spec), get, convert_literal(default)])
}

fn interpolate_operator(parameters: &Json) -> &'static str {
    match parameters.get("colorSpace").and_then(Json::as_str) {
        Some("hcl") => "interpolate-hcl",
        Some("lab") => "interpolate-lab",
        _ => "interpolate",
    }
}

fn function_type(parameters: &Json, hints: FunctionHints) -> &str {
    match parameters.get("type").and_then(Json::as_str) {
        Some(kind) => kind,
        None if hints.interpolated => "exponential",
        None => "interval",
    }
}

/// The output of a function when no stop matches.
fn fallback(parameters: &Json, spec: &PropertySpec) -> Json {
    let default = parameters
        .get("default")
        .cloned()
        .or_else(|| spec_default(spec));
    match default {
        Some(value) => convert_literal(&value),
        // Image properties have no default; an empty name is "no image".
        None if spec.kind == Type::ResolvedImage => Json::String(String::new()),
        None => Json::Null,
    }
}

/// Append `(input, output)` to a curve, skipping duplicate inputs (which functions never
/// validated) and the first input of a `step` (which is implied).
fn append_stop(curve: &mut Vec<Json>, input: &Json, output: &Json, is_step: bool) {
    if curve.len() > 3 && curve[curve.len() - 2] == *input {
        return;
    }
    if !(is_step && curve.len() == 2) {
        curve.push(input.clone());
    }
    curve.push(output.clone());
}

/// A `step` with a single output is given a no-op stop so it is well formed.
fn fixup_degenerate_step(curve: &mut Vec<Json>) {
    if curve.first().is_some_and(|op| op == "step") && curve.len() == 3 {
        curve.push(Json::from(0));
        curve.push(curve[2].clone());
    }
}

fn convert_property_function(
    parameters: &Json,
    spec: &PropertySpec,
    hints: FunctionHints,
    stops: &[(Json, Json)],
) -> Result<Json, Error> {
    let kind = function_type(parameters, hints);
    let get = serde_json::json!([
        "get",
        parameters.get("property").cloned().unwrap_or(Json::Null)
    ]);
    let default_wrap = |expression: Json| match parameters.get("default") {
        None => expression,
        Some(default) => serde_json::json!([
            "case",
            ["==", ["typeof", get], "number"],
            expression,
            convert_literal(default)
        ]),
    };
    Ok(match kind {
        "categorical" if stops.first().is_some_and(|(input, _)| input.is_boolean()) => {
            let mut expression = vec![Json::String("case".into())];
            for (input, output) in stops {
                expression.push(serde_json::json!(["==", get, input]));
                expression.push(output.clone());
            }
            expression.push(fallback(parameters, spec));
            Json::Array(expression)
        }
        "categorical" => {
            let mut expression = vec![Json::String("match".into()), get.clone()];
            for (input, output) in stops {
                append_stop(&mut expression, input, output, false);
            }
            expression.push(fallback(parameters, spec));
            Json::Array(expression)
        }
        "interval" => {
            let mut expression = vec![
                Json::String("step".into()),
                serde_json::json!(["number", get]),
            ];
            for (input, output) in stops {
                append_stop(&mut expression, input, output, true);
            }
            fixup_degenerate_step(&mut expression);
            default_wrap(Json::Array(expression))
        }
        "exponential" => {
            let base = parameters.get("base").and_then(Json::as_f64).unwrap_or(1.0);
            let mut expression = vec![
                Json::String(interpolate_operator(parameters).into()),
                if base == 1.0 {
                    serde_json::json!(["linear"])
                } else {
                    serde_json::json!(["exponential", base])
                },
                serde_json::json!(["number", get]),
            ];
            for (input, output) in stops {
                append_stop(&mut expression, input, output, false);
            }
            default_wrap(Json::Array(expression))
        }
        other => {
            return Err(Error::Invalid(format!(
                "Unknown property function type {other}"
            )));
        }
    })
}

fn convert_zoom_function(
    parameters: &Json,
    _spec: &PropertySpec,
    hints: FunctionHints,
    stops: &[(Json, Json)],
) -> Result<Json, Error> {
    let kind = function_type(parameters, hints);
    let (mut expression, is_step) = match kind {
        "interval" => (
            vec![Json::String("step".into()), serde_json::json!(["zoom"])],
            true,
        ),
        "exponential" => {
            let base = parameters.get("base").and_then(Json::as_f64).unwrap_or(1.0);
            (
                vec![
                    Json::String(interpolate_operator(parameters).into()),
                    if base == 1.0 {
                        serde_json::json!(["linear"])
                    } else {
                        serde_json::json!(["exponential", base])
                    },
                    serde_json::json!(["zoom"]),
                ],
                false,
            )
        }
        other => {
            return Err(Error::Invalid(format!(
                "Unknown zoom function type \"{other}\""
            )));
        }
    };
    for (input, output) in stops {
        append_stop(&mut expression, input, output, is_step);
    }
    fixup_degenerate_step(&mut expression);
    Ok(Json::Array(expression))
}

fn convert_zoom_and_property_function(
    parameters: &Json,
    spec: &PropertySpec,
    hints: FunctionHints,
    stops: &[(Json, Json)],
) -> Result<Json, Error> {
    // Group the stops by zoom, in first-seen order; each zoom gets its own property function.
    let mut zooms: Vec<(Json, Vec<(Json, Json)>)> = Vec::new();
    for (input, output) in stops {
        let zoom = input.get("zoom").cloned().unwrap_or(Json::Null);
        let value = input.get("value").cloned().unwrap_or(Json::Null);
        match zooms.iter_mut().find(|(z, _)| *z == zoom) {
            Some((_, group)) => group.push((value, output.clone())),
            None => zooms.push((zoom, vec![(value, output.clone())])),
        }
    }
    let mut inner = serde_json::Map::new();
    for key in ["type", "property", "default"] {
        if let Some(v) = parameters.get(key) {
            inner.insert(key.into(), v.clone());
        }
    }
    let inner = Json::Object(inner);
    // The zoom dimension interpolates linearly for interpolated properties and steps otherwise.
    if hints.interpolated {
        let mut expression = vec![
            Json::String(interpolate_operator(parameters).into()),
            serde_json::json!(["linear"]),
            serde_json::json!(["zoom"]),
        ];
        for (zoom, group) in &zooms {
            let output = convert_property_function(&inner, spec, hints, group)?;
            append_stop(&mut expression, zoom, &output, false);
        }
        Ok(Json::Array(expression))
    } else {
        let mut expression = vec![Json::String("step".into()), serde_json::json!(["zoom"])];
        for (zoom, group) in &zooms {
            let output = convert_property_function(&inner, spec, hints, group)?;
            append_stop(&mut expression, zoom, &output, true);
        }
        fixup_degenerate_step(&mut expression);
        Ok(Json::Array(expression))
    }
}

/// `"String with {name} token"` becomes `["concat", "String with ", ["get", "name"], " token"]`.
pub fn convert_token_string(text: &str) -> Json {
    let mut result = vec![Json::String("concat".into())];
    // `pos` is where the next literal piece starts; `scan` is where the next `{` is looked for.
    let mut pos = 0;
    let mut scan = 0;
    while let Some(open) = text[scan..].find('{').map(|i| i + scan) {
        let after = &text[open + 1..];
        match after.find(['{', '}']) {
            Some(close) if close > 0 && after.as_bytes()[close] == b'}' => {
                if open > pos {
                    result.push(Json::String(text[pos..open].into()));
                }
                result.push(serde_json::json!(["get", &after[..close]]));
                pos = open + 1 + close + 1;
                scan = pos;
            }
            Some(_) => scan = open + 1,
            None => break,
        }
    }
    if result.len() == 1 {
        return Json::String(text.into());
    }
    if pos < text.len() {
        result.push(Json::String(text[pos..].into()));
    } else if result.len() == 2 {
        return serde_json::json!(["to-string", result[1]]);
    }
    Json::Array(result)
}

// ---------------------------------------------------------------------------------------------
// CSS colors
// ---------------------------------------------------------------------------------------------

/// Parse a CSS color as the style spec accepts them: `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`,
/// `rgb()`/`rgba()` and `hsl()`/`hsla()` in their comma and space forms, the named colors and
/// `transparent`. Components come back straight (not premultiplied), in `0.0..=1.0`.
pub fn parse_color(text: &str) -> Option<Color> {
    let input = text.trim().to_lowercase();
    if input == "transparent" {
        return Some(Color::CLEAR);
    }
    if let Some((r, g, b)) = named_color(&input) {
        return Some(Color::rgb(
            f64::from(r) / 255.0,
            f64::from(g) / 255.0,
            f64::from(b) / 255.0,
        ));
    }
    if let Some(hex) = input.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    if let Some(inner) = input
        .strip_prefix("rgba(")
        .or_else(|| input.strip_prefix("rgb("))
        .and_then(|s| s.strip_suffix(')'))
    {
        return parse_rgb_function(inner);
    }
    if let Some(inner) = input
        .strip_prefix("hsla(")
        .or_else(|| input.strip_prefix("hsl("))
        .and_then(|s| s.strip_suffix(')'))
    {
        return parse_hsl_function(inner);
    }
    None
}

fn parse_hex_color(hex: &str) -> Option<Color> {
    if !matches!(hex.len(), 3 | 4 | 6 | 8) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let step = if hex.len() < 6 { 1 } else { 2 };
    let channel = |i: usize| -> f64 {
        let Some(digits) = hex.get(i * step..(i + 1) * step) else {
            return 1.0;
        };
        let value = if step == 1 {
            u32::from_str_radix(digits, 16).map(|v| v * 17)
        } else {
            u32::from_str_radix(digits, 16)
        };
        f64::from(value.unwrap_or(255)) / 255.0
    };
    Some(Color::rgba(channel(0), channel(1), channel(2), channel(3)))
}

/// One numeric argument of a color function: the number and whether it carried a `%`.
struct ColorArg {
    value: f64,
    percent: bool,
}

/// Split `r g b / a` or `r, g, b, a` into its arguments, requiring one consistent separator
/// style; `None` where the syntax is mixed.
fn color_function_args(inner: &str) -> Option<Vec<ColorArg>> {
    let inner = inner.trim();
    let (parts, comma_form): (Vec<&str>, bool) = if inner.contains(',') {
        if inner.contains('/') {
            return None;
        }
        (inner.split(',').map(str::trim).collect(), true)
    } else {
        let (colors, alpha) = match inner.split_once('/') {
            Some((c, a)) => (c, Some(a.trim())),
            None => (inner, None),
        };
        let mut parts: Vec<&str> = colors.split_whitespace().collect();
        if parts.len() != 3 {
            return None;
        }
        parts.extend(alpha);
        (parts, false)
    };
    if !(parts.len() == 3 || parts.len() == 4) || (comma_form && parts.iter().any(|p| p.is_empty()))
    {
        return None;
    }
    parts
        .iter()
        .map(|part| {
            let (number, percent) = match part.strip_suffix('%') {
                Some(n) => (n, true),
                None => (*part, false),
            };
            if number.is_empty()
                || !number
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'+' | b'-'))
            {
                return None;
            }
            let value = number.parse::<f64>().ok()?;
            Some(ColorArg { value, percent })
        })
        .collect()
}

fn parse_alpha(arg: &ColorArg) -> f64 {
    let a = if arg.percent {
        arg.value / 100.0
    } else {
        arg.value
    };
    a.clamp(0.0, 1.0)
}

fn parse_rgb_function(inner: &str) -> Option<Color> {
    let args = color_function_args(inner)?;
    let percents = args[..3].iter().filter(|a| a.percent).count();
    let max = match percents {
        3 => 100.0,
        0 => 255.0,
        _ => return None,
    };
    let channel = |i: usize| (args[i].value / max).clamp(0.0, 1.0);
    Some(Color::rgba(
        channel(0),
        channel(1),
        channel(2),
        args.get(3).map_or(1.0, parse_alpha),
    ))
}

fn parse_hsl_function(inner: &str) -> Option<Color> {
    // The hue may carry a `deg` suffix; strip it before the generic argument split.
    let normalized: String = {
        let trimmed = inner.trim_start();
        let hue_end = trimmed
            .find(|c: char| c == ',' || c.is_whitespace())
            .unwrap_or(trimmed.len());
        let (hue, rest) = trimmed.split_at(hue_end);
        format!("{}{rest}", hue.strip_suffix("deg").unwrap_or(hue))
    };
    let args = color_function_args(&normalized)?;
    if args[0].percent || !args[1].percent || !args[2].percent {
        return None;
    }
    let h = constrain_angle(args[0].value);
    let s = args[1].value.clamp(0.0, 100.0) / 100.0;
    let l = args[2].value.clamp(0.0, 100.0) / 100.0;
    // https://drafts.csswg.org/css-color-4/#hsl-to-rgb
    let f = |n: f64| {
        let k = (n + h / 30.0) % 12.0;
        let a = s * l.min(1.0 - l);
        l - a * (k - 3.0).min(9.0 - k).clamp(-1.0, 1.0)
    };
    Some(Color::rgba(
        f(0.0),
        f(8.0),
        f(4.0),
        args.get(3).map_or(1.0, parse_alpha),
    ))
}

/// The CSS named colors (<https://www.w3.org/TR/css-color-4/#named-colors>).
fn named_color(name: &str) -> Option<(u8, u8, u8)> {
    Some(match name {
        "aliceblue" => (240, 248, 255),
        "antiquewhite" => (250, 235, 215),
        "aqua" => (0, 255, 255),
        "aquamarine" => (127, 255, 212),
        "azure" => (240, 255, 255),
        "beige" => (245, 245, 220),
        "bisque" => (255, 228, 196),
        "black" => (0, 0, 0),
        "blanchedalmond" => (255, 235, 205),
        "blue" => (0, 0, 255),
        "blueviolet" => (138, 43, 226),
        "brown" => (165, 42, 42),
        "burlywood" => (222, 184, 135),
        "cadetblue" => (95, 158, 160),
        "chartreuse" => (127, 255, 0),
        "chocolate" => (210, 105, 30),
        "coral" => (255, 127, 80),
        "cornflowerblue" => (100, 149, 237),
        "cornsilk" => (255, 248, 220),
        "crimson" => (220, 20, 60),
        "cyan" => (0, 255, 255),
        "darkblue" => (0, 0, 139),
        "darkcyan" => (0, 139, 139),
        "darkgoldenrod" => (184, 134, 11),
        "darkgray" => (169, 169, 169),
        "darkgreen" => (0, 100, 0),
        "darkgrey" => (169, 169, 169),
        "darkkhaki" => (189, 183, 107),
        "darkmagenta" => (139, 0, 139),
        "darkolivegreen" => (85, 107, 47),
        "darkorange" => (255, 140, 0),
        "darkorchid" => (153, 50, 204),
        "darkred" => (139, 0, 0),
        "darksalmon" => (233, 150, 122),
        "darkseagreen" => (143, 188, 143),
        "darkslateblue" => (72, 61, 139),
        "darkslategray" => (47, 79, 79),
        "darkslategrey" => (47, 79, 79),
        "darkturquoise" => (0, 206, 209),
        "darkviolet" => (148, 0, 211),
        "deeppink" => (255, 20, 147),
        "deepskyblue" => (0, 191, 255),
        "dimgray" => (105, 105, 105),
        "dimgrey" => (105, 105, 105),
        "dodgerblue" => (30, 144, 255),
        "firebrick" => (178, 34, 34),
        "floralwhite" => (255, 250, 240),
        "forestgreen" => (34, 139, 34),
        "fuchsia" => (255, 0, 255),
        "gainsboro" => (220, 220, 220),
        "ghostwhite" => (248, 248, 255),
        "gold" => (255, 215, 0),
        "goldenrod" => (218, 165, 32),
        "gray" => (128, 128, 128),
        "green" => (0, 128, 0),
        "greenyellow" => (173, 255, 47),
        "grey" => (128, 128, 128),
        "honeydew" => (240, 255, 240),
        "hotpink" => (255, 105, 180),
        "indianred" => (205, 92, 92),
        "indigo" => (75, 0, 130),
        "ivory" => (255, 255, 240),
        "khaki" => (240, 230, 140),
        "lavender" => (230, 230, 250),
        "lavenderblush" => (255, 240, 245),
        "lawngreen" => (124, 252, 0),
        "lemonchiffon" => (255, 250, 205),
        "lightblue" => (173, 216, 230),
        "lightcoral" => (240, 128, 128),
        "lightcyan" => (224, 255, 255),
        "lightgoldenrodyellow" => (250, 250, 210),
        "lightgray" => (211, 211, 211),
        "lightgreen" => (144, 238, 144),
        "lightgrey" => (211, 211, 211),
        "lightpink" => (255, 182, 193),
        "lightsalmon" => (255, 160, 122),
        "lightseagreen" => (32, 178, 170),
        "lightskyblue" => (135, 206, 250),
        "lightslategray" => (119, 136, 153),
        "lightslategrey" => (119, 136, 153),
        "lightsteelblue" => (176, 196, 222),
        "lightyellow" => (255, 255, 224),
        "lime" => (0, 255, 0),
        "limegreen" => (50, 205, 50),
        "linen" => (250, 240, 230),
        "magenta" => (255, 0, 255),
        "maroon" => (128, 0, 0),
        "mediumaquamarine" => (102, 205, 170),
        "mediumblue" => (0, 0, 205),
        "mediumorchid" => (186, 85, 211),
        "mediumpurple" => (147, 112, 219),
        "mediumseagreen" => (60, 179, 113),
        "mediumslateblue" => (123, 104, 238),
        "mediumspringgreen" => (0, 250, 154),
        "mediumturquoise" => (72, 209, 204),
        "mediumvioletred" => (199, 21, 133),
        "midnightblue" => (25, 25, 112),
        "mintcream" => (245, 255, 250),
        "mistyrose" => (255, 228, 225),
        "moccasin" => (255, 228, 181),
        "navajowhite" => (255, 222, 173),
        "navy" => (0, 0, 128),
        "oldlace" => (253, 245, 230),
        "olive" => (128, 128, 0),
        "olivedrab" => (107, 142, 35),
        "orange" => (255, 165, 0),
        "orangered" => (255, 69, 0),
        "orchid" => (218, 112, 214),
        "palegoldenrod" => (238, 232, 170),
        "palegreen" => (152, 251, 152),
        "paleturquoise" => (175, 238, 238),
        "palevioletred" => (219, 112, 147),
        "papayawhip" => (255, 239, 213),
        "peachpuff" => (255, 218, 185),
        "peru" => (205, 133, 63),
        "pink" => (255, 192, 203),
        "plum" => (221, 160, 221),
        "powderblue" => (176, 224, 230),
        "purple" => (128, 0, 128),
        "rebeccapurple" => (102, 51, 153),
        "red" => (255, 0, 0),
        "rosybrown" => (188, 143, 143),
        "royalblue" => (65, 105, 225),
        "saddlebrown" => (139, 69, 19),
        "salmon" => (250, 128, 114),
        "sandybrown" => (244, 164, 96),
        "seagreen" => (46, 139, 87),
        "seashell" => (255, 245, 238),
        "sienna" => (160, 82, 45),
        "silver" => (192, 192, 192),
        "skyblue" => (135, 206, 235),
        "slateblue" => (106, 90, 205),
        "slategray" => (112, 128, 144),
        "slategrey" => (112, 128, 144),
        "snow" => (255, 250, 250),
        "springgreen" => (0, 255, 127),
        "steelblue" => (70, 130, 180),
        "tan" => (210, 180, 140),
        "teal" => (0, 128, 128),
        "thistle" => (216, 191, 216),
        "tomato" => (255, 99, 71),
        "turquoise" => (64, 224, 208),
        "violet" => (238, 130, 238),
        "wheat" => (245, 222, 179),
        "white" => (255, 255, 255),
        "whitesmoke" => (245, 245, 245),
        "yellow" => (255, 255, 0),
        "yellowgreen" => (154, 205, 50),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GeometryType;
    use serde_json::json;

    fn rgba(c: Option<Color>) -> Option<[f64; 4]> {
        c.map(|c| [c.r, c.g, c.b, c.a])
    }

    fn close(a: Option<[f64; 4]>, b: [f64; 4]) -> bool {
        a.is_some_and(|a| a.iter().zip(b.iter()).all(|(x, y)| (x - y).abs() < 1e-9))
    }

    #[test]
    fn parses_hex_colors() {
        assert!(close(rgba(parse_color("#f0c")), [1.0, 0.0, 0.8, 1.0]));
        assert!(close(
            rgba(parse_color("#F0C8")),
            [1.0, 0.0, 0.8, 136.0 / 255.0]
        ));
        assert!(close(rgba(parse_color("#ff00cc")), [1.0, 0.0, 0.8, 1.0]));
        assert!(close(
            rgba(parse_color("#ff00cc80")),
            [1.0, 0.0, 0.8, 128.0 / 255.0]
        ));
        assert_eq!(parse_color("#ff00c"), None);
        assert_eq!(parse_color("#gg0000"), None);
    }

    #[test]
    fn parses_named_colors() {
        assert!(close(rgba(parse_color("red")), [1.0, 0.0, 0.0, 1.0]));
        assert!(close(
            rgba(parse_color(" SteelBlue ")),
            [70.0 / 255.0, 130.0 / 255.0, 180.0 / 255.0, 1.0]
        ));
        assert!(close(
            rgba(parse_color("transparent")),
            [0.0, 0.0, 0.0, 0.0]
        ));
        assert_eq!(parse_color("__proto__"), None);
        assert_eq!(parse_color("oops blue"), None);
    }

    #[test]
    fn parses_rgb_functions() {
        assert!(close(
            rgba(parse_color("rgb(128 0 0)")),
            [128.0 / 255.0, 0.0, 0.0, 1.0]
        ));
        assert!(close(
            rgba(parse_color("rgb(50% 0% 0%)")),
            [0.5, 0.0, 0.0, 1.0]
        ));
        assert!(close(
            rgba(parse_color("rgba(255,0,255,0.6)")),
            [1.0, 0.0, 1.0, 0.6]
        ));
        assert!(close(
            rgba(parse_color("rgb(255 0 255 / 60%)")),
            [1.0, 0.0, 1.0, 0.6]
        ));
        assert!(close(
            rgba(parse_color("rgb(100% 0% 100% /.6)")),
            [1.0, 0.0, 1.0, 0.6]
        ));
        assert!(close(
            rgba(parse_color("rgba(0, 255, 0, 1)")),
            [0.0, 1.0, 0.0, 1.0]
        ));
        assert!(close(
            rgba(parse_color("rgb(300, -5, 0)")),
            [1.0, 0.0, 0.0, 1.0]
        ));
        // Mixed percentages and numbers, mixed separators, and bad numbers are rejected.
        assert_eq!(parse_color("rgb(50% 0 0)"), None);
        assert_eq!(parse_color("rgb(1, 2 3)"), None);
        assert_eq!(parse_color("rgb(1, 2, 3 / 0.5)"), None);
        assert_eq!(parse_color("rgb(1, 2, e)"), None);
        assert_eq!(parse_color("rgb(1, 2)"), None);
    }

    #[test]
    fn parses_hsl_functions() {
        assert!(close(
            rgba(parse_color("hsl(120 100% 50%)")),
            [0.0, 1.0, 0.0, 1.0]
        ));
        assert!(close(
            rgba(parse_color("hsla(120deg,100%,50%,.5)")),
            [0.0, 1.0, 0.0, 0.5]
        ));
        assert!(close(
            rgba(parse_color("hsl(240 100% 50% / 0)")),
            [0.0, 0.0, 1.0, 0.0]
        ));
        assert!(close(
            rgba(parse_color("hsl(12e1 100% 50%)")),
            [0.0, 1.0, 0.0, 1.0]
        ));
        assert!(close(
            rgba(parse_color("hsl(0, 0%, 83%)")),
            [0.83, 0.83, 0.83, 1.0]
        ));
        assert_eq!(parse_color("hsl(120 100 50)"), None);
        assert_eq!(parse_color("hsl(120% 100% 50%)"), None);
    }

    #[test]
    fn prints_colors_like_the_reference() {
        let c = parse_color("rgba(26, 207, 26, .73)").unwrap_or(Color::BLACK);
        assert_eq!(color_to_string(c), "rgba(26,207,26,0.73)");
        assert_eq!(js_number(6.02e-23), "6.02e-23");
        assert_eq!(js_number(1e21), "1e+21");
        assert_eq!(js_number(-3.0), "-3");
        assert_eq!(js_number(0.5), "0.5");
    }

    #[test]
    fn formats_numbers() {
        assert_eq!(
            format_number(123456.789, None, None, None, None),
            "123,456.789"
        );
        assert_eq!(
            format_number(-1234.5, None, None, Some(2.0), None),
            "-1,234.50"
        );
        assert_eq!(format_number(0.125, None, None, None, Some(2.0)), "0.13");
        assert_eq!(
            format_number(999.9995, None, None, None, Some(3.0)),
            "1,000"
        );
        assert_eq!(
            format_number(123456.789, Some("JPY"), None, None, None),
            "¥123,457"
        );
        assert_eq!(
            format_number(1234.567, None, Some("celsius"), None, None),
            "1,234.567°C"
        );
    }

    #[test]
    fn collates() {
        assert_eq!(collate("a", "A", false, false), Ordering::Equal);
        assert_eq!(collate("a", "A", true, false), Ordering::Less);
        assert_eq!(collate("a", "ä", true, false), Ordering::Equal);
        assert_eq!(collate("a", "ä", false, true), Ordering::Less);
        assert_eq!(collate("ä", "b", true, true), Ordering::Less);
        assert_eq!(collate("tēnā", "tena", false, false), Ordering::Equal);
        assert_eq!(collate("tēnā", "tena", false, true), Ordering::Greater);
    }

    // --- filters, after maplibre-style-spec/src/feature_filter/feature_filter.test.ts ---

    fn feature(properties: Json) -> Feature {
        Feature {
            id: None,
            kind: GeometryType::Unknown,
            properties: properties
                .as_object()
                .map(|m| {
                    m.iter()
                        .map(|(k, v)| (k.clone(), Value::from_json(v)))
                        .collect()
                })
                .unwrap_or_default(),
            geometry: Vec::new(),
        }
    }

    fn typed(kind: GeometryType) -> Feature {
        Feature {
            kind,
            ..feature(json!({}))
        }
    }

    fn with_id(id: Json) -> Feature {
        Feature {
            id: Some(Value::from_json(&id)),
            ..feature(json!({}))
        }
    }

    /// Compile a filter and evaluate it against a feature at a zoom.
    fn filter(json: Json) -> impl Fn(f64, &Feature) -> bool {
        let compiled = match Expression::parse_filter(&json) {
            Ok(e) => e,
            Err(err) => panic!("filter {json} did not parse: {err}"),
        };
        move |zoom, feature| {
            let ctx = Context {
                zoom,
                feature: Some(feature),
                ..Context::default()
            };
            compiled.evaluate(&ctx).ok().and_then(|v| v.as_bool()) == Some(true)
        }
    }

    #[test]
    fn expression_zoom() {
        let f = filter(json!([">=", ["number", ["get", "x"]], ["zoom"]]));
        assert!(!f(1.0, &feature(json!({"x": 0}))));
        assert!(f(1.0, &feature(json!({"x": 1.5}))));
        assert!(f(1.0, &feature(json!({"x": 2.5}))));
        assert!(!f(2.0, &feature(json!({"x": 0}))));
        assert!(!f(2.0, &feature(json!({"x": 1.5}))));
        assert!(f(2.0, &feature(json!({"x": 2.5}))));
    }

    #[test]
    fn expression_compare_two_properties() {
        let f = filter(json!([
            "==",
            ["string", ["get", "x"]],
            ["string", ["get", "y"]]
        ]));
        assert!(!f(0.0, &feature(json!({"x": 1, "y": 1}))));
        assert!(f(0.0, &feature(json!({"x": "1", "y": "1"}))));
        assert!(f(0.0, &feature(json!({"x": "same", "y": "same"}))));
        assert!(!f(0.0, &feature(json!({"x": null}))));
        assert!(!f(0.0, &feature(json!({}))));
    }

    #[test]
    fn expression_collator_comparison() {
        let sensitive = filter(json!([
            "==",
            ["string", ["get", "x"]],
            ["string", ["get", "y"]],
            ["collator", {"case-sensitive": true}]
        ]));
        assert!(!sensitive(0.0, &feature(json!({"x": "a", "y": "b"}))));
        assert!(!sensitive(0.0, &feature(json!({"x": "a", "y": "A"}))));
        assert!(sensitive(0.0, &feature(json!({"x": "a", "y": "a"}))));
        let insensitive = filter(json!([
            "==",
            ["string", ["get", "x"]],
            ["string", ["get", "y"]],
            ["collator", {"case-sensitive": false}]
        ]));
        assert!(!insensitive(0.0, &feature(json!({"x": "a", "y": "b"}))));
        assert!(insensitive(0.0, &feature(json!({"x": "a", "y": "A"}))));
        assert!(insensitive(0.0, &feature(json!({"x": "a", "y": "a"}))));
    }

    #[test]
    fn expression_any_all_literal_match() {
        let none = feature(json!({}));
        assert!(filter(json!(["all"]))(0.0, &none));
        assert!(filter(json!(["all", true]))(0.0, &none));
        assert!(!filter(json!(["all", true, false]))(0.0, &none));
        assert!(filter(json!(["all", true, true]))(0.0, &none));
        assert!(!filter(json!(["any"]))(0.0, &none));
        assert!(filter(json!(["any", true]))(0.0, &none));
        assert!(filter(json!(["any", true, false]))(0.0, &none));
        assert!(!filter(json!(["any", false, false]))(0.0, &none));
        assert!(filter(json!(["literal", true]))(0.0, &none));
        assert!(!filter(json!(["literal", false]))(0.0, &none));
        let m = filter(json!(["match", ["get", "x"], ["a", "b", "c"], true, false]));
        assert!(m(0.0, &feature(json!({"x": "a"}))));
        assert!(m(0.0, &feature(json!({"x": "c"}))));
        assert!(!m(0.0, &feature(json!({"x": "d"}))));
    }

    #[test]
    fn expression_type_errors() {
        let err = Expression::parse_filter(&json!([
            "==",
            ["number", ["get", "x"]],
            ["string", ["get", "y"]]
        ]))
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
        assert!(
            err.contains("Cannot compare types 'number' and 'string'."),
            "{err}"
        );
        let err = Expression::parse_filter(&json!(["number", ["get", "x"]]))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(
            err.contains("Expected boolean but found number instead."),
            "{err}"
        );
        assert!(Expression::parse_filter(&json!(["boolean", ["get", "x"]])).is_ok());
    }

    #[test]
    fn expression_global_state() {
        let compiled =
            Expression::parse_filter(&json!(["==", ["global-state", "x"], ["get", "x"]]));
        let compiled = compiled.unwrap_or_else(|e| panic!("{e}"));
        let state: BTreeMap<String, Value> = [("x".to_string(), Value::Number(1.0))].into();
        let run = |props: Json| {
            let f = feature(props);
            let ctx = Context {
                feature: Some(&f),
                global_state: Some(&state),
                ..Context::default()
            };
            compiled.evaluate(&ctx).ok() == Some(Value::Bool(true))
        };
        assert!(run(json!({"x": 1})));
        assert!(!run(json!({"x": 2})));
    }

    #[test]
    fn legacy_filter_detection() {
        assert!(!is_expression_filter(&json!([
            "in", "color", "red", "blue"
        ])));
        assert!(!is_expression_filter(&json!(["in", "value", 42])));
        assert!(!is_expression_filter(&json!(["in", "value", true])));
        assert!(!is_expression_filter(&json!(["in", "color", "red"])));
        assert!(is_expression_filter(&json!([
            "in",
            ["get", "color"],
            "reddish"
        ])));
        assert!(is_expression_filter(&json!([
            "in",
            ["get", "color"],
            ["red", "blue"]
        ])));
        assert!(is_expression_filter(&json!(["in", 42, 42])));
        assert!(is_expression_filter(&json!(["in", true, true])));
        assert!(is_expression_filter(&json!([
            "in",
            "red",
            ["get", "colors"]
        ])));
        // `$type` beside expression operators is an expression filter, `none` never is.
        assert!(is_expression_filter(&json!([
            "all",
            ["==", "$type", "Point"],
            [
                "case",
                ["==", ["get", "id"], ["global-state", "a"]],
                true,
                false
            ]
        ])));
        assert!(!is_expression_filter(&json!([
            "none",
            ["==", "$type", "Polygon"]
        ])));
    }

    #[test]
    fn converts_legacy_filters() {
        let converted = convert_filter(
            &json!([
                "all",
                ["in", "$type", "Polygon", "LineString", "Point"],
                ["all", ["in", "type", "island"]]
            ]),
            &mut Vec::new(),
        );
        assert_eq!(
            converted,
            json!([
                "all",
                [
                    "match",
                    ["geometry-type"],
                    ["LineString", "Point", "Polygon"],
                    true,
                    false
                ],
                ["match", ["get", "type"], ["island"], true, false]
            ])
        );
        let converted = convert_filter(&json!(["in", "$id", 1, 2, 3, 2, 1]), &mut Vec::new());
        assert_eq!(converted, json!(["match", ["id"], [1, 2, 3], true, false]));
    }

    #[test]
    fn mimics_legacy_type_mismatch_semantics() {
        let f = filter(json!([
            "any",
            ["all", [">", "y", 0], [">", "y", 0]],
            [">", "x", 0]
        ]));
        assert!(f(0.0, &feature(json!({"x": 0, "y": 1, "z": 1}))));
        assert!(f(0.0, &feature(json!({"x": 1, "y": 0, "z": 1}))));
        assert!(!f(0.0, &feature(json!({"x": 0, "y": 0, "z": 1}))));
        assert!(f(0.0, &feature(json!({"x": null, "y": 1, "z": 1}))));
        assert!(f(0.0, &feature(json!({"x": 1, "y": null, "z": 1}))));
        assert!(!f(0.0, &feature(json!({"x": null, "y": null, "z": 1}))));
    }

    #[test]
    fn legacy_degenerate() {
        let none = feature(json!({}));
        assert!(filter(Json::Null)(0.0, &none));
    }

    #[test]
    fn legacy_equality() {
        let f = filter(json!(["==", "foo", "bar"]));
        assert!(f(0.0, &feature(json!({"foo": "bar"}))));
        assert!(!f(0.0, &feature(json!({"foo": "baz"}))));

        let f = filter(json!(["==", "foo", 0]));
        assert!(f(0.0, &feature(json!({"foo": 0}))));
        assert!(!f(0.0, &feature(json!({"foo": 1}))));
        assert!(!f(0.0, &feature(json!({"foo": "0"}))));
        assert!(!f(0.0, &feature(json!({"foo": true}))));
        assert!(!f(0.0, &feature(json!({"foo": false}))));
        assert!(!f(0.0, &feature(json!({"foo": null}))));
        assert!(!f(0.0, &feature(json!({}))));

        let f = filter(json!(["==", "foo", null]));
        assert!(!f(0.0, &feature(json!({"foo": 0}))));
        assert!(!f(0.0, &feature(json!({"foo": 1}))));
        assert!(!f(0.0, &feature(json!({"foo": "0"}))));
        assert!(!f(0.0, &feature(json!({"foo": true}))));
        assert!(!f(0.0, &feature(json!({"foo": false}))));
        assert!(f(0.0, &feature(json!({"foo": null}))));
        assert!(!f(0.0, &feature(json!({}))));

        let f = filter(json!(["==", "$type", "LineString"]));
        assert!(!f(0.0, &typed(GeometryType::Point)));
        assert!(f(0.0, &typed(GeometryType::LineString)));

        let f = filter(json!(["==", "$id", 1234]));
        assert!(f(0.0, &with_id(json!(1234))));
        assert!(!f(0.0, &with_id(json!("1234"))));
        assert!(!f(0.0, &feature(json!({"id": 1234}))));
    }

    #[test]
    fn legacy_inequality() {
        let f = filter(json!(["!=", "foo", "bar"]));
        assert!(!f(0.0, &feature(json!({"foo": "bar"}))));
        assert!(f(0.0, &feature(json!({"foo": "baz"}))));

        let f = filter(json!(["!=", "foo", 0]));
        assert!(!f(0.0, &feature(json!({"foo": 0}))));
        assert!(f(0.0, &feature(json!({"foo": 1}))));
        assert!(f(0.0, &feature(json!({"foo": "0"}))));
        assert!(f(0.0, &feature(json!({"foo": true}))));
        assert!(f(0.0, &feature(json!({"foo": false}))));
        assert!(f(0.0, &feature(json!({"foo": null}))));
        assert!(f(0.0, &feature(json!({}))));

        let f = filter(json!(["!=", "foo", null]));
        assert!(f(0.0, &feature(json!({"foo": 0}))));
        assert!(f(0.0, &feature(json!({"foo": 1}))));
        assert!(f(0.0, &feature(json!({"foo": "0"}))));
        assert!(f(0.0, &feature(json!({"foo": true}))));
        assert!(f(0.0, &feature(json!({"foo": false}))));
        assert!(!f(0.0, &feature(json!({"foo": null}))));
        assert!(f(0.0, &feature(json!({}))));

        let f = filter(json!(["!=", "$type", "LineString"]));
        assert!(f(0.0, &typed(GeometryType::Point)));
        assert!(!f(0.0, &typed(GeometryType::LineString)));
    }

    /// The ordering operators against `0` and `"0"`, each over the reference's input table:
    /// `[1, 0, -1, "1", "0", "-1", true, false, null, missing]`.
    fn ordering_table(op: &str, value: Json, expected: [bool; 10]) {
        let f = filter(json!([op, "foo", value]));
        let inputs = [
            json!({"foo": 1}),
            json!({"foo": 0}),
            json!({"foo": -1}),
            json!({"foo": "1"}),
            json!({"foo": "0"}),
            json!({"foo": "-1"}),
            json!({"foo": true}),
            json!({"foo": false}),
            json!({"foo": null}),
            json!({}),
        ];
        for (input, want) in inputs.iter().zip(expected) {
            assert_eq!(
                f(0.0, &feature(input.clone())),
                want,
                "{op} {value} on {input}"
            );
        }
    }

    #[test]
    fn legacy_ordering() {
        ordering_table(
            "<",
            json!(0),
            [
                false, false, true, false, false, false, false, false, false, false,
            ],
        );
        ordering_table(
            "<",
            json!("0"),
            [
                false, false, false, false, false, true, false, false, false, false,
            ],
        );
        ordering_table(
            "<=",
            json!(0),
            [
                false, true, true, false, false, false, false, false, false, false,
            ],
        );
        ordering_table(
            "<=",
            json!("0"),
            [
                false, false, false, false, true, true, false, false, false, false,
            ],
        );
        ordering_table(
            ">",
            json!(0),
            [
                true, false, false, false, false, false, false, false, false, false,
            ],
        );
        ordering_table(
            ">",
            json!("0"),
            [
                false, false, false, true, false, false, false, false, false, false,
            ],
        );
        ordering_table(
            ">=",
            json!(0),
            [
                true, true, false, false, false, false, false, false, false, false,
            ],
        );
        ordering_table(
            ">=",
            json!("0"),
            [
                false, false, false, true, true, false, false, false, false, false,
            ],
        );
    }

    #[test]
    fn legacy_in() {
        let f = filter(json!(["in", "foo"]));
        assert!(!f(0.0, &feature(json!({"foo": 1}))));

        let f = filter(json!(["in", "foo", "0"]));
        assert!(!f(0.0, &feature(json!({"foo": 0}))));
        assert!(f(0.0, &feature(json!({"foo": "0"}))));
        assert!(!f(0.0, &feature(json!({"foo": true}))));
        assert!(!f(0.0, &feature(json!({"foo": false}))));
        assert!(!f(0.0, &feature(json!({"foo": null}))));
        assert!(!f(0.0, &feature(json!({}))));

        let f = filter(json!(["in", "foo", 0]));
        assert!(f(0.0, &feature(json!({"foo": 0}))));
        assert!(!f(0.0, &feature(json!({"foo": "0"}))));
        assert!(!f(0.0, &feature(json!({"foo": true}))));
        assert!(!f(0.0, &feature(json!({"foo": false}))));
        assert!(!f(0.0, &feature(json!({"foo": null}))));

        let f = filter(json!(["in", "foo", null]));
        assert!(!f(0.0, &feature(json!({"foo": 0}))));
        assert!(!f(0.0, &feature(json!({"foo": "0"}))));
        assert!(!f(0.0, &feature(json!({"foo": true}))));
        assert!(!f(0.0, &feature(json!({"foo": false}))));
        assert!(f(0.0, &feature(json!({"foo": null}))));

        let f = filter(json!(["in", "foo", 0, 1]));
        assert!(f(0.0, &feature(json!({"foo": 0}))));
        assert!(f(0.0, &feature(json!({"foo": 1}))));
        assert!(!f(0.0, &feature(json!({"foo": 3}))));

        let mut large = vec![json!("in"), json!("foo")];
        large.extend((0..2000).rev().map(|i| json!(i)));
        let f = filter(Json::Array(large.clone()));
        assert!(f(0.0, &feature(json!({"foo": 0}))));
        assert!(f(0.0, &feature(json!({"foo": 1}))));
        assert!(f(0.0, &feature(json!({"foo": 1999}))));
        assert!(!f(0.0, &feature(json!({"foo": 2000}))));

        large.insert(2, json!("b"));
        large.push(json!("a"));
        let f = filter(Json::Array(large));
        assert!(f(0.0, &feature(json!({"foo": "b"}))));
        assert!(f(0.0, &feature(json!({"foo": "a"}))));
        assert!(f(0.0, &feature(json!({"foo": 0}))));
        assert!(f(0.0, &feature(json!({"foo": 1999}))));
        assert!(!f(0.0, &feature(json!({"foo": 2000}))));

        let f = filter(json!(["in", "$type", "LineString", "Polygon"]));
        assert!(!f(0.0, &typed(GeometryType::Point)));
        assert!(f(0.0, &typed(GeometryType::LineString)));
        assert!(f(0.0, &typed(GeometryType::Polygon)));
        let f = filter(json!(["in", "$type", "Polygon", "LineString", "Point"]));
        assert!(f(0.0, &typed(GeometryType::Point)));
        assert!(f(0.0, &typed(GeometryType::LineString)));
        assert!(f(0.0, &typed(GeometryType::Polygon)));
    }

    #[test]
    fn legacy_not_in() {
        let f = filter(json!(["!in", "foo"]));
        assert!(f(0.0, &feature(json!({"foo": 1}))));

        let f = filter(json!(["!in", "foo", "0"]));
        assert!(f(0.0, &feature(json!({"foo": 0}))));
        assert!(!f(0.0, &feature(json!({"foo": "0"}))));
        assert!(f(0.0, &feature(json!({"foo": null}))));
        assert!(f(0.0, &feature(json!({}))));

        let f = filter(json!(["!in", "foo", 0]));
        assert!(!f(0.0, &feature(json!({"foo": 0}))));
        assert!(f(0.0, &feature(json!({"foo": "0"}))));
        assert!(f(0.0, &feature(json!({"foo": null}))));

        let f = filter(json!(["!in", "foo", null]));
        assert!(f(0.0, &feature(json!({"foo": 0}))));
        assert!(f(0.0, &feature(json!({"foo": "0"}))));
        assert!(!f(0.0, &feature(json!({"foo": null}))));

        let f = filter(json!(["!in", "foo", 0, 1]));
        assert!(!f(0.0, &feature(json!({"foo": 0}))));
        assert!(!f(0.0, &feature(json!({"foo": 1}))));
        assert!(f(0.0, &feature(json!({"foo": 3}))));

        let mut large = vec![json!("!in"), json!("foo")];
        large.extend((0..2000).map(|i| json!(i)));
        let f = filter(Json::Array(large));
        assert!(!f(0.0, &feature(json!({"foo": 0}))));
        assert!(!f(0.0, &feature(json!({"foo": 1}))));
        assert!(!f(0.0, &feature(json!({"foo": 1999}))));
        assert!(f(0.0, &feature(json!({"foo": 2000}))));

        let f = filter(json!(["!in", "$type", "LineString", "Polygon"]));
        assert!(f(0.0, &typed(GeometryType::Point)));
        assert!(!f(0.0, &typed(GeometryType::LineString)));
        assert!(!f(0.0, &typed(GeometryType::Polygon)));
    }

    #[test]
    fn legacy_any_all_none() {
        let one = feature(json!({"foo": 1}));
        assert!(!filter(json!(["any"]))(0.0, &one));
        assert!(filter(json!(["any", ["==", "foo", 1]]))(0.0, &one));
        assert!(!filter(json!(["any", ["==", "foo", 0]]))(0.0, &one));
        assert!(filter(json!(["any", ["==", "foo", 0], ["==", "foo", 1]]))(
            0.0, &one
        ));

        assert!(filter(json!(["all"]))(0.0, &one));
        assert!(filter(json!(["all", ["==", "foo", 1]]))(0.0, &one));
        assert!(!filter(json!(["all", ["==", "foo", 0]]))(0.0, &one));
        assert!(!filter(json!(["all", ["==", "foo", 0], ["==", "foo", 1]]))(
            0.0, &one
        ));

        assert!(filter(json!(["none"]))(0.0, &one));
        assert!(!filter(json!(["none", ["==", "foo", 1]]))(0.0, &one));
        assert!(filter(json!(["none", ["==", "foo", 0]]))(0.0, &one));
        assert!(!filter(json!(["none", ["==", "foo", 0], ["==", "foo", 1]]))(0.0, &one));
    }

    #[test]
    fn legacy_has() {
        let f = filter(json!(["has", "foo"]));
        assert!(f(0.0, &feature(json!({"foo": 0}))));
        assert!(f(0.0, &feature(json!({"foo": 1}))));
        assert!(f(0.0, &feature(json!({"foo": "0"}))));
        assert!(f(0.0, &feature(json!({"foo": true}))));
        assert!(f(0.0, &feature(json!({"foo": false}))));
        assert!(f(0.0, &feature(json!({"foo": null}))));
        assert!(!f(0.0, &feature(json!({}))));

        let f = filter(json!(["!has", "foo"]));
        assert!(!f(0.0, &feature(json!({"foo": 0}))));
        assert!(!f(0.0, &feature(json!({"foo": "0"}))));
        assert!(!f(0.0, &feature(json!({"foo": false}))));
        assert!(!f(0.0, &feature(json!({"foo": null}))));
        assert!(f(0.0, &feature(json!({}))));

        let f = filter(json!([
            "all",
            ["==", "$type", "LineString"],
            ["all", ["==", "class", "rail"], ["has", "service"]]
        ]));
        let rail = |props: Json| Feature {
            kind: GeometryType::LineString,
            ..feature(props)
        };
        assert!(f(0.0, &rail(json!({"class": "rail", "service": "yard"}))));
        assert!(!f(0.0, &rail(json!({"class": "rail"}))));
        assert!(!f(0.0, &rail(json!({"class": "road", "service": "yard"}))));
    }

    // --- legacy functions ---

    #[test]
    fn converts_token_strings() {
        assert_eq!(
            convert_token_string("String with {name} token"),
            json!(["concat", "String with ", ["get", "name"], " token"])
        );
        assert_eq!(
            convert_token_string("{name}"),
            json!(["to-string", ["get", "name"]])
        );
        assert_eq!(convert_token_string("plain"), json!("plain"));
        assert_eq!(
            convert_token_string("{a}{b}"),
            json!(["concat", ["get", "a"], ["get", "b"]])
        );
        assert_eq!(
            convert_token_string("{{a}"),
            json!(["concat", "{", ["get", "a"]])
        );
    }

    #[test]
    fn converts_zoom_functions() {
        let spec = PropertySpec {
            kind: Type::Number,
            default: None,
            data_driven: false,
        };
        let converted = convert_function(
            &json!({"stops": [[0, 1], [10, 2]], "base": 1.5}),
            &spec,
            spec.function_hints(),
        )
        .unwrap_or(Json::Null);
        assert_eq!(
            converted,
            json!(["interpolate", ["exponential", 1.5], ["zoom"], 0, 1, 10, 2])
        );
        let converted = convert_function(
            &json!({"stops": [[0, "a"]], "type": "interval"}),
            &spec,
            spec.function_hints(),
        )
        .unwrap_or(Json::Null);
        assert_eq!(converted, json!(["step", ["zoom"], "a", 0, "a"]));
    }

    #[test]
    fn folds_constants_and_tracks_dependence() {
        let e = Expression::parse(&json!(["+", 1, ["*", 2, 3]]), None)
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(e.as_constant(), Some(&Value::Number(7.0)));
        let e = Expression::parse(&json!(["get", "x"]), None).unwrap_or_else(|e| panic!("{e}"));
        assert!(!e.is_feature_constant());
        assert!(e.is_zoom_constant());
        let e = Expression::parse(&json!(["zoom"]), None).unwrap_or_else(|e| panic!("{e}"));
        assert!(e.is_feature_constant());
        assert!(!e.is_zoom_constant());
        assert!(matches!(
            Expression::parse(&json!(["within", {}]), None),
            Err(Error::Unsupported(_))
        ));
    }
}
