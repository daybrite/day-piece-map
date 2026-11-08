// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! Mapbox Vector Tile decoding (<https://github.com/mapbox/vector-tile-spec,> version 2): a
//! protobuf of layers, each with a key and value table and features whose geometry is a
//! command stream in tile coordinates. Hand-rolled: the format is three message types.
//!
//! The wire format is protobuf's: each field is a tag varint `(field << 3) | wire_type`
//! followed by a varint (type 0), eight bytes (1), a length-prefixed blob (2) or four bytes
//! (5). Fields this decoder does not know are skipped by their wire type, so tiles from an
//! encoder with extensions still decode. A malformed tile answers [`Error::Invalid`], never a
//! panic: every read is bounds-checked and the geometry decoder refuses a command whose count
//! exceeds what is left of the stream before allocating anything for it.
//!
//! Geometry comes out as [`Feature::geometry`] parts: each point of a (multi)point as a
//! one-point part, each line of a (multi)line as a part, each ring of a (multi)polygon as a
//! part in file order, with the ring's first point repeated at the end where the stream's
//! `ClosePath` command closes it (the tile encodes the closure as a command, not a point).

use std::collections::BTreeMap;

use day_geometry::Point;

use crate::{Error, Feature, GeometryType, Value};

/// One decoded tile.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Tile {
    pub layers: Vec<Layer>,
}

/// One layer of a tile: its name, its extent (the coordinate range across the tile, 4096 by
/// convention) and its features.
#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    pub name: String,
    pub version: u32,
    pub extent: u32,
    pub features: Vec<Feature>,
}

impl Tile {
    /// The layer named `name`, if the tile has it.
    pub fn layer(&self, name: &str) -> Option<&Layer> {
        self.layers.iter().find(|l| l.name == name)
    }
}

/// The extent a layer has when it does not say (the spec's default).
const DEFAULT_EXTENT: u32 = 4096;

/// Decode a tile's protobuf bytes (already inflated).
pub fn decode(bytes: &[u8]) -> Result<Tile, Error> {
    let mut reader = Reader::new(bytes);
    let mut layers = Vec::new();
    while let Some((field, wire)) = reader.tag()? {
        match (field, wire) {
            (3, Wire::Len) => layers.push(decode_layer(reader.bytes()?)?),
            _ => reader.skip(wire)?,
        }
    }
    Ok(Tile { layers })
}

// ---------------------------------------------------------------------------
// Protobuf primitives
// ---------------------------------------------------------------------------

/// A protobuf wire type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wire {
    Varint,
    Fixed64,
    Len,
    Fixed32,
}

/// A cursor over a message's bytes.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

fn invalid(what: &str) -> Error {
    Error::Invalid(format!("mvt: {what}"))
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, pos: 0 }
    }

    fn is_empty(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    /// A base-128 varint, at most ten bytes.
    fn varint(&mut self) -> Result<u64, Error> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self
                .bytes
                .get(self.pos)
                .ok_or_else(|| invalid("truncated varint"))?;
            self.pos += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(invalid("varint longer than ten bytes"))
    }

    /// The next field's number and wire type, or `None` at the end of the message.
    fn tag(&mut self) -> Result<Option<(u64, Wire)>, Error> {
        if self.is_empty() {
            return Ok(None);
        }
        let key = self.varint()?;
        let wire = match key & 7 {
            0 => Wire::Varint,
            1 => Wire::Fixed64,
            2 => Wire::Len,
            5 => Wire::Fixed32,
            other => return Err(invalid(&format!("wire type {other}"))),
        };
        Ok(Some((key >> 3, wire)))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| invalid("length overflow"))?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or_else(|| invalid("truncated field"))?;
        self.pos = end;
        Ok(slice)
    }

    /// A length-delimited field's payload.
    fn bytes(&mut self) -> Result<&'a [u8], Error> {
        let len = self.varint()?;
        let len = usize::try_from(len).map_err(|_| invalid("length overflow"))?;
        self.take(len)
    }

    fn fixed32(&mut self) -> Result<u32, Error> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn fixed64(&mut self) -> Result<u64, Error> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Step over a field this decoder does not read.
    fn skip(&mut self, wire: Wire) -> Result<(), Error> {
        match wire {
            Wire::Varint => self.varint().map(|_| ()),
            Wire::Fixed64 => self.take(8).map(|_| ()),
            Wire::Len => self.bytes().map(|_| ()),
            Wire::Fixed32 => self.take(4).map(|_| ()),
        }
    }

    /// A `repeated uint32` field: packed (a blob of varints) or one value per occurrence.
    fn repeated_u32(&mut self, wire: Wire, into: &mut Vec<u32>) -> Result<(), Error> {
        match wire {
            Wire::Len => {
                let mut inner = Reader::new(self.bytes()?);
                while !inner.is_empty() {
                    into.push(to_u32(inner.varint()?)?);
                }
                Ok(())
            }
            Wire::Varint => {
                into.push(to_u32(self.varint()?)?);
                Ok(())
            }
            _ => Err(invalid("repeated uint32 with a fixed-size wire type")),
        }
    }
}

fn to_u32(v: u64) -> Result<u32, Error> {
    u32::try_from(v).map_err(|_| invalid("uint32 out of range"))
}

/// Protobuf's zigzag decoding of a signed 32-bit integer (the geometry's parameters).
fn zigzag32(v: u32) -> i32 {
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

/// Protobuf's zigzag decoding of a signed 64-bit integer (a `sint64` value).
fn zigzag64(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

fn utf8(bytes: &[u8], what: &str) -> Result<String, Error> {
    String::from_utf8(bytes.to_vec()).map_err(|_| invalid(&format!("{what} is not UTF-8")))
}

// ---------------------------------------------------------------------------
// Layer, value, feature
// ---------------------------------------------------------------------------

fn decode_layer(bytes: &[u8]) -> Result<Layer, Error> {
    let mut reader = Reader::new(bytes);
    let mut name = None;
    let mut version = 1;
    let mut extent = DEFAULT_EXTENT;
    let mut keys = Vec::new();
    let mut values = Vec::new();
    // Features may precede the key and value tables in the stream; decode them once the
    // tables are complete.
    let mut raw_features = Vec::new();
    while let Some((field, wire)) = reader.tag()? {
        match (field, wire) {
            (1, Wire::Len) => name = Some(utf8(reader.bytes()?, "layer name")?),
            (2, Wire::Len) => raw_features.push(reader.bytes()?),
            (3, Wire::Len) => keys.push(utf8(reader.bytes()?, "key")?),
            (4, Wire::Len) => values.push(decode_value(reader.bytes()?)?),
            (5, Wire::Varint) => extent = to_u32(reader.varint()?)?,
            (15, Wire::Varint) => version = to_u32(reader.varint()?)?,
            _ => reader.skip(wire)?,
        }
    }
    let name = name.ok_or_else(|| invalid("layer without a name"))?;
    if extent == 0 {
        return Err(invalid("layer extent 0"));
    }
    let features = raw_features
        .into_iter()
        .map(|raw| decode_feature(raw, &keys, &values))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Layer {
        name,
        version,
        extent,
        features,
    })
}

/// A `Value` message: one of the typed fields set. An empty message is `Null`; an unknown
/// field is skipped, so a value of a kind a later spec adds decodes as `Null` rather than
/// failing the tile.
fn decode_value(bytes: &[u8]) -> Result<Value, Error> {
    let mut reader = Reader::new(bytes);
    let mut value = Value::Null;
    while let Some((field, wire)) = reader.tag()? {
        value = match (field, wire) {
            (1, Wire::Len) => Value::String(utf8(reader.bytes()?, "string value")?),
            (2, Wire::Fixed32) => Value::Number(f64::from(f32::from_bits(reader.fixed32()?))),
            (3, Wire::Fixed64) => Value::Number(f64::from_bits(reader.fixed64()?)),
            (4, Wire::Varint) => Value::Number(reader.varint()? as i64 as f64),
            (5, Wire::Varint) => Value::Number(reader.varint()? as f64),
            (6, Wire::Varint) => Value::Number(zigzag64(reader.varint()?) as f64),
            (7, Wire::Varint) => Value::Bool(reader.varint()? != 0),
            _ => {
                reader.skip(wire)?;
                continue;
            }
        };
    }
    Ok(value)
}

fn decode_feature(bytes: &[u8], keys: &[String], values: &[Value]) -> Result<Feature, Error> {
    let mut reader = Reader::new(bytes);
    let mut id = None;
    let mut kind = GeometryType::Unknown;
    let mut tags = Vec::new();
    let mut commands = Vec::new();
    while let Some((field, wire)) = reader.tag()? {
        match (field, wire) {
            (1, Wire::Varint) => id = Some(Value::Number(reader.varint()? as f64)),
            (2, Wire::Len | Wire::Varint) => reader.repeated_u32(wire, &mut tags)?,
            (3, Wire::Varint) => {
                kind = match reader.varint()? {
                    0 => GeometryType::Unknown,
                    1 => GeometryType::Point,
                    2 => GeometryType::LineString,
                    3 => GeometryType::Polygon,
                    other => return Err(invalid(&format!("geometry type {other}"))),
                }
            }
            (4, Wire::Len | Wire::Varint) => reader.repeated_u32(wire, &mut commands)?,
            _ => reader.skip(wire)?,
        }
    }
    if tags.len() % 2 != 0 {
        return Err(invalid("feature tags are not key/value pairs"));
    }
    let mut properties = BTreeMap::new();
    for pair in tags.as_chunks::<2>().0 {
        let key = keys
            .get(pair[0] as usize)
            .ok_or_else(|| invalid(&format!("tag key index {} out of range", pair[0])))?;
        let value = values
            .get(pair[1] as usize)
            .ok_or_else(|| invalid(&format!("tag value index {} out of range", pair[1])))?;
        properties.insert(key.clone(), value.clone());
    }
    Ok(Feature {
        id,
        kind,
        properties,
        geometry: decode_geometry(&commands, kind)?,
    })
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

const MOVE_TO: u32 = 1;
const LINE_TO: u32 = 2;
const CLOSE_PATH: u32 = 7;

/// Decode a geometry command stream into parts, per [`Feature::geometry`].
///
/// `MoveTo` starts a part (each point of a point feature is its own part; a line or ring
/// begins with one), `LineTo` extends the current part, `ClosePath` appends the current ring's
/// first point to its end. A `LineTo` before any `MoveTo`, a count of zero, or a count that
/// overruns the stream is invalid. A `ClosePath` on a point or line feature (the spec forbids
/// it, some encoders emit it) is ignored, and one with a count other than one closes once.
/// An `Unknown` geometry type decodes with the line rules so its commands are at least
/// well-formed; the renderer ignores such features.
fn decode_geometry(commands: &[u32], kind: GeometryType) -> Result<Vec<Vec<Point>>, Error> {
    let mut parts: Vec<Vec<Point>> = Vec::new();
    let (mut x, mut y) = (0i64, 0i64);
    let mut i = 0;
    while i < commands.len() {
        let command = commands[i];
        i += 1;
        let (id, count) = (command & 7, (command >> 3) as usize);
        match id {
            MOVE_TO | LINE_TO => {
                if count == 0 {
                    return Err(invalid("geometry command with count 0"));
                }
                if count.checked_mul(2).is_none_or(|n| n > commands.len() - i) {
                    return Err(invalid("geometry command overruns the stream"));
                }
                if id == LINE_TO && parts.is_empty() {
                    return Err(invalid("LineTo before any MoveTo"));
                }
                for _ in 0..count {
                    x += i64::from(zigzag32(commands[i]));
                    y += i64::from(zigzag32(commands[i + 1]));
                    i += 2;
                    let point = Point {
                        x: x as f64,
                        y: y as f64,
                    };
                    if id == MOVE_TO || kind == GeometryType::Point {
                        parts.push(vec![point]);
                    } else if let Some(part) = parts.last_mut() {
                        part.push(point);
                    }
                }
            }
            CLOSE_PATH => {
                if kind == GeometryType::Polygon
                    && let Some(ring) = parts.last_mut()
                    && let Some(&first) = ring.first()
                    && ring.last() != Some(&first)
                {
                    ring.push(first);
                }
            }
            other => return Err(invalid(&format!("geometry command {other}"))),
        }
    }
    Ok(parts)
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// Encode a tile: the inverse of [`decode`], for a test fixture or a sample served from inside
/// an app. Geometry is rounded to integer tile units; a polygon part is closed with
/// `ClosePath` (a repeated last point is dropped first); a feature's `id` is written when it
/// is a whole number; a property that is not a string, a number or a boolean is skipped, as
/// the format has no encoding for it.
pub fn encode(tile: &Tile) -> Vec<u8> {
    let mut out = Vec::new();
    for layer in &tile.layers {
        put_bytes(&mut out, 3, &encode_layer(layer));
    }
    out
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn put_field_varint(out: &mut Vec<u8>, field: u64, v: u64) {
    put_varint(out, field << 3);
    put_varint(out, v);
}

fn put_bytes(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
    put_varint(out, (field << 3) | 2);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn put_packed(out: &mut Vec<u8>, field: u64, values: &[u32]) {
    let mut body = Vec::new();
    for &v in values {
        put_varint(&mut body, u64::from(v));
    }
    put_bytes(out, field, &body);
}

fn zigzag_encode32(v: i64) -> u32 {
    let v = v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    ((v << 1) ^ (v >> 31)) as u32
}

fn encode_value(value: &Value) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    match value {
        Value::String(s) => put_bytes(&mut out, 1, s.as_bytes()),
        Value::Number(n) if n.fract() == 0.0 && n.abs() < 9.0e15 => {
            if *n < 0.0 {
                put_field_varint(&mut out, 6, ((*n as i64) << 1 ^ ((*n as i64) >> 63)) as u64);
            } else {
                put_field_varint(&mut out, 5, *n as u64);
            }
        }
        Value::Number(n) => {
            put_varint(&mut out, (3 << 3) | 1);
            out.extend_from_slice(&n.to_bits().to_le_bytes());
        }
        Value::Bool(b) => put_field_varint(&mut out, 7, u64::from(*b)),
        _ => return None,
    }
    Some(out)
}

fn encode_geometry(feature: &Feature) -> Vec<u32> {
    let mut out = Vec::new();
    let (mut cx, mut cy) = (0i64, 0i64);
    let mut push = |out: &mut Vec<u32>, p: Point| {
        let (ix, iy) = (p.x.round() as i64, p.y.round() as i64);
        out.push(zigzag_encode32(ix - cx));
        out.push(zigzag_encode32(iy - cy));
        cx = ix;
        cy = iy;
    };
    match feature.kind {
        GeometryType::Point => {
            let points: Vec<Point> = feature
                .geometry
                .iter()
                .filter_map(|part| part.first().copied())
                .collect();
            if !points.is_empty() {
                out.push((points.len() as u32) << 3 | 1);
                for p in points {
                    push(&mut out, p);
                }
            }
        }
        _ => {
            let closed = feature.kind == GeometryType::Polygon;
            for part in &feature.geometry {
                let mut points: Vec<Point> = Vec::new();
                for p in part {
                    let q = Point::new(p.x.round(), p.y.round());
                    if points.last() != Some(&q) {
                        points.push(q);
                    }
                }
                if closed && points.len() > 1 && points.first() == points.last() {
                    points.pop();
                }
                if points.len() < 2 {
                    continue;
                }
                out.push(1 << 3 | 1);
                push(&mut out, points[0]);
                out.push(((points.len() - 1) as u32) << 3 | 2);
                for &p in &points[1..] {
                    push(&mut out, p);
                }
                if closed {
                    out.push(1 << 3 | 7);
                }
            }
        }
    }
    out
}

fn encode_layer(layer: &Layer) -> Vec<u8> {
    let mut keys: Vec<&str> = Vec::new();
    let mut values: Vec<Vec<u8>> = Vec::new();
    let mut out = Vec::new();
    put_field_varint(&mut out, 15, u64::from(layer.version.max(1)));
    put_bytes(&mut out, 1, layer.name.as_bytes());
    for feature in &layer.features {
        let mut body = Vec::new();
        if let Some(Value::Number(id)) = feature.id
            && id.fract() == 0.0
            && id >= 0.0
        {
            put_field_varint(&mut body, 1, id as u64);
        }
        let mut tags = Vec::new();
        for (key, value) in &feature.properties {
            let Some(encoded) = encode_value(value) else {
                continue;
            };
            let k = keys.iter().position(|k| k == key).unwrap_or_else(|| {
                keys.push(key);
                keys.len() - 1
            });
            let v = values
                .iter()
                .position(|v| *v == encoded)
                .unwrap_or_else(|| {
                    values.push(encoded);
                    values.len() - 1
                });
            tags.push(k as u32);
            tags.push(v as u32);
        }
        put_packed(&mut body, 2, &tags);
        let kind = match feature.kind {
            GeometryType::Unknown => 0,
            GeometryType::Point => 1,
            GeometryType::LineString => 2,
            GeometryType::Polygon => 3,
        };
        put_field_varint(&mut body, 3, kind);
        put_packed(&mut body, 4, &encode_geometry(feature));
        put_bytes(&mut out, 2, &body);
    }
    for key in keys {
        put_bytes(&mut out, 3, key.as_bytes());
    }
    for value in values {
        put_bytes(&mut out, 4, &value);
    }
    put_field_varint(&mut out, 5, u64::from(layer.extent));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- A small protobuf encoder, enough to build tiles the decoder has to read. ----------

    fn varint(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn field_varint(out: &mut Vec<u8>, field: u64, v: u64) {
        varint(out, field << 3);
        varint(out, v);
    }

    fn field_bytes(out: &mut Vec<u8>, field: u64, bytes: &[u8]) {
        varint(out, (field << 3) | 2);
        varint(out, bytes.len() as u64);
        out.extend_from_slice(bytes);
    }

    fn field_fixed32(out: &mut Vec<u8>, field: u64, v: u32) {
        varint(out, (field << 3) | 5);
        out.extend_from_slice(&v.to_le_bytes());
    }

    fn field_fixed64(out: &mut Vec<u8>, field: u64, v: u64) {
        varint(out, (field << 3) | 1);
        out.extend_from_slice(&v.to_le_bytes());
    }

    fn packed(out: &mut Vec<u8>, field: u64, values: &[u32]) {
        let mut body = Vec::new();
        for &v in values {
            varint(&mut body, u64::from(v));
        }
        field_bytes(out, field, &body);
    }

    fn zig32(v: i32) -> u32 {
        ((v << 1) ^ (v >> 31)) as u32
    }

    fn zig64(v: i64) -> u64 {
        ((v << 1) ^ (v >> 63)) as u64
    }

    fn command(id: u32, count: u32) -> u32 {
        (count << 3) | id
    }

    enum Val<'a> {
        Str(&'a str),
        Float(f32),
        Double(f64),
        Int(i64),
        Uint(u64),
        Sint(i64),
        Bool(bool),
    }

    fn value(v: &Val<'_>) -> Vec<u8> {
        let mut out = Vec::new();
        match v {
            Val::Str(s) => field_bytes(&mut out, 1, s.as_bytes()),
            Val::Float(f) => field_fixed32(&mut out, 2, f.to_bits()),
            Val::Double(d) => field_fixed64(&mut out, 3, d.to_bits()),
            Val::Int(i) => field_varint(&mut out, 4, *i as u64),
            Val::Uint(u) => field_varint(&mut out, 5, *u),
            Val::Sint(s) => field_varint(&mut out, 6, zig64(*s)),
            Val::Bool(b) => field_varint(&mut out, 7, u64::from(*b)),
        }
        out
    }

    struct Feat {
        id: Option<u64>,
        kind: u64,
        tags: Vec<u32>,
        geometry: Vec<u32>,
    }

    fn feature(f: &Feat) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(id) = f.id {
            field_varint(&mut out, 1, id);
        }
        packed(&mut out, 2, &f.tags);
        field_varint(&mut out, 3, f.kind);
        packed(&mut out, 4, &f.geometry);
        out
    }

    fn layer(name: &str, keys: &[&str], values: &[Val<'_>], features: &[Feat]) -> Vec<u8> {
        let mut out = Vec::new();
        field_varint(&mut out, 15, 2);
        field_bytes(&mut out, 1, name.as_bytes());
        for f in features {
            field_bytes(&mut out, 2, &feature(f));
        }
        for k in keys {
            field_bytes(&mut out, 3, k.as_bytes());
        }
        for v in values {
            field_bytes(&mut out, 4, &value(v));
        }
        field_varint(&mut out, 5, 4096);
        out
    }

    fn tile(layers: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        for l in layers {
            field_bytes(&mut out, 3, l);
        }
        out
    }

    /// A `MoveTo` to `(x, y)` followed by `LineTo`s through `rest`, from the cursor `at`,
    /// returning the commands and the new cursor.
    fn path(at: &mut (i32, i32), points: &[(i32, i32)], close: bool) -> Vec<u32> {
        let mut out = vec![command(MOVE_TO, 1)];
        out.push(zig32(points[0].0 - at.0));
        out.push(zig32(points[0].1 - at.1));
        *at = points[0];
        if points.len() > 1 {
            out.push(command(LINE_TO, (points.len() - 1) as u32));
            for p in &points[1..] {
                out.push(zig32(p.0 - at.0));
                out.push(zig32(p.1 - at.1));
                *at = *p;
            }
        }
        if close {
            out.push(command(CLOSE_PATH, 1));
        }
        out
    }

    fn pt(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    // -- Tests -------------------------------------------------------------------------------

    #[test]
    fn empty_buffer_is_an_empty_tile() {
        assert_eq!(decode(&[]).unwrap(), Tile::default());
    }

    #[test]
    fn every_value_type_round_trips() {
        let values = [
            Val::Str("hello"),
            Val::Float(1.5),
            Val::Double(-2.25),
            Val::Int(-7),
            Val::Uint(9_000_000_000),
            Val::Sint(-123456789012),
            Val::Bool(true),
            Val::Bool(false),
        ];
        let keys = ["s", "f", "d", "i", "u", "si", "t", "ff"];
        let tags: Vec<u32> = (0..8u32).flat_map(|i| [i, i]).collect();
        let f = Feat {
            id: Some(42),
            kind: 1,
            tags,
            geometry: vec![command(MOVE_TO, 1), zig32(25), zig32(17)],
        };
        let bytes = tile(&[layer("props", &keys, &values, &[f])]);
        let t = decode(&bytes).unwrap();
        let l = t.layer("props").expect("layer");
        assert_eq!((l.version, l.extent), (2, 4096));
        let f = &l.features[0];
        assert_eq!(f.id, Some(Value::Number(42.0)));
        assert_eq!(f.kind, GeometryType::Point);
        assert_eq!(f.geometry, vec![vec![pt(25.0, 17.0)]]);
        let p = &f.properties;
        assert_eq!(p["s"], Value::String("hello".into()));
        assert_eq!(p["f"], Value::Number(1.5));
        assert_eq!(p["d"], Value::Number(-2.25));
        assert_eq!(p["i"], Value::Number(-7.0));
        assert_eq!(p["u"], Value::Number(9_000_000_000.0));
        assert_eq!(p["si"], Value::Number(-123456789012.0));
        assert_eq!(p["t"], Value::Bool(true));
        assert_eq!(p["ff"], Value::Bool(false));
    }

    #[test]
    fn multipoint_is_one_part_per_point() {
        let f = Feat {
            id: None,
            kind: 1,
            tags: vec![],
            geometry: vec![command(MOVE_TO, 2), zig32(5), zig32(7), zig32(3), zig32(2)],
        };
        let t = decode(&tile(&[layer("pts", &[], &[], &[f])])).unwrap();
        let f = &t.layers[0].features[0];
        assert_eq!(f.id, None);
        assert_eq!(f.geometry, vec![vec![pt(5.0, 7.0)], vec![pt(8.0, 9.0)]]);
    }

    #[test]
    fn multilinestring_is_one_part_per_line() {
        let mut at = (0, 0);
        let mut g = path(&mut at, &[(2, 2), (2, 10), (10, 10)], false);
        g.extend(path(&mut at, &[(1, 1), (3, 5)], false));
        let f = Feat {
            id: Some(1),
            kind: 2,
            tags: vec![],
            geometry: g,
        };
        let t = decode(&tile(&[layer("lines", &[], &[], &[f])])).unwrap();
        let f = &t.layers[0].features[0];
        assert_eq!(f.kind, GeometryType::LineString);
        assert_eq!(
            f.geometry,
            vec![
                vec![pt(2.0, 2.0), pt(2.0, 10.0), pt(10.0, 10.0)],
                vec![pt(1.0, 1.0), pt(3.0, 5.0)],
            ]
        );
    }

    #[test]
    fn multipolygon_with_hole_keeps_ring_order_and_closes_rings() {
        // The spec's multipolygon example: a square, then a square with a hole.
        let mut at = (0, 0);
        let mut g = path(&mut at, &[(0, 0), (10, 0), (10, 10), (0, 10)], true);
        g.extend(path(
            &mut at,
            &[(11, 11), (20, 11), (20, 20), (11, 20)],
            true,
        ));
        g.extend(path(
            &mut at,
            &[(13, 13), (13, 17), (17, 17), (17, 13)],
            true,
        ));
        let f = Feat {
            id: Some(3),
            kind: 3,
            tags: vec![],
            geometry: g,
        };
        let t = decode(&tile(&[layer("polys", &[], &[], &[f])])).unwrap();
        let f = &t.layers[0].features[0];
        assert_eq!(f.kind, GeometryType::Polygon);
        assert_eq!(f.geometry.len(), 3);
        assert_eq!(
            f.geometry[0],
            vec![
                pt(0.0, 0.0),
                pt(10.0, 0.0),
                pt(10.0, 10.0),
                pt(0.0, 10.0),
                pt(0.0, 0.0)
            ]
        );
        assert_eq!(f.geometry[1].first(), Some(&pt(11.0, 11.0)));
        assert_eq!(f.geometry[1].last(), Some(&pt(11.0, 11.0)));
        assert_eq!(f.geometry[1].len(), 5);
        assert_eq!(
            f.geometry[2],
            vec![
                pt(13.0, 13.0),
                pt(13.0, 17.0),
                pt(17.0, 17.0),
                pt(17.0, 13.0),
                pt(13.0, 13.0)
            ]
        );
    }

    #[test]
    fn close_path_on_a_line_is_ignored() {
        let f = Feat {
            id: None,
            kind: 2,
            tags: vec![],
            geometry: vec![
                command(MOVE_TO, 1),
                zig32(1),
                zig32(1),
                command(LINE_TO, 1),
                zig32(4),
                zig32(0),
                command(CLOSE_PATH, 1),
            ],
        };
        let t = decode(&tile(&[layer("l", &[], &[], &[f])])).unwrap();
        assert_eq!(
            t.layers[0].features[0].geometry,
            vec![vec![pt(1.0, 1.0), pt(5.0, 1.0)]]
        );
    }

    #[test]
    fn multiple_layers_defaults_and_lookup() {
        let mut bare = Vec::new();
        field_bytes(&mut bare, 1, b"bare");
        let bytes = tile(&[layer("water", &[], &[], &[]), bare]);
        let t = decode(&bytes).unwrap();
        assert_eq!(t.layers.len(), 2);
        assert!(t.layer("water").is_some());
        let bare = t.layer("bare").expect("bare layer");
        assert_eq!((bare.version, bare.extent), (1, 4096));
        assert!(bare.features.is_empty());
        assert!(t.layer("roads").is_none());
    }

    #[test]
    fn unknown_fields_are_skipped() {
        let mut l = layer("x", &["k"], &[Val::Str("v")], &[]);
        field_varint(&mut l, 99, 7);
        field_bytes(&mut l, 98, b"extension");
        field_fixed32(&mut l, 97, 1);
        field_fixed64(&mut l, 96, 2);
        let mut v = value(&Val::Str("v"));
        field_fixed32(&mut v, 20, 5);
        field_bytes(&mut l, 4, &v);
        let mut bytes = tile(&[l]);
        field_varint(&mut bytes, 50, 1);
        let t = decode(&bytes).unwrap();
        assert_eq!(t.layers[0].name, "x");
        assert_eq!(t.layers[0].features.len(), 0);
    }

    #[test]
    fn unknown_value_kind_is_null() {
        let mut v = Vec::new();
        field_varint(&mut v, 9, 1);
        let f = Feat {
            id: None,
            kind: 1,
            tags: vec![0, 0],
            geometry: vec![command(MOVE_TO, 1), 0, 0],
        };
        let mut l = Vec::new();
        field_bytes(&mut l, 1, b"n");
        field_bytes(&mut l, 2, &feature(&f));
        field_bytes(&mut l, 3, b"k");
        field_bytes(&mut l, 4, &v);
        let t = decode(&tile(&[l])).unwrap();
        assert_eq!(t.layers[0].features[0].properties["k"], Value::Null);
    }

    #[test]
    fn unpacked_repeated_fields_decode_too() {
        // proto2 parsers must accept a repeated scalar either packed or one per tag.
        let mut f = Vec::new();
        field_varint(&mut f, 2, 0);
        field_varint(&mut f, 2, 0);
        field_varint(&mut f, 3, 1);
        for c in [command(MOVE_TO, 1), zig32(3), zig32(4)] {
            field_varint(&mut f, 4, u64::from(c));
        }
        let mut l = Vec::new();
        field_bytes(&mut l, 1, b"n");
        field_bytes(&mut l, 2, &f);
        field_bytes(&mut l, 3, b"k");
        field_bytes(&mut l, 4, &value(&Val::Int(1)));
        let t = decode(&tile(&[l])).unwrap();
        let f = &t.layers[0].features[0];
        assert_eq!(f.properties["k"], Value::Number(1.0));
        assert_eq!(f.geometry, vec![vec![pt(3.0, 4.0)]]);
    }

    #[test]
    fn bad_tag_indexes_are_invalid() {
        let f = Feat {
            id: None,
            kind: 1,
            tags: vec![0, 5],
            geometry: vec![command(MOVE_TO, 1), 0, 0],
        };
        let bytes = tile(&[layer("n", &["k"], &[Val::Bool(true)], &[f])]);
        assert!(matches!(decode(&bytes), Err(Error::Invalid(_))));
        let f = Feat {
            id: None,
            kind: 1,
            tags: vec![3, 0],
            geometry: vec![command(MOVE_TO, 1), 0, 0],
        };
        let bytes = tile(&[layer("n", &["k"], &[Val::Bool(true)], &[f])]);
        assert!(matches!(decode(&bytes), Err(Error::Invalid(_))));
        let f = Feat {
            id: None,
            kind: 1,
            tags: vec![0],
            geometry: vec![command(MOVE_TO, 1), 0, 0],
        };
        let bytes = tile(&[layer("n", &["k"], &[Val::Bool(true)], &[f])]);
        assert!(matches!(decode(&bytes), Err(Error::Invalid(_))));
    }

    #[test]
    fn bad_geometry_is_invalid_not_a_panic() {
        let cases: Vec<Vec<u32>> = vec![
            vec![command(LINE_TO, 1), 0, 0],
            vec![command(MOVE_TO, 0)],
            vec![command(MOVE_TO, 1), 0],
            vec![command(MOVE_TO, u32::MAX >> 3), 0, 0],
            vec![command(5, 1), 0, 0],
            vec![command(MOVE_TO, 1), 0, 0, command(LINE_TO, 3), 0, 0, 0, 0],
        ];
        for g in cases {
            let f = Feat {
                id: None,
                kind: 2,
                tags: vec![],
                geometry: g.clone(),
            };
            let bytes = tile(&[layer("n", &[], &[], &[f])]);
            assert!(matches!(decode(&bytes), Err(Error::Invalid(_))), "{g:?}");
        }
    }

    #[test]
    fn truncated_and_corrupt_buffers_error() {
        let mut at = (0, 0);
        let g = path(&mut at, &[(0, 0), (10, 0), (10, 10)], true);
        let f = Feat {
            id: Some(1),
            kind: 3,
            tags: vec![0, 0],
            geometry: g,
        };
        let bytes = tile(&[layer("n", &["k"], &[Val::Str("v")], &[f])]);
        assert!(decode(&bytes).is_ok());
        for cut in 1..bytes.len() {
            // Every strict prefix either decodes (a prefix that ends on a field boundary and
            // still has the referenced tables) or errors; it never panics.
            let _ = decode(&bytes[..cut]);
        }
        // A length that overruns the buffer.
        let mut bad = Vec::new();
        varint(&mut bad, (3 << 3) | 2);
        varint(&mut bad, 1000);
        bad.extend_from_slice(&[0u8; 4]);
        assert!(matches!(decode(&bad), Err(Error::Invalid(_))));
        // A length that overflows usize.
        let mut bad = Vec::new();
        varint(&mut bad, (3 << 3) | 2);
        varint(&mut bad, u64::MAX);
        assert!(matches!(decode(&bad), Err(Error::Invalid(_))));
        // A varint that never ends.
        assert!(matches!(decode(&[0xff; 12]), Err(Error::Invalid(_))));
        // Deprecated group wire types.
        assert!(matches!(decode(&[(3 << 3) | 3]), Err(Error::Invalid(_))));
        // Random-ish bytes.
        let mut seed = 0x9e3779b97f4a7c15u64;
        for _ in 0..200 {
            let mut buf = Vec::new();
            for _ in 0..32 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                buf.push(seed as u8);
            }
            let _ = decode(&buf);
        }
    }

    #[test]
    fn zigzag_matches_protobuf() {
        assert_eq!(zigzag32(0), 0);
        assert_eq!(zigzag32(1), -1);
        assert_eq!(zigzag32(2), 1);
        assert_eq!(zigzag32(3), -2);
        assert_eq!(zigzag32(u32::MAX), i32::MIN);
        assert_eq!(zigzag64(4294967294), 2147483647);
        assert_eq!(zigzag64(4294967295), -2147483648);
    }

    #[test]
    fn encode_round_trips_through_decode() {
        let mut props = BTreeMap::new();
        props.insert("name".to_string(), Value::String("Port".into()));
        props.insert("rank".to_string(), Value::Number(2.0));
        props.insert("neg".to_string(), Value::Number(-7.0));
        props.insert("height".to_string(), Value::Number(12.5));
        props.insert("capital".to_string(), Value::Bool(true));
        props.insert("skipped".to_string(), Value::Null);
        let polygon = Feature {
            id: Some(Value::Number(9.0)),
            kind: GeometryType::Polygon,
            properties: props.clone(),
            geometry: vec![
                vec![
                    Point::new(0.0, 0.0),
                    Point::new(100.0, 0.0),
                    Point::new(100.0, 100.0),
                    Point::new(0.0, 100.0),
                    Point::new(0.0, 0.0),
                ],
                vec![
                    Point::new(20.0, 20.0),
                    Point::new(20.0, 40.0),
                    Point::new(40.0, 40.0),
                    Point::new(20.0, 20.0),
                ],
            ],
        };
        let line = Feature {
            id: None,
            kind: GeometryType::LineString,
            properties: BTreeMap::new(),
            geometry: vec![vec![Point::new(-5.0, 3.0), Point::new(50.4, 60.6)]],
        };
        let points = Feature {
            id: None,
            kind: GeometryType::Point,
            properties: BTreeMap::new(),
            geometry: vec![vec![Point::new(1.0, 2.0)], vec![Point::new(3.0, 4.0)]],
        };
        let tile = Tile {
            layers: vec![Layer {
                name: "mixed".into(),
                version: 2,
                extent: 4096,
                features: vec![polygon, line, points],
            }],
        };
        let decoded = decode(&encode(&tile)).unwrap();
        let layer = &decoded.layers[0];
        assert_eq!(layer.name, "mixed");
        assert_eq!(layer.extent, 4096);
        let p = &layer.features[0];
        assert_eq!(p.id, Some(Value::Number(9.0)));
        props.remove("skipped");
        assert_eq!(p.properties, props);
        assert_eq!(p.geometry.len(), 2);
        assert_eq!(p.geometry[0].first(), p.geometry[0].last());
        assert_eq!(p.geometry[0].len(), 5);
        let l = &layer.features[1];
        assert_eq!(
            l.geometry,
            vec![vec![Point::new(-5.0, 3.0), Point::new(50.0, 61.0)]]
        );
        assert_eq!(layer.features[2].geometry.len(), 2);
    }
}
