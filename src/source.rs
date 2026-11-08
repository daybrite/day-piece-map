// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

//! Where tiles come from: a style source resolved to something that answers a [`TileId`] with
//! tile bytes. Two kinds, behind one trait: a z/x/y URL template (given inline or through a
//! TileJSON document, which is what every hosted tile service speaks), and a PMTiles archive
//! (<https://github.com/protomaps/PMTiles,> one static file read with HTTP range requests, which
//! is what self-hosting on object storage speaks). Both go through `day-part-http`, so the
//! platform's own network stack carries every byte and the tests intercept with its
//! `Simulation`.
//!
//! [`TemplateSource`] follows the TileJSON 3.0 spec
//! (<https://github.com/mapbox/tilejson-spec/tree/master/3.0.0>) the way MapLibre reads it: the
//! style's inline `tiles`, zoom range, bounds, scheme and attribution win over the document's.
//! [`PmtilesSource`] follows the PMTiles v3 spec
//! (<https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md>): a 127-byte header, a root
//! directory within the first 16 KiB, optional leaf directories, and tile data, every section
//! fetched with a `Range` request and the directories cached once read.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use day_part_http::{Client, Request, Response};

use crate::Error;

/// A tile address: zoom, column, row (XYZ scheme, row 0 at the north).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileId {
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

impl TileId {
    pub const fn new(z: u8, x: u32, y: u32) -> TileId {
        TileId { z, x, y }
    }
    /// The tile one zoom level up that contains this one.
    pub fn parent(self) -> Option<TileId> {
        (self.z > 0).then(|| TileId::new(self.z - 1, self.x / 2, self.y / 2))
    }
    /// The four tiles one zoom level down that this one covers.
    pub fn children(self) -> [TileId; 4] {
        let (z, x, y) = (self.z + 1, self.x * 2, self.y * 2);
        [
            TileId::new(z, x, y),
            TileId::new(z, x + 1, y),
            TileId::new(z, x, y + 1),
            TileId::new(z, x + 1, y + 1),
        ]
    }
    /// The row in the TMS scheme (row 0 at the south), which is what a `{-y}` placeholder or
    /// a `"scheme": "tms"` source wants.
    pub fn flipped_y(self) -> u32 {
        let rows = 1u32.checked_shl(u32::from(self.z)).unwrap_or(0);
        rows.wrapping_sub(1).wrapping_sub(self.y)
    }
    /// The tile's PMTiles id: its position on the Hilbert curve of its zoom level, after
    /// every tile of the levels above (the spec's TileID).
    pub fn pmtiles_id(self) -> u64 {
        let z = u32::from(self.z.min(31));
        let before = ((1u64 << (2 * z)) - 1) / 3;
        let (mut x, mut y) = (u64::from(self.x), u64::from(self.y));
        let mut d = 0u64;
        let mut s = (1u64 << z) >> 1;
        while s > 0 {
            let rx = u64::from(x & s != 0);
            let ry = u64::from(y & s != 0);
            d += s * s * ((3 * rx) ^ ry);
            if ry == 0 {
                if rx == 1 {
                    x = s.wrapping_sub(1).wrapping_sub(x);
                    y = s.wrapping_sub(1).wrapping_sub(y);
                }
                std::mem::swap(&mut x, &mut y);
            }
            s >>= 1;
        }
        before + d
    }
}

/// A style's `sources` entry, as [`crate::style`] reads it from the style document.
#[derive(Clone, Debug, PartialEq)]
pub enum SourceSpec {
    /// `"type": "vector"`: tiles by URL template, from `tiles` or from the TileJSON at `url`.
    /// A `url` of the form `pmtiles://<https://…>` names a PMTiles archive instead.
    Vector {
        url: Option<String>,
        tiles: Vec<String>,
        minzoom: Option<u8>,
        maxzoom: Option<u8>,
        /// `[west, south, east, north]` in degrees.
        bounds: Option<[f64; 4]>,
        /// `"xyz"` (the default) or `"tms"` (rows counted from the south).
        scheme: Option<String>,
        attribution: Option<String>,
    },
    /// Any other `type` (raster, raster-dem, geojson, image, video): named, not served.
    Unsupported(String),
}

/// A future on the UI thread (the HTTP part's futures are not `Send` on the web).
pub type LocalFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Something that serves tiles.
pub trait TileSource {
    /// The zoom range the source has tiles for; a request outside it answers `Ok(None)`.
    fn min_zoom(&self) -> u8;
    fn max_zoom(&self) -> u8;
    /// `[west, south, east, north]` in degrees, where the source says.
    fn bounds(&self) -> Option<[f64; 4]>;
    /// The credit line the source asks to be shown, where it says.
    fn attribution(&self) -> Option<String>;
    /// The tile's protobuf bytes, inflated; `Ok(None)` where the source has no such tile
    /// (a 204 or 404, or an archive without it).
    fn fetch(&self, id: TileId) -> LocalFuture<'_, Result<Option<Vec<u8>>, Error>>;
}

/// The zoom range a vector source has when nothing says: vector tiles stop at 14 by
/// convention and overzoom from there.
const DEFAULT_MIN_ZOOM: u8 = 0;
const DEFAULT_MAX_ZOOM: u8 = 14;

/// Resolve a source spec into a tile source: read the TileJSON or the PMTiles header, and
/// answer with something that serves tiles.
pub fn open(
    client: Client,
    spec: &SourceSpec,
) -> LocalFuture<'static, Result<Rc<dyn TileSource>, Error>> {
    let spec = spec.clone();
    Box::pin(async move {
        match spec {
            SourceSpec::Vector {
                ref url, ref tiles, ..
            } => {
                let archive = url
                    .as_deref()
                    .filter(|_| tiles.is_empty())
                    .and_then(pmtiles_url);
                match archive {
                    Some(archive) => {
                        let source = PmtilesSource::open(client, archive).await?;
                        Ok(Rc::new(source) as Rc<dyn TileSource>)
                    }
                    None => {
                        let source = TemplateSource::open(client, &spec).await?;
                        Ok(Rc::new(source) as Rc<dyn TileSource>)
                    }
                }
            }
            SourceSpec::Unsupported(kind) => Err(Error::Unsupported(format!("{kind} sources"))),
        }
    })
}

/// The archive URL a source `url` names, if it names one: `pmtiles://<http url>`, or a bare
/// URL whose path ends in `.pmtiles`.
fn pmtiles_url(url: &str) -> Option<String> {
    if let Some(rest) = url.strip_prefix("pmtiles://") {
        return Some(rest.to_string());
    }
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.ends_with(".pmtiles").then(|| url.to_string())
}

fn http_error(e: day_part_http::HttpError) -> Error {
    Error::Http(e.to_string())
}

async fn get(client: &Client, request: Request) -> Result<Response, Error> {
    client.fetch_future(request).await.map_err(http_error)
}

// ---------------------------------------------------------------------------
// gzip
// ---------------------------------------------------------------------------

/// The two bytes every gzip member starts with.
const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// The most a single inflated tile or directory may grow to (a corrupt or hostile length is
/// an error, not an allocation).
const INFLATE_LIMIT: usize = 64 << 20;

/// Whether `bytes` are a gzip member.
pub fn is_gzip(bytes: &[u8]) -> bool {
    bytes.starts_with(&GZIP_MAGIC)
}

/// Inflate a gzip member (RFC 1952): skip the header and its optional fields, inflate the
/// raw deflate stream, drop the trailer. `miniz_oxide` has no gzip wrapper of its own.
pub fn gunzip(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let bad = |what: &str| Error::Invalid(format!("gzip: {what}"));
    if !is_gzip(bytes) || bytes.len() < 18 {
        return Err(bad("not a gzip member"));
    }
    if bytes[2] != 8 {
        return Err(bad("compression method is not deflate"));
    }
    let flags = bytes[3];
    let mut pos = 10;
    if flags & 0x04 != 0 {
        // FEXTRA: a little-endian length, then that many bytes.
        let len = bytes
            .get(pos..pos + 2)
            .map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])))
            .ok_or_else(|| bad("truncated extra field"))?;
        pos += 2 + len;
    }
    for bit in [0x08, 0x10] {
        // FNAME, FCOMMENT: zero-terminated strings.
        if flags & bit != 0 {
            let rest = bytes.get(pos..).ok_or_else(|| bad("truncated header"))?;
            let end = rest
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| bad("unterminated header string"))?;
            pos += end + 1;
        }
    }
    if flags & 0x02 != 0 {
        // FHCRC: a two-byte header checksum.
        pos += 2;
    }
    // The trailer: CRC-32 and the inflated size modulo 2^32, eight bytes.
    let end = bytes.len() - 8;
    let deflate = bytes.get(pos..end).ok_or_else(|| bad("truncated header"))?;
    miniz_oxide::inflate::decompress_to_vec_with_limit(deflate, INFLATE_LIMIT)
        .map_err(|e| bad(&format!("inflate failed: {e}")))
}

/// Inflate `bytes` if they are gzip, else return them as they are: a server that sends gzip
/// without `Content-Encoding`, or an archive whose compression is `Unknown`.
pub fn gunzip_if_gzip(bytes: Vec<u8>) -> Result<Vec<u8>, Error> {
    if is_gzip(&bytes) {
        gunzip(&bytes)
    } else {
        Ok(bytes)
    }
}

// ---------------------------------------------------------------------------
// URL templates
// ---------------------------------------------------------------------------

/// Tiles by URL template: `{z}/{x}/{y}` substituted, several templates rotated by tile.
pub struct TemplateSource {
    client: Client,
    templates: Vec<String>,
    min_zoom: u8,
    max_zoom: u8,
    bounds: Option<[f64; 4]>,
    attribution: Option<String>,
    /// Rows counted from the south (`"scheme": "tms"`).
    tms: bool,
}

/// The fields a TileJSON document and a style's inline source share.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TileJson {
    pub tiles: Vec<String>,
    pub minzoom: Option<u8>,
    pub maxzoom: Option<u8>,
    pub bounds: Option<[f64; 4]>,
    pub scheme: Option<String>,
    pub attribution: Option<String>,
}

impl TileJson {
    /// Read a TileJSON 3.0 document; `tiles` is required, the rest optional.
    pub fn parse(text: &str) -> Result<TileJson, Error> {
        let bad = |what: &str| Error::Invalid(format!("TileJSON: {what}"));
        let json: serde_json::Value =
            serde_json::from_str(text).map_err(|e| bad(&format!("not JSON: {e}")))?;
        let object = json.as_object().ok_or_else(|| bad("not an object"))?;
        let tiles = object
            .get("tiles")
            .and_then(|t| t.as_array())
            .ok_or_else(|| bad("no tiles array"))?
            .iter()
            .map(|t| {
                t.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| bad("tile template not a string"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if tiles.is_empty() {
            return Err(bad("tiles array is empty"));
        }
        let zoom = |key: &str| -> Option<u8> {
            object
                .get(key)
                .and_then(|z| z.as_f64())
                .map(|z| z.clamp(0.0, 30.0) as u8)
        };
        let bounds = object
            .get("bounds")
            .and_then(|b| b.as_array())
            .and_then(|b| {
                let n: Vec<f64> = b.iter().filter_map(|v| v.as_f64()).collect();
                (n.len() == 4).then(|| [n[0], n[1], n[2], n[3]])
            });
        let string = |key: &str| object.get(key).and_then(|s| s.as_str()).map(str::to_string);
        Ok(TileJson {
            tiles,
            minzoom: zoom("minzoom"),
            maxzoom: zoom("maxzoom"),
            bounds,
            scheme: string("scheme"),
            attribution: string("attribution"),
        })
    }

    /// `over` wins field by field where it says something (MapLibre's `extend(tileJSON,
    /// options)`: the style's inline values override the document's).
    fn overridden_by(mut self, over: &TileJson) -> TileJson {
        if !over.tiles.is_empty() {
            self.tiles = over.tiles.clone();
        }
        self.minzoom = over.minzoom.or(self.minzoom);
        self.maxzoom = over.maxzoom.or(self.maxzoom);
        self.bounds = over.bounds.or(self.bounds);
        self.scheme = over.scheme.clone().or(self.scheme);
        self.attribution = over.attribution.clone().or(self.attribution);
        self
    }
}

impl TemplateSource {
    /// A source from the resolved fields; `tiles` must be non-empty.
    pub fn new(client: Client, json: TileJson) -> Result<TemplateSource, Error> {
        if json.tiles.is_empty() {
            return Err(Error::Invalid("vector source: no tile templates".into()));
        }
        Ok(TemplateSource {
            client,
            templates: json.tiles,
            min_zoom: json.minzoom.unwrap_or(DEFAULT_MIN_ZOOM),
            max_zoom: json.maxzoom.unwrap_or(DEFAULT_MAX_ZOOM),
            bounds: json.bounds,
            attribution: json.attribution,
            tms: json.scheme.as_deref() == Some("tms"),
        })
    }

    /// A source from a style's vector spec: its inline `tiles`, or the TileJSON at its `url`
    /// fetched through `client`, with the inline fields overriding the document's.
    pub fn open(
        client: Client,
        spec: &SourceSpec,
    ) -> LocalFuture<'static, Result<TemplateSource, Error>> {
        let spec = spec.clone();
        Box::pin(async move {
            let SourceSpec::Vector {
                url,
                tiles,
                minzoom,
                maxzoom,
                bounds,
                scheme,
                attribution,
            } = spec
            else {
                return Err(Error::Unsupported("source is not a vector source".into()));
            };
            let inline = TileJson {
                tiles,
                minzoom,
                maxzoom,
                bounds,
                scheme,
                attribution,
            };
            let resolved = if inline.tiles.is_empty() {
                let url = url
                    .ok_or_else(|| Error::Invalid("vector source: neither tiles nor url".into()))?;
                Self::fetch_tilejson(&client, &url)
                    .await?
                    .overridden_by(&inline)
            } else {
                inline
            };
            TemplateSource::new(client, resolved)
        })
    }

    /// The TileJSON document at `url`.
    pub async fn fetch_tilejson(client: &Client, url: &str) -> Result<TileJson, Error> {
        let response = get(client, Request::get(url)).await?;
        if response.status != 200 {
            return Err(Error::Status {
                url: url.to_string(),
                status: response.status,
            });
        }
        TileJson::parse(&response.text())
    }

    /// The URL tile `id` is fetched from: the template for `(x + y) % n` with `{z}`, `{x}`,
    /// `{y}` and `{-y}` substituted (and `{y}` flipped under the TMS scheme).
    pub fn url_for(&self, id: TileId) -> String {
        let n = self.templates.len() as u64;
        let which = ((u64::from(id.x) + u64::from(id.y)) % n.max(1)) as usize;
        let template = self.templates.get(which).map(String::as_str).unwrap_or("");
        let y = if self.tms { id.flipped_y() } else { id.y };
        template
            .replace("{z}", &id.z.to_string())
            .replace("{x}", &id.x.to_string())
            .replace("{-y}", &id.flipped_y().to_string())
            .replace("{y}", &y.to_string())
    }
}

impl TileSource for TemplateSource {
    fn min_zoom(&self) -> u8 {
        self.min_zoom
    }
    fn max_zoom(&self) -> u8 {
        self.max_zoom
    }
    fn bounds(&self) -> Option<[f64; 4]> {
        self.bounds
    }
    fn attribution(&self) -> Option<String> {
        self.attribution.clone()
    }
    fn fetch(&self, id: TileId) -> LocalFuture<'_, Result<Option<Vec<u8>>, Error>> {
        Box::pin(async move {
            if id.z < self.min_zoom || id.z > self.max_zoom {
                return Ok(None);
            }
            let url = self.url_for(id);
            let response = get(&self.client, Request::get(&url)).await?;
            match response.status {
                200 => gunzip_if_gzip(response.body).map(Some),
                204 | 404 => Ok(None),
                status => Err(Error::Status { url, status }),
            }
        })
    }
}

// ---------------------------------------------------------------------------
// PMTiles
// ---------------------------------------------------------------------------

/// How a PMTiles archive compresses its directories or its tiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    Unknown,
    None,
    Gzip,
    Brotli,
    Zstd,
}

impl Compression {
    fn from_byte(b: u8) -> Result<Compression, Error> {
        Ok(match b {
            0 => Compression::Unknown,
            1 => Compression::None,
            2 => Compression::Gzip,
            3 => Compression::Brotli,
            4 => Compression::Zstd,
            other => return Err(Error::Invalid(format!("PMTiles: compression {other}"))),
        })
    }

    /// Undo this compression. `Unknown` sniffs for gzip; brotli and zstd are not implemented.
    pub fn decompress(self, bytes: Vec<u8>) -> Result<Vec<u8>, Error> {
        match self {
            Compression::None => Ok(bytes),
            Compression::Gzip => gunzip(&bytes),
            Compression::Unknown => gunzip_if_gzip(bytes),
            Compression::Brotli => Err(Error::Unsupported("PMTiles brotli compression".into())),
            Compression::Zstd => Err(Error::Unsupported("PMTiles zstd compression".into())),
        }
    }
}

/// The 127-byte PMTiles v3 header.
#[derive(Clone, Debug, PartialEq)]
pub struct Header {
    pub root_offset: u64,
    pub root_length: u64,
    pub metadata_offset: u64,
    pub metadata_length: u64,
    pub leaf_offset: u64,
    pub leaf_length: u64,
    pub tile_data_offset: u64,
    pub tile_data_length: u64,
    pub addressed_tiles: u64,
    pub tile_entries: u64,
    pub tile_contents: u64,
    pub clustered: bool,
    pub internal_compression: Compression,
    pub tile_compression: Compression,
    /// 0 unknown, 1 MVT, 2 PNG, 3 JPEG, 4 WebP, 5 AVIF, 6 MapLibre Vector Tile.
    pub tile_type: u8,
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// `[west, south, east, north]` in degrees.
    pub bounds: [f64; 4],
    pub center_zoom: u8,
    /// `(longitude, latitude)` in degrees.
    pub center: (f64, f64),
}

/// The header's length, and the prefix a reader fetches first (the spec promises the root
/// directory fits in it).
pub const HEADER_LEN: usize = 127;
const PREFIX_LEN: u64 = 16384;

impl Header {
    /// Read the header from the archive's first bytes.
    pub fn parse(bytes: &[u8]) -> Result<Header, Error> {
        let bad = |what: &str| Error::Invalid(format!("PMTiles: {what}"));
        if bytes.len() < HEADER_LEN {
            return Err(bad("header shorter than 127 bytes"));
        }
        if &bytes[0..7] != b"PMTiles" {
            return Err(bad("bad magic"));
        }
        if bytes[7] != 3 {
            return Err(Error::Unsupported(format!("PMTiles version {}", bytes[7])));
        }
        let u64_at = |at: usize| {
            let b = &bytes[at..at + 8];
            u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
        };
        let degrees_at = |at: usize| {
            let b = &bytes[at..at + 4];
            f64::from(i32::from_le_bytes([b[0], b[1], b[2], b[3]])) / 10_000_000.0
        };
        Ok(Header {
            root_offset: u64_at(8),
            root_length: u64_at(16),
            metadata_offset: u64_at(24),
            metadata_length: u64_at(32),
            leaf_offset: u64_at(40),
            leaf_length: u64_at(48),
            tile_data_offset: u64_at(56),
            tile_data_length: u64_at(64),
            addressed_tiles: u64_at(72),
            tile_entries: u64_at(80),
            tile_contents: u64_at(88),
            clustered: bytes[96] == 1,
            internal_compression: Compression::from_byte(bytes[97])?,
            tile_compression: Compression::from_byte(bytes[98])?,
            tile_type: bytes[99],
            min_zoom: bytes[100],
            max_zoom: bytes[101],
            bounds: [
                degrees_at(102),
                degrees_at(106),
                degrees_at(110),
                degrees_at(114),
            ],
            center_zoom: bytes[118],
            center: (degrees_at(119), degrees_at(123)),
        })
    }
}

/// One directory entry: a tile (or a run of tiles sharing one blob), or, with `run_length`
/// 0, a leaf directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub tile_id: u64,
    /// Relative to the tile data section (a tile) or the leaf directories section (a leaf).
    pub offset: u64,
    pub length: u32,
    pub run_length: u32,
}

/// A decoded directory: entries ascending by tile id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Directory {
    pub entries: Vec<Entry>,
}

impl Directory {
    /// Decode a directory's (already decompressed) bytes: the entry count, then the tile ids
    /// as deltas, the run lengths, the lengths, and the offsets with `0` meaning "right after
    /// the previous entry".
    pub fn parse(bytes: &[u8]) -> Result<Directory, Error> {
        let bad = |what: &str| Error::Invalid(format!("PMTiles directory: {what}"));
        let mut pos = 0usize;
        let mut varint = || -> Result<u64, Error> {
            let mut value = 0u64;
            for shift in (0..64).step_by(7) {
                let byte = *bytes.get(pos).ok_or_else(|| bad("truncated"))?;
                pos += 1;
                value |= u64::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    return Ok(value);
                }
            }
            Err(bad("varint too long"))
        };
        let n = varint()?;
        // Four varints per entry at least one byte each: a count the bytes cannot hold is a
        // lie, not something to allocate for.
        if n == 0 || n > (bytes.len() as u64) / 4 {
            return Err(bad("entry count does not fit the directory"));
        }
        let n = n as usize;
        let mut entries = Vec::with_capacity(n);
        let mut last = 0u64;
        for _ in 0..n {
            last = last
                .checked_add(varint()?)
                .ok_or_else(|| bad("tile id overflow"))?;
            entries.push(Entry {
                tile_id: last,
                offset: 0,
                length: 0,
                run_length: 0,
            });
        }
        for e in entries.iter_mut() {
            e.run_length = u32::try_from(varint()?).map_err(|_| bad("run length too large"))?;
        }
        for e in entries.iter_mut() {
            e.length = u32::try_from(varint()?).map_err(|_| bad("length too large"))?;
            if e.length == 0 {
                return Err(bad("entry length 0"));
            }
        }
        let mut next = 0u64;
        for (i, e) in entries.iter_mut().enumerate() {
            let value = varint()?;
            e.offset = if value == 0 && i > 0 {
                next
            } else {
                value
                    .checked_sub(1)
                    .ok_or_else(|| bad("offset 0 for the first entry"))?
            };
            next = e.offset + u64::from(e.length);
        }
        Ok(Directory { entries })
    }

    /// The entry covering `tile_id`: the tile's own, the run it falls in, or the leaf
    /// directory that would hold it. `None` where the directory has nothing for it.
    pub fn find(&self, tile_id: u64) -> Option<Entry> {
        let after = self.entries.partition_point(|e| e.tile_id <= tile_id);
        let entry = *self.entries.get(after.checked_sub(1)?)?;
        if entry.run_length == 0 || tile_id - entry.tile_id < u64::from(entry.run_length) {
            Some(entry)
        } else {
            None
        }
    }
}

/// A PMTiles v3 archive over HTTP range requests.
pub struct PmtilesSource {
    client: Client,
    url: String,
    header: Header,
    root: Directory,
    metadata: serde_json::Value,
    /// Leaf directories by their offset in the leaf section, kept once read.
    leaves: RefCell<BTreeMap<u64, Rc<Directory>>>,
}

/// How deep a chain of leaf directories a reader follows (the spec discourages more than one
/// level; a cycle must not spin).
const MAX_LEAF_DEPTH: usize = 4;

impl PmtilesSource {
    /// Open the archive at `url`: fetch its first 16 KiB for the header, the root directory
    /// and (where it fits there) the metadata.
    pub fn open(client: Client, url: String) -> LocalFuture<'static, Result<PmtilesSource, Error>> {
        Box::pin(async move {
            let prefix = read_range(&client, &url, 0, PREFIX_LEN).await?;
            let header = Header::parse(&prefix)?;
            if !matches!(header.tile_type, 0 | 1) {
                return Err(Error::Unsupported(format!(
                    "PMTiles tile type {} (only MVT archives draw)",
                    header.tile_type
                )));
            }
            let root_bytes = section(
                &client,
                &url,
                &prefix,
                header.root_offset,
                header.root_length,
            )
            .await?;
            let root = Directory::parse(&header.internal_compression.decompress(root_bytes)?)?;
            let metadata = if header.metadata_length > 0 {
                let bytes = section(
                    &client,
                    &url,
                    &prefix,
                    header.metadata_offset,
                    header.metadata_length,
                )
                .await?;
                let bytes = header.internal_compression.decompress(bytes)?;
                serde_json::from_slice(&bytes)
                    .map_err(|e| Error::Invalid(format!("PMTiles metadata: {e}")))?
            } else {
                serde_json::Value::Null
            };
            Ok(PmtilesSource {
                client,
                url,
                header,
                root,
                metadata,
                leaves: RefCell::new(BTreeMap::new()),
            })
        })
    }

    /// The archive's header.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The archive's JSON metadata (`vector_layers`, `name`, `attribution`, …), or `Null`
    /// where it has none.
    pub fn metadata(&self) -> &serde_json::Value {
        &self.metadata
    }

    /// The root directory.
    pub fn root(&self) -> &Directory {
        &self.root
    }

    /// The leaf directory at `offset` in the leaf section, from the cache or the network.
    async fn leaf(&self, offset: u64, length: u32) -> Result<Rc<Directory>, Error> {
        if let Some(leaf) = self.leaves.borrow().get(&offset) {
            return Ok(leaf.clone());
        }
        let bytes = read_range(
            &self.client,
            &self.url,
            self.header.leaf_offset.saturating_add(offset),
            u64::from(length),
        )
        .await?;
        let leaf = Rc::new(Directory::parse(
            &self.header.internal_compression.decompress(bytes)?,
        )?);
        self.leaves.borrow_mut().insert(offset, leaf.clone());
        Ok(leaf)
    }

    /// The directory entry for `tile_id`, following leaf pointers.
    async fn entry(&self, tile_id: u64) -> Result<Option<Entry>, Error> {
        let mut entry = self.root.find(tile_id);
        for _ in 0..MAX_LEAF_DEPTH {
            match entry {
                Some(e) if e.run_length == 0 => {
                    entry = self.leaf(e.offset, e.length).await?.find(tile_id);
                }
                other => return Ok(other),
            }
        }
        Err(Error::Invalid(
            "PMTiles: leaf directories nested too deep".into(),
        ))
    }
}

impl TileSource for PmtilesSource {
    fn min_zoom(&self) -> u8 {
        self.header.min_zoom
    }
    fn max_zoom(&self) -> u8 {
        self.header.max_zoom
    }
    fn bounds(&self) -> Option<[f64; 4]> {
        Some(self.header.bounds)
    }
    fn attribution(&self) -> Option<String> {
        self.metadata
            .get("attribution")
            .and_then(|a| a.as_str())
            .map(str::to_string)
    }
    fn fetch(&self, id: TileId) -> LocalFuture<'_, Result<Option<Vec<u8>>, Error>> {
        Box::pin(async move {
            if id.z < self.header.min_zoom || id.z > self.header.max_zoom {
                return Ok(None);
            }
            let Some(entry) = self.entry(id.pmtiles_id()).await? else {
                return Ok(None);
            };
            let bytes = read_range(
                &self.client,
                &self.url,
                self.header.tile_data_offset.saturating_add(entry.offset),
                u64::from(entry.length),
            )
            .await?;
            self.header.tile_compression.decompress(bytes).map(Some)
        })
    }
}

/// A section of the archive: sliced from the prefix already read where it lies within it,
/// else fetched with its own range request.
async fn section(
    client: &Client,
    url: &str,
    prefix: &[u8],
    offset: u64,
    length: u64,
) -> Result<Vec<u8>, Error> {
    let end = offset.saturating_add(length);
    let within = usize::try_from(offset).ok().zip(usize::try_from(end).ok());
    match within.and_then(|(a, b)| prefix.get(a..b)) {
        Some(bytes) => Ok(bytes.to_vec()),
        None => read_range(client, url, offset, length).await,
    }
}

/// GET `length` bytes of `url` from `offset` with a `Range` header. A 206 answers the range
/// (trimmed if the server sent more); a 200 is the whole resource, sliced here; a short
/// answer is invalid; anything else is a status error.
async fn read_range(
    client: &Client,
    url: &str,
    offset: u64,
    length: u64,
) -> Result<Vec<u8>, Error> {
    if length == 0 {
        return Ok(Vec::new());
    }
    let end = offset.saturating_add(length).saturating_sub(1);
    let request = Request::get(url).header("Range", &format!("bytes={offset}-{end}"));
    let response = get(client, request).await?;
    let short = || Error::Invalid(format!("PMTiles: {url} answered fewer bytes than asked"));
    let want = usize::try_from(length).map_err(|_| short())?;
    match response.status {
        206 => {
            let mut body = response.body;
            if body.len() < want && offset + length <= PREFIX_LEN {
                // The first read asks for 16 KiB; a smaller archive legitimately ends early.
                return Ok(body);
            }
            if body.len() < want {
                return Err(short());
            }
            body.truncate(want);
            Ok(body)
        }
        200 => {
            let start = usize::try_from(offset).map_err(|_| short())?;
            let body = &response.body;
            let end = start.saturating_add(want).min(body.len());
            body.get(start..end).map(<[u8]>::to_vec).ok_or_else(short)
        }
        status => Err(Error::Status {
            url: url.to_string(),
            status,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use day_part_http::simulation::{Reply, Simulation};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};

    /// Run a future to completion on this thread, parking between polls: the simulation's
    /// completions arrive from its timer thread and unpark us.
    fn block_on<F: Future>(future: F) -> F::Output {
        struct Unpark(std::thread::Thread);
        impl std::task::Wake for Unpark {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return value;
            }
            std::thread::park();
        }
    }

    fn client(sim: &Simulation) -> Client {
        Client::builder().transport(sim.clone()).build()
    }

    fn vector(url: Option<&str>, tiles: &[&str]) -> SourceSpec {
        SourceSpec::Vector {
            url: url.map(str::to_string),
            tiles: tiles.iter().map(|t| t.to_string()).collect(),
            minzoom: None,
            maxzoom: None,
            bounds: None,
            scheme: None,
            attribution: None,
        }
    }

    // -- gzip, by hand: header, raw deflate, CRC-32 and size trailer. ----------------------

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
        out.extend(miniz_oxide::deflate::compress_to_vec(data, 6));
        out.extend_from_slice(&crc32(data).to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out
    }

    #[test]
    fn gunzip_round_trips_and_handles_header_fields() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(gunzip(&gzip(&data)).unwrap(), data);
        assert_eq!(gunzip_if_gzip(data.clone()).unwrap(), data);
        // FEXTRA + FNAME + FCOMMENT + FHCRC, all set.
        let mut fancy = vec![0x1f, 0x8b, 8, 0x1e, 0, 0, 0, 0, 0, 0xff];
        fancy.extend_from_slice(&[3, 0, b'a', b'b', b'c']);
        fancy.extend_from_slice(b"name\0");
        fancy.extend_from_slice(b"comment\0");
        fancy.extend_from_slice(&[0x12, 0x34]);
        fancy.extend(miniz_oxide::deflate::compress_to_vec(&data, 1));
        fancy.extend_from_slice(&crc32(&data).to_le_bytes());
        fancy.extend_from_slice(&(data.len() as u32).to_le_bytes());
        assert_eq!(gunzip(&fancy).unwrap(), data);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        // Truncated and corrupt members.
        let whole = gzip(&data);
        assert!(matches!(gunzip(&whole[..5]), Err(Error::Invalid(_))));
        assert!(matches!(
            gunzip(&whole[..whole.len() / 2]),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(gunzip(b"plain"), Err(Error::Invalid(_))));
        let mut unterminated = vec![0x1f, 0x8b, 8, 0x08, 0, 0, 0, 0, 0, 0xff];
        unterminated.extend_from_slice(b"no terminator at all....");
        assert!(matches!(gunzip(&unterminated), Err(Error::Invalid(_))));
    }

    // -- Tile ids ----------------------------------------------------------------------------

    #[test]
    fn pmtiles_ids_follow_the_spec_table() {
        assert_eq!(TileId::new(0, 0, 0).pmtiles_id(), 0);
        assert_eq!(TileId::new(1, 0, 0).pmtiles_id(), 1);
        assert_eq!(TileId::new(1, 0, 1).pmtiles_id(), 2);
        assert_eq!(TileId::new(1, 1, 1).pmtiles_id(), 3);
        assert_eq!(TileId::new(1, 1, 0).pmtiles_id(), 4);
        assert_eq!(TileId::new(2, 0, 0).pmtiles_id(), 5);
        assert_eq!(TileId::new(12, 3423, 1763).pmtiles_id(), 19078479);
        // Every tile of a level has a distinct id in the level's range.
        let mut ids: Vec<u64> = (0..16)
            .flat_map(|x| (0..16).map(move |y| TileId::new(4, x, y).pmtiles_id()))
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, (85..85 + 256).collect::<Vec<_>>());
    }

    #[test]
    fn flipped_rows_and_parents() {
        assert_eq!(TileId::new(3, 1, 0).flipped_y(), 7);
        assert_eq!(TileId::new(0, 0, 0).flipped_y(), 0);
        assert_eq!(TileId::new(3, 5, 6).parent(), Some(TileId::new(2, 2, 3)));
        assert_eq!(TileId::new(0, 0, 0).parent(), None);
    }

    // -- URL templates -----------------------------------------------------------------------

    #[test]
    fn tilejson_then_tile_fetch_substitutes_and_records() {
        let sim = Simulation::new();
        sim.route("GET", "https://tiles.example/v3.json", |_| {
            Ok(Reply::new(
                200,
                r#"{"tilejson":"3.0.0","tiles":["https://a.tiles.example/{z}/{x}/{y}.pbf","https://b.tiles.example/{z}/{x}/{y}.pbf"],"minzoom":2,"maxzoom":12,"bounds":[-10,35,30,70],"attribution":"&copy; Test"}"#,
            ))
        });
        sim.handle(
            |m, u| m == "GET" && u.starts_with("https://a.tiles.example/"),
            |_| Ok(Reply::new(200, b"tile-a".to_vec())),
        );
        sim.handle(
            |m, u| m == "GET" && u.starts_with("https://b.tiles.example/"),
            |_| Ok(Reply::new(200, b"tile-b".to_vec())),
        );
        let source = block_on(open(
            client(&sim),
            &vector(Some("https://tiles.example/v3.json"), &[]),
        ))
        .unwrap();
        assert_eq!((source.min_zoom(), source.max_zoom()), (2, 12));
        assert_eq!(source.bounds(), Some([-10.0, 35.0, 30.0, 70.0]));
        assert_eq!(source.attribution().as_deref(), Some("&copy; Test"));
        assert_eq!(sim.request_count("GET", "https://tiles.example/v3.json"), 1);

        let tile = block_on(source.fetch(TileId::new(5, 16, 10))).unwrap();
        assert_eq!(tile.as_deref(), Some(&b"tile-a"[..]));
        assert_eq!(
            sim.request_count("GET", "https://a.tiles.example/5/16/10.pbf"),
            1
        );
        let tile = block_on(source.fetch(TileId::new(5, 16, 11))).unwrap();
        assert_eq!(tile.as_deref(), Some(&b"tile-b"[..]));
        assert_eq!(
            sim.request_count("GET", "https://b.tiles.example/5/16/11.pbf"),
            1
        );
        // Out of the zoom range: no request at all.
        assert_eq!(block_on(source.fetch(TileId::new(13, 0, 0))).unwrap(), None);
        assert_eq!(block_on(source.fetch(TileId::new(1, 0, 0))).unwrap(), None);
        assert_eq!(sim.requests().len(), 3);
    }

    #[test]
    fn inline_tiles_override_the_document_and_tms_flips_rows() {
        let sim = Simulation::new();
        sim.route("GET", "https://tiles.example/tms.json", |_| {
            Ok(Reply::new(
                200,
                r#"{"tiles":["https://t.example/{z}/{x}/{y}"],"scheme":"tms","maxzoom":8}"#,
            ))
        });
        sim.handle(
            |m, u| m == "GET" && u.starts_with("https://t.example/"),
            |_| Ok(Reply::new(200, b"ok".to_vec())),
        );
        let spec = SourceSpec::Vector {
            url: Some("https://tiles.example/tms.json".into()),
            tiles: vec![],
            minzoom: None,
            maxzoom: Some(10),
            bounds: None,
            scheme: None,
            attribution: Some("inline".into()),
        };
        let source = block_on(open(client(&sim), &spec)).unwrap();
        assert_eq!((source.min_zoom(), source.max_zoom()), (0, 10));
        assert_eq!(source.attribution().as_deref(), Some("inline"));
        assert!(
            block_on(source.fetch(TileId::new(3, 1, 0)))
                .unwrap()
                .is_some()
        );
        assert_eq!(sim.request_count("GET", "https://t.example/3/1/7"), 1);

        // Inline templates: no TileJSON request, defaults for the zoom range, `{-y}` flips.
        let source = block_on(open(
            client(&sim),
            &vector(None, &["https://t.example/{z}/{x}/{-y}.mvt"]),
        ))
        .unwrap();
        assert_eq!((source.min_zoom(), source.max_zoom()), (0, 14));
        assert!(
            block_on(source.fetch(TileId::new(2, 3, 3)))
                .unwrap()
                .is_some()
        );
        assert_eq!(sim.request_count("GET", "https://t.example/2/3/0.mvt"), 1);
        assert_eq!(
            sim.request_count("GET", "https://tiles.example/tms.json"),
            1
        );
    }

    #[test]
    fn statuses_map_to_none_or_errors() {
        let sim = Simulation::new();
        sim.route("GET", "https://t.example/1/0/0", |_| {
            Ok(Reply::new(204, Vec::new()))
        });
        sim.route("GET", "https://t.example/1/1/0", |_| {
            Ok(Reply::new(404, b"nope".to_vec()))
        });
        sim.route("GET", "https://t.example/1/0/1", |_| {
            Ok(Reply::new(500, b"boom".to_vec()))
        });
        sim.route("GET", "https://t.example/1/1/1", |_| {
            Err(day_part_http::HttpError::Connect)
        });
        let source = block_on(open(
            client(&sim),
            &vector(None, &["https://t.example/{z}/{x}/{y}"]),
        ))
        .unwrap();
        assert_eq!(block_on(source.fetch(TileId::new(1, 0, 0))).unwrap(), None);
        assert_eq!(block_on(source.fetch(TileId::new(1, 1, 0))).unwrap(), None);
        assert_eq!(
            block_on(source.fetch(TileId::new(1, 0, 1))),
            Err(Error::Status {
                url: "https://t.example/1/0/1".into(),
                status: 500
            })
        );
        assert!(matches!(
            block_on(source.fetch(TileId::new(1, 1, 1))),
            Err(Error::Http(_))
        ));
    }

    #[test]
    fn a_gzip_body_is_inflated() {
        let sim = Simulation::new();
        let tile: Vec<u8> = (0..3000u32).map(|i| (i * 7 % 13) as u8).collect();
        let zipped = gzip(&tile);
        sim.route("GET", "https://t.example/0/0/0", move |_| {
            Ok(Reply::new(200, zipped.clone()))
        });
        let source = block_on(open(
            client(&sim),
            &vector(None, &["https://t.example/{z}/{x}/{y}"]),
        ))
        .unwrap();
        assert_eq!(
            block_on(source.fetch(TileId::new(0, 0, 0))).unwrap(),
            Some(tile)
        );
    }

    #[test]
    fn open_rejects_what_it_cannot_serve() {
        let sim = Simulation::new();
        sim.route("GET", "https://tiles.example/missing.json", |_| {
            Ok(Reply::new(404, Vec::new()))
        });
        sim.route("GET", "https://tiles.example/broken.json", |_| {
            Ok(Reply::new(200, b"{\"name\":1}".to_vec()))
        });
        assert_eq!(
            block_on(open(
                client(&sim),
                &SourceSpec::Unsupported("raster".into())
            ))
            .err(),
            Some(Error::Unsupported("raster sources".into()))
        );
        assert_eq!(
            block_on(open(
                client(&sim),
                &vector(Some("https://tiles.example/missing.json"), &[])
            ))
            .err(),
            Some(Error::Status {
                url: "https://tiles.example/missing.json".into(),
                status: 404
            })
        );
        assert!(matches!(
            block_on(open(
                client(&sim),
                &vector(Some("https://tiles.example/broken.json"), &[])
            )),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            block_on(open(client(&sim), &vector(None, &[]))),
            Err(Error::Invalid(_))
        ));
    }

    // -- PMTiles: an archive built in memory, served by a handler that honours Range. --------

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

    fn encode_directory(entries: &[Entry]) -> Vec<u8> {
        let mut out = Vec::new();
        varint(&mut out, entries.len() as u64);
        let mut last = 0;
        for e in entries {
            varint(&mut out, e.tile_id - last);
            last = e.tile_id;
        }
        for e in entries {
            varint(&mut out, u64::from(e.run_length));
        }
        for e in entries {
            varint(&mut out, u64::from(e.length));
        }
        let mut next = 0;
        for (i, e) in entries.iter().enumerate() {
            if i > 0 && e.offset == next {
                varint(&mut out, 0);
            } else {
                varint(&mut out, e.offset + 1);
            }
            next = e.offset + u64::from(e.length);
        }
        out
    }

    fn entry(tile_id: u64, offset: u64, length: u32, run_length: u32) -> Entry {
        Entry {
            tile_id,
            offset,
            length,
            run_length,
        }
    }

    fn degrees(d: f64) -> [u8; 4] {
        ((d * 10_000_000.0) as i32).to_le_bytes()
    }

    struct Archive {
        bytes: Vec<u8>,
        tile_a: Vec<u8>,
        tile_b: Vec<u8>,
        leaf_range: (u64, u64),
        tile_a_range: (u64, u64),
        tile_b_range: (u64, u64),
    }

    /// Root: a direct entry for z0 (tile id 0) with a run of three tiles sharing one blob, a
    /// leaf pointer for everything from tile id 5 (zoom 2). The leaf holds z2 tiles 5 and 7.
    /// Directories and tiles are gzip; metadata sits inside the 16 KiB prefix.
    fn archive() -> Archive {
        let tile_a = b"mvt-bytes-for-a".to_vec();
        let tile_b = (0..2000u32).map(|i| (i % 7) as u8).collect::<Vec<u8>>();
        let tile_a_z = gzip(&tile_a);
        let tile_b_z = gzip(&tile_b);
        let mut tile_data = tile_a_z.clone();
        tile_data.extend_from_slice(&tile_b_z);
        let leaf = gzip(&encode_directory(&[
            entry(5, 0, tile_a_z.len() as u32, 1),
            entry(7, tile_a_z.len() as u64, tile_b_z.len() as u32, 1),
        ]));
        let root = gzip(&encode_directory(&[
            entry(0, 0, tile_a_z.len() as u32, 3),
            entry(5, 0, leaf.len() as u32, 0),
        ]));
        let metadata = gzip(br#"{"name":"test","attribution":"(c) Archive","vector_layers":[]}"#);

        let root_offset = HEADER_LEN as u64;
        let metadata_offset = root_offset + root.len() as u64;
        let leaf_offset = metadata_offset + metadata.len() as u64;
        let tile_data_offset = leaf_offset + leaf.len() as u64;

        let mut h = Vec::with_capacity(HEADER_LEN);
        h.extend_from_slice(b"PMTiles");
        h.push(3);
        for v in [
            root_offset,
            root.len() as u64,
            metadata_offset,
            metadata.len() as u64,
            leaf_offset,
            leaf.len() as u64,
            tile_data_offset,
            tile_data.len() as u64,
            5,
            3,
            2,
        ] {
            h.extend_from_slice(&v.to_le_bytes());
        }
        h.push(1); // clustered
        h.push(2); // internal compression: gzip
        h.push(2); // tile compression: gzip
        h.push(1); // MVT
        h.push(0); // min zoom
        h.push(2); // max zoom
        h.extend_from_slice(&degrees(-180.0));
        h.extend_from_slice(&degrees(-85.0));
        h.extend_from_slice(&degrees(180.0));
        h.extend_from_slice(&degrees(85.0));
        h.push(1); // center zoom
        h.extend_from_slice(&degrees(2.5));
        h.extend_from_slice(&degrees(48.5));
        assert_eq!(h.len(), HEADER_LEN);

        let mut bytes = h;
        bytes.extend_from_slice(&root);
        bytes.extend_from_slice(&metadata);
        bytes.extend_from_slice(&leaf);
        bytes.extend_from_slice(&tile_data);
        Archive {
            bytes,
            tile_a,
            tile_b,
            leaf_range: (leaf_offset, leaf_offset + leaf.len() as u64 - 1),
            tile_a_range: (
                tile_data_offset,
                tile_data_offset + tile_a_z.len() as u64 - 1,
            ),
            tile_b_range: (
                tile_data_offset + tile_a_z.len() as u64,
                tile_data_offset + tile_data.len() as u64 - 1,
            ),
        }
    }

    type Ranges = Arc<Mutex<Vec<(u64, u64)>>>;

    /// Serve `bytes` at `url` with `Range` honoured (206 and the slice) and every asked range
    /// recorded; a request without one gets the whole thing as a 200.
    fn serve(sim: &Simulation, url: &str, bytes: Vec<u8>, ranges: &Ranges) {
        let ranges = ranges.clone();
        sim.route("GET", url, move |request| {
            let range = request
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("range"))
                .and_then(|(_, v)| v.strip_prefix("bytes=").map(str::to_string));
            let Some(range) = range else {
                return Ok(Reply::new(200, bytes.clone()));
            };
            let (a, b) = range.split_once('-').expect("a-b range");
            let (a, b): (u64, u64) = (a.parse().expect("start"), b.parse().expect("end"));
            ranges.lock().expect("ranges").push((a, b));
            let start = (a as usize).min(bytes.len());
            let end = ((b + 1) as usize).min(bytes.len());
            Ok(Reply::new(206, bytes[start..end].to_vec()).header(
                "Content-Range",
                format!("bytes {start}-{}/{}", end.saturating_sub(1), bytes.len()),
            ))
        });
    }

    #[test]
    fn pmtiles_archive_is_read_with_ranges_and_directories_are_cached() {
        let sim = Simulation::new();
        let archive = archive();
        let ranges: Ranges = Arc::new(Mutex::new(Vec::new()));
        let url = "https://cdn.example/world.pmtiles";
        serve(&sim, url, archive.bytes.clone(), &ranges);

        let spec = vector(Some("pmtiles://https://cdn.example/world.pmtiles"), &[]);
        let source = block_on(open(client(&sim), &spec)).unwrap();
        assert_eq!(
            sim.request_count("GET", url),
            1,
            "header, root and metadata from one prefix read"
        );
        assert_eq!(ranges.lock().unwrap().as_slice(), &[(0, 16383)]);
        assert_eq!((source.min_zoom(), source.max_zoom()), (0, 2));
        assert_eq!(source.bounds(), Some([-180.0, -85.0, 180.0, 85.0]));
        assert_eq!(source.attribution().as_deref(), Some("(c) Archive"));

        // A z2 tile through the leaf: the leaf read, then the tile read.
        let tile = block_on(source.fetch(TileId::new(2, 0, 0))).unwrap();
        assert_eq!(tile, Some(archive.tile_a.clone()));
        assert_eq!(sim.request_count("GET", url), 3);
        assert_eq!(ranges.lock().unwrap()[1], archive.leaf_range);
        assert_eq!(ranges.lock().unwrap()[2], archive.tile_a_range);

        // Another tile of the same leaf (id 7 = z2 x1 y1): the leaf is cached, one read.
        assert_eq!(TileId::new(2, 1, 1).pmtiles_id(), 7);
        let tile = block_on(source.fetch(TileId::new(2, 1, 1))).unwrap();
        assert_eq!(tile, Some(archive.tile_b.clone()));
        assert_eq!(sim.request_count("GET", url), 4);
        assert_eq!(ranges.lock().unwrap()[3], archive.tile_b_range);

        // A tile the leaf lacks (id 6): the leaf is still cached, no request.
        assert_eq!(TileId::new(2, 1, 0).pmtiles_id(), 6);
        assert_eq!(block_on(source.fetch(TileId::new(2, 1, 0))).unwrap(), None);
        assert_eq!(sim.request_count("GET", url), 4);

        // The run-length entry covers tile ids 0, 1, 2 with tile A's blob; id 3 and 4 miss.
        assert_eq!(
            block_on(source.fetch(TileId::new(0, 0, 0))).unwrap(),
            Some(archive.tile_a.clone())
        );
        assert_eq!(
            block_on(source.fetch(TileId::new(1, 0, 1))).unwrap(),
            Some(archive.tile_a.clone())
        );
        assert_eq!(block_on(source.fetch(TileId::new(1, 1, 1))).unwrap(), None);
        assert_eq!(sim.request_count("GET", url), 6);

        // Out of the zoom range: nothing asked.
        assert_eq!(block_on(source.fetch(TileId::new(3, 0, 0))).unwrap(), None);
        assert_eq!(sim.request_count("GET", url), 6);
    }

    #[test]
    fn pmtiles_whole_body_200_is_sliced_and_bare_urls_open() {
        let sim = Simulation::new();
        let archive = archive();
        let url = "https://cdn.example/world.pmtiles";
        let bytes = archive.bytes.clone();
        sim.route("GET", url, move |_| Ok(Reply::new(200, bytes.clone())));
        let source = block_on(open(client(&sim), &vector(Some(url), &[]))).unwrap();
        assert_eq!(
            block_on(source.fetch(TileId::new(2, 1, 1))).unwrap(),
            Some(archive.tile_b)
        );
    }

    #[test]
    fn pmtiles_refuses_what_it_cannot_read() {
        let sim = Simulation::new();
        let mut brotli = archive().bytes;
        brotli[97] = 3;
        sim.route("GET", "https://cdn.example/brotli.pmtiles", move |_| {
            Ok(Reply::new(200, brotli.clone()))
        });
        let mut raster = archive().bytes;
        raster[99] = 2;
        sim.route("GET", "https://cdn.example/raster.pmtiles", move |_| {
            Ok(Reply::new(200, raster.clone()))
        });
        sim.route("GET", "https://cdn.example/gone.pmtiles", |_| {
            Ok(Reply::new(404, Vec::new()))
        });
        sim.route("GET", "https://cdn.example/junk.pmtiles", |_| {
            Ok(Reply::new(
                200,
                b"not an archive, not at all, not even close".to_vec(),
            ))
        });
        let open_at = |url: &str| block_on(open(client(&sim), &vector(Some(url), &[])));
        assert!(matches!(
            open_at("https://cdn.example/brotli.pmtiles"),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            open_at("https://cdn.example/raster.pmtiles"),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            open_at("https://cdn.example/gone.pmtiles"),
            Err(Error::Status { status: 404, .. })
        ));
        assert!(matches!(
            open_at("https://cdn.example/junk.pmtiles"),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn directory_parse_and_find() {
        let entries = [
            entry(0, 0, 10, 3),
            entry(5, 10, 20, 1),
            entry(9, 0, 7, 0),
            entry(20, 100, 5, 2),
        ];
        let dir = Directory::parse(&encode_directory(&entries)).unwrap();
        assert_eq!(dir.entries, entries);
        assert_eq!(dir.find(0), Some(entries[0]));
        assert_eq!(dir.find(2), Some(entries[0]));
        assert_eq!(dir.find(3), None);
        assert_eq!(dir.find(5), Some(entries[1]));
        assert_eq!(dir.find(6), None);
        assert_eq!(dir.find(9), Some(entries[2]));
        assert_eq!(
            dir.find(15),
            Some(entries[2]),
            "a leaf pointer covers up to the next entry"
        );
        assert_eq!(dir.find(21), Some(entries[3]));
        assert_eq!(dir.find(22), None);
        assert!(matches!(Directory::parse(&[]), Err(Error::Invalid(_))));
        assert!(matches!(
            Directory::parse(&[200, 1]),
            Err(Error::Invalid(_))
        ));
        let truncated = encode_directory(&entries);
        assert!(matches!(
            Directory::parse(&truncated[..truncated.len() - 2]),
            Err(Error::Invalid(_))
        ));
        assert_eq!(Directory::default().find(0), None);
    }

    #[test]
    fn header_parses_every_field() {
        let h = Header::parse(&archive().bytes).unwrap();
        assert_eq!(h.root_offset, 127);
        assert!(h.clustered);
        assert_eq!(h.internal_compression, Compression::Gzip);
        assert_eq!(h.tile_compression, Compression::Gzip);
        assert_eq!(
            (h.tile_type, h.min_zoom, h.max_zoom, h.center_zoom),
            (1, 0, 2, 1)
        );
        assert_eq!(h.center, (2.5, 48.5));
        assert_eq!(
            (h.addressed_tiles, h.tile_entries, h.tile_contents),
            (5, 3, 2)
        );
        assert!(matches!(Header::parse(&[0; 100]), Err(Error::Invalid(_))));
        let mut v2 = archive().bytes;
        v2[7] = 2;
        assert!(matches!(Header::parse(&v2), Err(Error::Unsupported(_))));
    }

    #[test]
    fn pmtiles_url_forms() {
        assert_eq!(
            pmtiles_url("pmtiles://https://x.example/a.pmtiles").as_deref(),
            Some("https://x.example/a.pmtiles")
        );
        assert_eq!(
            pmtiles_url("https://x.example/a.pmtiles?v=1").as_deref(),
            Some("https://x.example/a.pmtiles?v=1")
        );
        assert_eq!(pmtiles_url("https://x.example/a.json"), None);
    }
}
