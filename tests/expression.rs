// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! Runs MapLibre's expression fixtures (`tests/data/expression`, see `tests/data/README.md`)
//! against `day_piece_map::expression`.
//!
//! Each `test.json` holds an `expression` (optionally with a `propertySpec`), the `expected`
//! compilation result (success with its type and constancy, or an error) and, for successes,
//! `inputs` (`[globals, feature]` pairs) with their `outputs`. The harness mirrors MapLibre's
//! `expression.test.ts`: colors are reported as premultiplied `[r, g, b, a]`, formatted text
//! and images as the objects the reference serializes, and numbers are compared at the six
//! significant digits the fixtures were written with.

use day_piece_map::expression::{
    Context, Expression, FunctionHints, PropertySpec, Type, is_expression,
};
use day_piece_map::{Feature, FormattedSection, GeometryType, Value};
use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Fixtures that are not run, by directory (a prefix of the fixture's path), with the reason.
const SKIPPED: &[(&str, &str)] = &[
    (
        "distance",
        "`distance` needs geodesic geometry over GeoJSON; parses as Unsupported",
    ),
    (
        "within",
        "`within` needs GeoJSON polygon containment of tile geometry; Unsupported",
    ),
    (
        "heatmap-density",
        "no heatmap density global in the evaluation context; Unsupported",
    ),
    (
        "elevation",
        "no elevation global in the evaluation context; Unsupported",
    ),
    (
        "is-supported-script",
        "needs the host's script support table; Unsupported",
    ),
    (
        "resolved-locale",
        "needs the host's ICU locale resolution; Unsupported",
    ),
    (
        "collator/accent-equals-de",
        "depends on `resolved-locale`, which is Unsupported",
    ),
    (
        "collator/variable-gt",
        "locale tailoring (Swedish sorts ä after z) that the hand-rolled collator does not model",
    ),
    (
        "collator/variable-lteq",
        "locale tailoring (Swedish sorts ä after z) that the hand-rolled collator does not model",
    ),
    (
        "global-state/projection",
        "the `projectionDefinition` property type is not a map style type this crate models",
    ),
    (
        "global-state/projection-invalid-value",
        "the `projectionDefinition` property type is not modelled",
    ),
    (
        "step/projection",
        "the `projectionDefinition` property type is not modelled",
    ),
    (
        "interpolate/projection",
        "the `projectionDefinition` property type is not modelled",
    ),
    (
        "interpolate-hcl/uninterpolable-output/projection",
        "the `projectionDefinition` property type is not modelled",
    ),
    (
        "interpolate-lab/uninterpolable-output/projection",
        "the `projectionDefinition` property type is not modelled",
    ),
    (
        "interpolate/linear-number-array",
        "the `numberArray` property type is not modelled",
    ),
    (
        "interpolate-hcl/uninterpolable-output/number-array",
        "the `numberArray` property type is not modelled",
    ),
    (
        "interpolate-lab/uninterpolable-output/number-array",
        "the `numberArray` property type is not modelled",
    ),
    (
        "legacy/identity/number_array",
        "the `numberArray` property type is not modelled",
    ),
    (
        "interpolate/linear-color-array",
        "the `colorArray` property type is not modelled",
    ),
    (
        "interpolate-hcl/linear-color-array",
        "the `colorArray` property type is not modelled",
    ),
    (
        "interpolate-lab/linear-color-array",
        "the `colorArray` property type is not modelled",
    ),
    (
        "legacy/identity/color_array",
        "the `colorArray` property type is not modelled",
    ),
    (
        "legacy/identity/padding",
        "the `padding` property type is not modelled",
    ),
    (
        "semiliteral/text-variable-anchor-offset",
        "the `variableAnchorOffsetCollection` property type is not modelled",
    ),
];

fn skip_reason(name: &str) -> Option<&'static str> {
    SKIPPED
        .iter()
        .find(|(prefix, _)| name == *prefix || name.starts_with(&format!("{prefix}/")))
        .map(|(_, reason)| *reason)
}

fn fixture_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            fixture_files(&path, out);
        } else if path.file_name().is_some_and(|n| n == "test.json") {
            out.push(path);
        }
    }
}

/// The `PropertySpec` (and legacy-function hints) of a fixture's `propertySpec`; `None` for a
/// property type this crate does not model.
fn property_spec(spec: Option<&Json>) -> Option<(PropertySpec, FunctionHints)> {
    let Some(spec) = spec else {
        return Some((
            PropertySpec::default_spec(),
            FunctionHints {
                interpolated: true,
                tokens: false,
            },
        ));
    };
    let kind = match spec.get("type").and_then(Json::as_str) {
        None => Type::Value,
        Some("number") => Type::Number,
        Some("string") => Type::String,
        Some("boolean") => Type::Boolean,
        Some("color") => Type::Color,
        Some("formatted") => Type::Formatted,
        Some("resolvedImage") => Type::ResolvedImage,
        Some("null") => Type::Null,
        Some("enum") => Type::Enum(
            spec.get("values")
                .and_then(Json::as_object)
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default(),
        ),
        Some("array") => {
            let item = match spec.get("value").and_then(Json::as_str) {
                Some("number") => Type::Number,
                Some("string") => Type::String,
                Some("boolean") => Type::Boolean,
                _ => Type::Value,
            };
            let length = spec
                .get("length")
                .and_then(Json::as_u64)
                .map(|n| n as usize);
            Type::array(item, length)
        }
        Some(_) => return None,
    };
    let data_driven = match spec.get("property-type").and_then(Json::as_str) {
        None => true,
        Some(kind) => kind == "data-driven" || kind == "cross-faded-data-driven",
    };
    // As in the reference harness: a missing `expression` block means interpolated.
    let interpolated = match spec.get("expression") {
        None => true,
        Some(e) => e
            .get("interpolated")
            .and_then(Json::as_bool)
            .unwrap_or(false),
    };
    let tokens = spec.get("tokens").and_then(Json::as_bool).unwrap_or(false);
    Some((
        PropertySpec {
            kind,
            default: spec.get("default").map(Value::from_json),
            data_driven,
        },
        FunctionHints {
            interpolated,
            tokens,
        },
    ))
}

trait DefaultSpec {
    fn default_spec() -> PropertySpec;
}

impl DefaultSpec for PropertySpec {
    fn default_spec() -> PropertySpec {
        PropertySpec {
            kind: Type::Value,
            default: None,
            data_driven: true,
        }
    }
}

fn geometry_kind(geometry: Option<&Json>) -> GeometryType {
    match geometry.and_then(|g| g.get("type")).and_then(Json::as_str) {
        Some("Point" | "MultiPoint") => GeometryType::Point,
        Some("LineString" | "MultiLineString") => GeometryType::LineString,
        Some("Polygon" | "MultiPolygon") => GeometryType::Polygon,
        _ => GeometryType::Unknown,
    }
}

fn value_map(json: Option<&Json>) -> BTreeMap<String, Value> {
    json.and_then(Json::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), Value::from_json(v)))
                .collect()
        })
        .unwrap_or_default()
}

fn json_number(n: f64) -> Json {
    serde_json::Number::from_f64(n).map_or(Json::Null, Json::Number)
}

/// A color as the reference serializes it: premultiplied components.
fn color_object(c: day_geometry::Color) -> Json {
    serde_json::json!({
        "r": json_number(c.r * c.a),
        "g": json_number(c.g * c.a),
        "b": json_number(c.b * c.a),
        "a": json_number(c.a),
    })
}

fn section_json(section: &FormattedSection, available_images: &[String]) -> Json {
    serde_json::json!({
        "text": section.text,
        // A section keeps the image's name; whether the sprite has it is what the `image`
        // expression answered from the same list, so it is looked up again here.
        "image": section.image.as_ref().map(|name| serde_json::json!({
            "name": name,
            "available": available_images.iter().any(|i| i == name),
        })),
        "scale": section.scale.map(json_number),
        "fontStack": section.font_stack.as_ref().map(|f| f.join(",")),
        "textColor": section.text_color.map(color_object),
        "verticalAlign": section.vertical_align,
    })
}

/// The JSON the reference harness would have written for `value`.
fn to_json(value: &Value, result_type: &Type, available_images: &[String]) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::Bool(*b),
        Value::Number(n) => json_number(*n),
        Value::String(s) => Json::String(s.clone()),
        Value::Color(c) if *result_type == Type::Color => Json::Array(vec![
            json_number(c.r * c.a),
            json_number(c.g * c.a),
            json_number(c.b * c.a),
            json_number(c.a),
        ]),
        Value::Color(c) => color_object(*c),
        Value::Array(items) => Json::Array(
            items
                .iter()
                .map(|i| to_json(i, &Type::Value, available_images))
                .collect(),
        ),
        Value::Object(map) => Json::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), to_json(v, &Type::Value, available_images)))
                .collect(),
        ),
        Value::Formatted(sections) => serde_json::json!({
            "sections": sections.iter().map(|s| section_json(s, available_images)).collect::<Vec<Json>>(),
        }),
        Value::Image { name, available } => {
            serde_json::json!({ "name": name, "available": available })
        }
        Value::Collator { .. } => Json::Object(Default::default()),
    }
}

/// The reference's `stripPrecision`: six significant digits, truncated, never past the
/// decimal point.
fn strip_precision(x: f64) -> f64 {
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    let multiplier = 10f64.powf((6.0 - x.abs().log10().ceil()).max(0.0));
    let first = (x * multiplier).floor() / multiplier;
    (first * multiplier).floor() / multiplier
}

/// Numbers agree when, truncated to the fixtures' six significant digits, they differ by at
/// most one unit in the last kept digit (the truncation itself can flip that digit).
fn numbers_agree(actual: f64, expected: f64) -> bool {
    if actual == expected {
        return true;
    }
    if !actual.is_finite() || !expected.is_finite() {
        return false;
    }
    let unit = if expected == 0.0 {
        1e-9
    } else {
        10f64.powf(expected.abs().log10().ceil() - 6.0).max(1e-9)
    };
    (strip_precision(actual) - strip_precision(expected)).abs() <= unit * 1.01
}

fn deep_equal(actual: &Json, expected: &Json) -> bool {
    match (actual, expected) {
        (Json::Number(a), Json::Number(e)) => match (a.as_f64(), e.as_f64()) {
            (Some(a), Some(e)) => numbers_agree(a, e),
            _ => false,
        },
        (Json::Array(a), Json::Array(e)) => {
            a.len() == e.len() && a.iter().zip(e).all(|(a, e)| deep_equal(a, e))
        }
        (Json::Object(a), Json::Object(e)) => {
            a.len() == e.len()
                && a.iter()
                    .all(|(k, v)| e.get(k).is_some_and(|ev| deep_equal(v, ev)))
        }
        _ => actual == expected,
    }
}

/// Why a fixture failed, or `None` when it passed.
fn run_fixture(fixture: &Json) -> Option<String> {
    let Some((spec, hints)) = property_spec(fixture.get("propertySpec")) else {
        return Some("propertySpec type is not modelled (add it to SKIPPED with a reason)".into());
    };
    let expression = fixture.get("expression").unwrap_or(&Json::Null);
    let expected = fixture.get("expected")?;
    let compiled = expected.get("compiled")?;
    // The reference harness feeds the fixture straight to `createPropertyExpression`, so a bare
    // array is parsed as an expression (and fails); `parse_property` would read it as a constant.
    let parsed = if expression.is_array() && !is_expression(expression) {
        Expression::parse(expression, Some(&spec.kind))
    } else {
        Expression::parse_property_with(expression, &spec, hints)
    };
    if compiled.get("result").and_then(Json::as_str) == Some("error") {
        return match parsed {
            Err(_) => None,
            Ok(_) => Some(format!(
                "parsed, but the reference reports errors {}",
                compiled.get("errors").unwrap_or(&Json::Null)
            )),
        };
    }
    let parsed = match parsed {
        Ok(e) => e,
        Err(err) => return Some(format!("did not parse: {err}")),
    };
    let expect_bool = |key: &str| compiled.get(key).and_then(Json::as_bool).unwrap_or(true);
    if parsed.is_feature_constant() != expect_bool("isFeatureConstant") {
        return Some(format!(
            "isFeatureConstant: ours {}, expected {}",
            parsed.is_feature_constant(),
            expect_bool("isFeatureConstant")
        ));
    }
    if parsed.is_zoom_constant() != expect_bool("isZoomConstant") {
        return Some(format!(
            "isZoomConstant: ours {}, expected {}",
            parsed.is_zoom_constant(),
            expect_bool("isZoomConstant")
        ));
    }
    let expected_type = compiled.get("type").and_then(Json::as_str).unwrap_or("");
    if parsed.result_type().name() != expected_type {
        return Some(format!(
            "type: ours {}, expected {expected_type}",
            parsed.result_type().name()
        ));
    }

    let inputs = fixture
        .get("inputs")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default();
    let outputs = expected
        .get("outputs")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default();
    let global_state = value_map(fixture.get("globalState"));
    for (i, (input, expected_output)) in inputs.iter().zip(&outputs).enumerate() {
        let globals = input.get(0).unwrap_or(&Json::Null);
        let feature_json = input.get(1).unwrap_or(&Json::Null);
        let available_images: Vec<String> = globals
            .get("availableImages")
            .and_then(Json::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Json::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        let feature = Feature {
            id: feature_json.get("id").map(Value::from_json),
            kind: geometry_kind(feature_json.get("geometry")),
            properties: value_map(feature_json.get("properties")),
            geometry: Vec::new(),
        };
        let feature_state = value_map(feature_json.get("featureState"));
        let ctx = Context {
            zoom: globals.get("zoom").and_then(Json::as_f64).unwrap_or(0.0),
            feature: Some(&feature),
            feature_state: Some(&feature_state),
            global_state: fixture.get("globalState").map(|_| &global_state),
            tile: None,
            available_images: Some(&available_images),
        };
        let result = parsed.evaluate(&ctx);
        let expects_error = expected_output
            .as_object()
            .is_some_and(|o| o.contains_key("error") && o.len() == 1);
        match (result, expects_error) {
            (Err(_), true) => {}
            (Ok(value), true) => {
                return Some(format!(
                    "input {i}: evaluated to {}, expected the error {expected_output}",
                    to_json(&value, parsed.result_type(), &available_images)
                ));
            }
            (Err(err), false) => {
                return Some(format!(
                    "input {i}: failed with {err}, expected {expected_output}"
                ));
            }
            (Ok(value), false) => {
                let actual = to_json(&value, parsed.result_type(), &available_images);
                if !deep_equal(&actual, expected_output) {
                    return Some(format!(
                        "input {i}: ours {actual}, expected {expected_output}"
                    ));
                }
            }
        }
    }
    None
}

#[test]
fn maplibre_expression_fixtures() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/expression");
    let mut files = Vec::new();
    fixture_files(&root, &mut files);
    assert!(!files.is_empty(), "no fixtures under {}", root.display());

    #[derive(Default)]
    struct Tally {
        passed: usize,
        failed: usize,
        skipped: usize,
    }
    let mut by_dir: BTreeMap<String, Tally> = BTreeMap::new();
    let mut failures: Vec<String> = Vec::new();
    let mut skips: Vec<String> = Vec::new();
    let (mut passed, mut failed, mut skipped) = (0, 0, 0);
    for path in &files {
        let name = path
            .parent()
            .and_then(|p| p.strip_prefix(&root).ok())
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let top = name.split('/').next().unwrap_or("").to_string();
        let tally = by_dir.entry(top).or_default();
        if let Some(reason) = skip_reason(&name) {
            skipped += 1;
            tally.skipped += 1;
            skips.push(format!("{name}: {reason}"));
            continue;
        }
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let fixture: Json = match serde_json::from_str(&text) {
            Ok(j) => j,
            Err(err) => {
                failed += 1;
                tally.failed += 1;
                failures.push(format!("{name}: unreadable fixture: {err}"));
                continue;
            }
        };
        match run_fixture(&fixture) {
            None => {
                passed += 1;
                tally.passed += 1;
            }
            Some(why) => {
                failed += 1;
                tally.failed += 1;
                failures.push(format!("{name}: {why}"));
            }
        }
    }

    println!(
        "{:<24} {:>6} {:>6} {:>7}",
        "directory", "pass", "fail", "skip"
    );
    for (dir, t) in &by_dir {
        println!("{dir:<24} {:>6} {:>6} {:>7}", t.passed, t.failed, t.skipped);
    }
    println!("{:<24} {passed:>6} {failed:>6} {skipped:>7}", "total");
    println!();
    println!("skipped ({}):", skips.len());
    for s in &skips {
        println!("  {s}");
    }
    if !failures.is_empty() {
        println!();
        println!("failed ({}):", failures.len());
        for f in &failures {
            println!("  {f}");
        }
    }
    assert_eq!(
        failed, 0,
        "{failed} expression fixtures failed (see the list above)"
    );
}
