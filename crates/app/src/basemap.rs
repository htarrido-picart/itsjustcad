// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Georeferenced satellite/OSM tile underlay.
//!
//! A basemap is a raster ground image stitched from web-map tiles and placed
//! under the model on the z=0 plane, projected to local meters with the SAME
//! equirectangular projection used by GeoJSON/OSM import ([`GeoOrigin`]), so it
//! lines up with imported site geometry. It is TRANSIENT session/view state:
//! the stitched pixels are never written into the op-log or save file (they can
//! be several megabytes and are reproducible from the location). See
//! [`itsjustcad_doc::Basemap`].
//!
//! OFFLINE / SEALED STANCE. Nothing here touches the network on its own. The
//! module is split into three pure, testable layers:
//!   1. slippy-tile math ([`lonlat_to_tile`], [`TileId`]) — pure arithmetic;
//!   2. a pluggable [`TileProvider`] that maps a tile to a URL and cache key
//!      (OSM raster + a keyless satellite source, NO API key by default);
//!   3. a [`TileSource`] that hands back tile PNG bytes. The mock/cache sources
//!      never hit the network; the live HTTP source is the ONLY thing that
//!      does, and it is only constructed when the user explicitly opts in.
//!
//! [`build_basemap`] ties them together: it decodes each tile PNG, blits it into
//! one big RGBA canvas, and returns a georeferenced [`Basemap`]. It is generic
//! over the [`TileSource`], so tests drive it with a mock (no live network).

use std::path::PathBuf;

use itsjustcad_commands::geo::GeoOrigin;
use itsjustcad_doc::{Basemap, GeoLocation};

/// Web-Mercator tile side in pixels (the universal slippy-map tile size).
pub const TILE_PX: u32 = 256;

// ── slippy-tile math ────────────────────────────────────────────────────────

/// A slippy-map tile address: zoom `z` and integer tile column `x` / row `y`.
/// `x` grows east, `y` grows south, both in `0..2^z`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TileId {
    pub z: u32,
    pub x: u32,
    pub y: u32,
}

/// Number of tiles along one axis at zoom `z` (`2^z`).
pub fn tiles_per_axis(z: u32) -> u32 {
    1u32 << z
}

/// Convert a lon/lat (degrees) at zoom `z` to a FRACTIONAL tile coordinate
/// `(fx, fy)` under the standard Web-Mercator slippy scheme. The integer parts
/// are the containing [`TileId`]; the fractions locate the point inside it.
///
/// Latitude is clamped to the Web-Mercator limit (±85.0511°) so the `tan`/`ln`
/// stay finite at the poles.
pub fn lonlat_to_tile_frac(lon_deg: f64, lat_deg: f64, z: u32) -> (f64, f64) {
    let n = tiles_per_axis(z) as f64;
    let lat = lat_deg.clamp(-85.051_128_78, 85.051_128_78).to_radians();
    let fx = (lon_deg + 180.0) / 360.0 * n;
    let fy = (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * n;
    (fx, fy)
}

/// The tile that CONTAINS a lon/lat at zoom `z`.
pub fn lonlat_to_tile(lon_deg: f64, lat_deg: f64, z: u32) -> TileId {
    let (fx, fy) = lonlat_to_tile_frac(lon_deg, lat_deg, z);
    let max = tiles_per_axis(z).saturating_sub(1);
    TileId {
        z,
        x: (fx.floor() as i64).clamp(0, max as i64) as u32,
        y: (fy.floor() as i64).clamp(0, max as i64) as u32,
    }
}

/// Longitude of a tile's WEST edge (its left border), in degrees.
pub fn tile_west_lon(x: u32, z: u32) -> f64 {
    x as f64 / tiles_per_axis(z) as f64 * 360.0 - 180.0
}

/// Latitude of a tile's NORTH edge (its top border), in degrees.
pub fn tile_north_lat(y: u32, z: u32) -> f64 {
    let n = tiles_per_axis(z) as f64;
    let t = std::f64::consts::PI * (1.0 - 2.0 * y as f64 / n);
    t.sinh().atan().to_degrees()
}

/// A rectangular block of tiles (inclusive ranges) at one zoom, covering a
/// lon/lat bounding box with `margin` extra tiles on every side for context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TileGrid {
    pub z: u32,
    pub x0: u32,
    pub y0: u32,
    /// Number of tile columns (`x`) and rows (`y`).
    pub cols: u32,
    pub rows: u32,
}

impl TileGrid {
    /// Cover a lon/lat box `[west,east] × [south,north]` at zoom `z`, padded by
    /// `margin` tiles on each side (clamped to the valid `0..2^z` range).
    pub fn covering(
        west: f64,
        south: f64,
        east: f64,
        north: f64,
        z: u32,
        margin: u32,
    ) -> TileGrid {
        let max = tiles_per_axis(z).saturating_sub(1);
        // North latitude → smaller y; west longitude → smaller x.
        let tl = lonlat_to_tile(west, north, z);
        let br = lonlat_to_tile(east, south, z);
        let x0 = tl.x.saturating_sub(margin);
        let y0 = tl.y.saturating_sub(margin);
        let x1 = (br.x + margin).min(max);
        let y1 = (br.y + margin).min(max);
        TileGrid {
            z,
            x0,
            y0,
            cols: x1 - x0 + 1,
            rows: y1 - y0 + 1,
        }
    }

    /// Iterate the tile ids row-major (north-west first).
    pub fn tiles(&self) -> impl Iterator<Item = TileId> + '_ {
        (0..self.rows).flat_map(move |dy| {
            (0..self.cols).map(move |dx| TileId {
                z: self.z,
                x: self.x0 + dx,
                y: self.y0 + dy,
            })
        })
    }

    /// Assembled canvas size in pixels.
    pub fn canvas_px(&self) -> (u32, u32) {
        (self.cols * TILE_PX, self.rows * TILE_PX)
    }

    /// The geographic corners of the whole grid: `(west, north, east, south)`
    /// degrees — the outer borders of the corner tiles.
    pub fn bounds_deg(&self) -> (f64, f64, f64, f64) {
        let west = tile_west_lon(self.x0, self.z);
        let east = tile_west_lon(self.x0 + self.cols, self.z);
        let north = tile_north_lat(self.y0, self.z);
        let south = tile_north_lat(self.y0 + self.rows, self.z);
        (west, north, east, south)
    }
}

// ── pluggable provider ───────────────────────────────────────────────────────

/// A basemap tile provider: turns a [`TileId`] into a fetch URL and a stable
/// on-disk cache key. Pluggable so satellite / OSM / a private WMTS can drop in.
pub trait TileProvider: Send + Sync {
    /// Short slug used in the cache path and status label (`osm`, `sat`).
    fn slug(&self) -> &str;
    /// Fully-qualified tile URL. NO API key required for the built-ins.
    fn tile_url(&self, t: TileId) -> String;
    /// Attribution string (basemaps legally require it).
    fn attribution(&self) -> &str;
}

/// OpenStreetMap standard raster tiles (`tile.openstreetmap.org`). Keyless.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsmProvider;

impl TileProvider for OsmProvider {
    fn slug(&self) -> &str {
        "osm"
    }
    fn tile_url(&self, t: TileId) -> String {
        format!("https://tile.openstreetmap.org/{}/{}/{}.png", t.z, t.x, t.y)
    }
    fn attribution(&self) -> &str {
        "© OpenStreetMap contributors"
    }
}

/// Esri "World Imagery" satellite tiles — a keyless open source in the common
/// `{z}/{y}/{x}` (row before column) order used by ArcGIS tile services.
#[derive(Clone, Copy, Debug, Default)]
pub struct SatelliteProvider;

impl TileProvider for SatelliteProvider {
    fn slug(&self) -> &str {
        "sat"
    }
    fn tile_url(&self, t: TileId) -> String {
        format!(
            "https://server.arcgisonline.com/ArcGIS/rest/services/\
World_Imagery/MapServer/tile/{}/{}/{}",
            t.z, t.y, t.x
        )
    }
    fn attribution(&self) -> &str {
        "Imagery © Esri, Maxar, Earthstar Geographics"
    }
}

/// Pick a built-in provider by name; defaults to OSM for anything unknown.
pub fn provider_by_name(name: &str) -> Box<dyn TileProvider> {
    match name.trim().to_ascii_lowercase().as_str() {
        "sat" | "satellite" | "imagery" | "esri" => Box::new(SatelliteProvider),
        _ => Box::new(OsmProvider),
    }
}

/// On-disk cache path for a tile: `<cache>/basemap/<slug>/<z>/<x>/<y>.png`.
pub fn tile_cache_path(cache_root: &std::path::Path, slug: &str, t: TileId) -> PathBuf {
    cache_root
        .join("basemap")
        .join(slug)
        .join(t.z.to_string())
        .join(t.x.to_string())
        .join(format!("{}.png", t.y))
}

/// Default cache root: `~/.cache/itsjustcad` (falls back to the temp dir).
pub fn default_cache_root() -> PathBuf {
    dirs::cache_dir()
        .map(|d| d.join("itsjustcad"))
        .unwrap_or_else(std::env::temp_dir)
}

// ── tile source (the ONLY network boundary) ──────────────────────────────────

/// Something that yields a tile's PNG bytes. The trait is the seam between the
/// pure stitcher and the outside world: tests use [`MockTileSource`] (no
/// network), the app uses a cache-then-HTTP source that only reaches the
/// network after an explicit opt-in.
pub trait TileSource {
    /// PNG bytes for `t`, or an error string. MUST NOT block indefinitely.
    fn fetch(&self, t: TileId) -> Result<Vec<u8>, String>;
}

/// A fully in-memory source: hand it a map of `TileId → PNG bytes`. Never
/// touches the network or disk — the workhorse for tests and cached replays.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct MockTileSource {
    tiles: std::collections::HashMap<TileId, Vec<u8>>,
}

#[cfg(test)]
impl MockTileSource {
    pub fn new() -> Self {
        Self::default()
    }
    /// Fill every tile of a grid with the same solid-colour PNG. Handy for
    /// sanity shots and tests that just need a recognisable underlay.
    pub fn fill_solid(&mut self, grid: &TileGrid, rgba: [u8; 4]) {
        let png = solid_tile_png(rgba);
        for t in grid.tiles() {
            self.tiles.insert(t, png.clone());
        }
    }
}

#[cfg(test)]
impl TileSource for MockTileSource {
    fn fetch(&self, t: TileId) -> Result<Vec<u8>, String> {
        self.tiles
            .get(&t)
            .cloned()
            .ok_or_else(|| format!("no mock tile for {}/{}/{}", t.z, t.x, t.y))
    }
}

/// Encode a solid-colour `TILE_PX × TILE_PX` RGBA PNG. Used by the mock source
/// and the offline sanity path (no network needed to see a basemap).
pub fn solid_tile_png(rgba: [u8; 4]) -> Vec<u8> {
    let mut buf = Vec::with_capacity((TILE_PX * TILE_PX * 4) as usize);
    for _ in 0..(TILE_PX * TILE_PX) {
        buf.extend_from_slice(&rgba);
    }
    encode_png_rgba(&buf, TILE_PX, TILE_PX).expect("solid tile encodes")
}

/// A cache-first tile source: read `<cache>/basemap/<slug>/z/x/y.png` if
/// present, else fetch over HTTP (blocking on the app's tokio runtime) via the
/// provider's URL, WRITE the bytes to the cache, and return them.
///
/// This is the ONLY type in the module that can touch the network, and it does
/// so only when constructed — the app constructs it exclusively after the user
/// opts in with `basemap ...`. When `allow_network` is false it is a pure disk
/// cache: a cache miss is an error, never a fetch (the offline/sealed default).
pub struct CachedHttpTileSource {
    provider: Box<dyn TileProvider>,
    cache_root: PathBuf,
    handle: tokio::runtime::Handle,
    allow_network: bool,
}

impl CachedHttpTileSource {
    /// Build a source. `allow_network` gates live fetches; with it `false` this
    /// only ever reads the on-disk cache (offline replay of a warm cache).
    pub fn new(
        provider: Box<dyn TileProvider>,
        cache_root: PathBuf,
        handle: tokio::runtime::Handle,
        allow_network: bool,
    ) -> Self {
        Self {
            provider,
            cache_root,
            handle,
            allow_network,
        }
    }

    /// Blocking HTTP GET of one tile on the tokio runtime. A short timeout keeps
    /// a stalled server from wedging the UI thread. Sends a descriptive
    /// User-Agent (OSM's tile policy requires one).
    fn http_get(&self, url: &str) -> Result<Vec<u8>, String> {
        let url = url.to_string();
        self.handle.block_on(async move {
            let client = reqwest::Client::builder()
                .user_agent("ItsJustCAD/0.1 (basemap; +https://github.com)")
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .map_err(|e| e.to_string())?;
            let resp = client.get(&url).send().await.map_err(|e| e.to_string())?;
            if !resp.status().is_success() {
                return Err(format!("tile server returned {}", resp.status()));
            }
            let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
            Ok(bytes.to_vec())
        })
    }
}

impl TileSource for CachedHttpTileSource {
    fn fetch(&self, t: TileId) -> Result<Vec<u8>, String> {
        let path = tile_cache_path(&self.cache_root, self.provider.slug(), t);
        if let Ok(bytes) = std::fs::read(&path)
            && !bytes.is_empty()
        {
            return Ok(bytes);
        }
        if !self.allow_network {
            return Err(format!(
                "tile {}/{}/{} not cached and network is off",
                t.z, t.x, t.y
            ));
        }
        let bytes = self.http_get(&self.provider.tile_url(t))?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, &bytes);
        Ok(bytes)
    }
}

/// Encode raw RGBA8 pixels to PNG bytes.
pub fn encode_png_rgba(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    use image::{ImageEncoder, ImageError};
    let mut out = std::io::Cursor::new(Vec::new());
    image::codecs::png::PngEncoder::new(&mut out)
        .write_image(rgba, w, h, image::ExtendedColorType::Rgba8)
        .map_err(|e: ImageError| e.to_string())?;
    Ok(out.into_inner())
}

// ── stitcher ─────────────────────────────────────────────────────────────────

/// Choose a slippy zoom level so the ~`span_m`-metre site fills roughly
/// `target_px` pixels at the given latitude. Clamped to `[1, 19]` (past ~19 the
/// public tile servers have no data). Higher zoom = finer detail, more tiles.
pub fn pick_zoom(lat_deg: f64, span_m: f64, target_px: f64) -> u32 {
    // Web-Mercator ground resolution at zoom z: metres/pixel =
    //   156543.03 * cos(lat) / 2^z. Solve for z so span_m ≈ target_px * m/px.
    const EQUATOR_M_PER_PX_Z0: f64 = 156_543.033_928_04;
    let cos_lat = lat_deg.to_radians().cos().max(1e-6);
    let want_m_per_px = (span_m / target_px).max(1e-6);
    let z = (EQUATOR_M_PER_PX_Z0 * cos_lat / want_m_per_px).log2();
    (z.round() as i64).clamp(1, 19) as u32
}

/// Build a georeferenced [`Basemap`] for a location: choose a zoom, cover a
/// `span_m`-metre square with tiles, fetch each via `source`, stitch them into
/// one RGBA canvas, and georeference the canvas to local meters via `origin`
/// (the same projection as GeoJSON import).
///
/// `source` is the network boundary; with a [`MockTileSource`] or a warm cache
/// this runs entirely offline. `opacity` is the blend used when rendering.
pub fn build_basemap(
    loc: GeoLocation,
    span_m: f64,
    opacity: f32,
    label_slug: &str,
    source: &dyn TileSource,
) -> Result<Basemap, String> {
    let origin = GeoOrigin {
        lat_deg: loc.lat_deg,
        lon_deg: loc.lon_deg,
    };
    let z = pick_zoom(loc.lat_deg, span_m, 1024.0);

    // Degree half-span of the site square about the location.
    let half = span_m / 2.0;
    let m_per_deg_lat = 111_320.0;
    let m_per_deg_lon = (m_per_deg_lat * loc.lat_deg.to_radians().cos()).max(1.0);
    let dlat = half / m_per_deg_lat;
    let dlon = half / m_per_deg_lon;
    let grid = TileGrid::covering(
        loc.lon_deg - dlon,
        loc.lat_deg - dlat,
        loc.lon_deg + dlon,
        loc.lat_deg + dlat,
        z,
        0,
    );

    let (cw, ch) = grid.canvas_px();
    if cw == 0 || ch == 0 {
        return Err("empty tile grid".into());
    }
    let mut canvas = vec![0u8; (cw * ch * 4) as usize];
    for (i, t) in grid.tiles().enumerate() {
        let png = source.fetch(t)?;
        let img = image::load_from_memory(&png)
            .map_err(|e| format!("decode tile {}/{}/{}: {e}", t.z, t.x, t.y))?
            .to_rgba8();
        let dx = (i as u32 % grid.cols) * TILE_PX;
        let dy = (i as u32 / grid.cols) * TILE_PX;
        blit(&mut canvas, cw, &img, dx, dy);
    }

    // Georeference: the canvas spans the grid's geographic bounds. Project the
    // NW and SE corners to local meters; the lower-left doc corner is (west,
    // south) so +x is east and +y is north (image row 0 = north = top).
    let (west, north, east, south) = grid.bounds_deg();
    let ll = origin.project_public(west, south);
    let ur = origin.project_public(east, north);
    let corner = glam::DVec2::new(ll[0], ll[1]);
    let width = ur[0] - ll[0];
    let height = ur[1] - ll[1];

    // Flip rows so image top (north) maps to +y (north) in doc space: the
    // renderer treats corner as lower-left with row 0 at the TOP visually, so
    // we vertically flip the canvas to put south at the bottom.
    let flipped = flip_vertical(&canvas, cw, ch);

    Ok(Basemap {
        rgba: flipped,
        width_px: cw,
        height_px: ch,
        corner,
        width,
        height,
        opacity: opacity.clamp(0.0, 1.0),
        label: format!("{label_slug} z{z}"),
    })
}

/// Copy an RGBA tile into `canvas` (row-major, `canvas_w` px wide) at pixel
/// offset `(dx, dy)`. Assumes the tile fits inside the canvas.
fn blit(canvas: &mut [u8], canvas_w: u32, tile: &image::RgbaImage, dx: u32, dy: u32) {
    let (tw, th) = tile.dimensions();
    for row in 0..th {
        let src = tile.as_raw();
        let src_off = (row * tw * 4) as usize;
        let dst_off = (((dy + row) * canvas_w + dx) * 4) as usize;
        let n = (tw * 4) as usize;
        canvas[dst_off..dst_off + n].copy_from_slice(&src[src_off..src_off + n]);
    }
}

/// Flip an RGBA image vertically (top row becomes bottom row).
fn flip_vertical(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
    let stride = (w * 4) as usize;
    let mut out = vec![0u8; rgba.len()];
    for row in 0..h as usize {
        let src = row * stride;
        let dst = (h as usize - 1 - row) * stride;
        out[dst..dst + stride].copy_from_slice(&rgba[src..src + stride]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(lat: f64, lon: f64) -> GeoLocation {
        GeoLocation {
            lat_deg: lat,
            lon_deg: lon,
            tz_hours: 0.0,
        }
    }

    // ── tile math ───────────────────────────────────────────────────────────

    #[test]
    fn tiles_per_axis_is_power_of_two() {
        assert_eq!(tiles_per_axis(0), 1);
        assert_eq!(tiles_per_axis(1), 2);
        assert_eq!(tiles_per_axis(16), 65_536);
    }

    #[test]
    fn null_island_sits_at_grid_centre() {
        // lon0/lat0 at zoom z is the boundary between the four central tiles:
        // fractional coord = n/2 exactly.
        let (fx, fy) = lonlat_to_tile_frac(0.0, 0.0, 2);
        assert!((fx - 2.0).abs() < 1e-9, "fx={fx}");
        assert!((fy - 2.0).abs() < 1e-9, "fy={fy}");
    }

    #[test]
    fn known_slippy_tile_berlin_z16() {
        // Berlin (52.5163, 13.3777) at z16 is a well-published slippy address.
        let t = lonlat_to_tile(13.3777, 52.5163, 16);
        assert_eq!(t.z, 16);
        assert_eq!(t.x, 35203);
        assert_eq!(t.y, 21493);
    }

    #[test]
    fn west_of_greenwich_is_left_half() {
        // Any western longitude lands in the left half of the map (x < n/2).
        let t = lonlat_to_tile(-122.4, 37.8, 10); // San Francisco
        assert!(t.x < tiles_per_axis(10) / 2, "x={}", t.x);
    }

    #[test]
    fn tile_edges_round_trip_to_containing_tile() {
        let z = 14;
        let t = lonlat_to_tile(-73.9857, 40.7484, z); // Empire State Building
        let w = tile_west_lon(t.x, z);
        let n = tile_north_lat(t.y, z);
        // A point just inside the NW corner must map back to the same tile.
        let back = lonlat_to_tile(w + 1e-6, n - 1e-6, z);
        assert_eq!(back, t);
    }

    #[test]
    fn north_lat_decreases_with_y() {
        // Row 0 is the north edge (+~85°); higher y is further south.
        assert!(tile_north_lat(0, 8) > tile_north_lat(128, 8));
        assert!((tile_north_lat(128, 8)).abs() < 1e-6); // equator at the middle row
    }

    // ── grid coverage ─────────────────────────────────────────────────────────

    #[test]
    fn covering_a_point_with_margin_gives_odd_block() {
        // A degenerate box (a point) padded by 1 tile → 3×3 block centred on it.
        let g = TileGrid::covering(13.3777, 52.5163, 13.3777, 52.5163, 16, 1);
        assert_eq!(g.cols, 3);
        assert_eq!(g.rows, 3);
        assert_eq!(g.canvas_px(), (768, 768));
        assert_eq!(g.tiles().count(), 9);
    }

    #[test]
    fn grid_bounds_enclose_the_request() {
        let (w, s, e, n) = (13.30, 52.48, 13.45, 52.55);
        let g = TileGrid::covering(w, s, e, n, 15, 0);
        let (gw, gn, ge, gs) = g.bounds_deg();
        assert!(gw <= w && ge >= e, "lon {gw}..{ge} vs {w}..{e}");
        assert!(gn >= n && gs <= s, "lat {gs}..{gn} vs {s}..{n}");
    }

    // ── provider ────────────────────────────────────────────────────────────

    #[test]
    fn osm_url_is_z_x_y_png() {
        let u = OsmProvider.tile_url(TileId { z: 16, x: 35207, y: 21493 });
        assert_eq!(u, "https://tile.openstreetmap.org/16/35207/21493.png");
    }

    #[test]
    fn satellite_url_is_z_y_x() {
        // ArcGIS imagery uses row-before-column ordering.
        let u = SatelliteProvider.tile_url(TileId { z: 10, x: 5, y: 7 });
        assert!(u.ends_with("/10/7/5"), "{u}");
    }

    #[test]
    fn provider_by_name_defaults_to_osm() {
        assert_eq!(provider_by_name("nonsense").slug(), "osm");
        assert_eq!(provider_by_name("satellite").slug(), "sat");
        assert_eq!(provider_by_name("SAT").slug(), "sat");
    }

    #[test]
    fn cache_path_is_slug_z_x_y() {
        let p = tile_cache_path(
            std::path::Path::new("/c"),
            "osm",
            TileId { z: 3, x: 4, y: 5 },
        );
        assert!(p.ends_with("basemap/osm/3/4/5.png"), "{p:?}");
    }

    // ── zoom pick ─────────────────────────────────────────────────────────────

    #[test]
    fn pick_zoom_is_monotonic_in_span() {
        // A smaller site (finer detail) demands a higher zoom.
        let close = pick_zoom(40.0, 200.0, 1024.0);
        let far = pick_zoom(40.0, 5000.0, 1024.0);
        assert!(close > far, "close={close} far={far}");
        assert!((1..=19).contains(&close));
    }

    // ── mock fetch + stitch (NO network) ──────────────────────────────────────

    #[test]
    fn mock_source_errors_on_missing_tile() {
        let src = MockTileSource::new();
        assert!(src.fetch(TileId { z: 1, x: 0, y: 0 }).is_err());
    }

    #[test]
    fn solid_tile_png_decodes_back_to_colour() {
        let png = solid_tile_png([10, 20, 30, 255]);
        let img = image::load_from_memory(&png).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (TILE_PX, TILE_PX));
        assert_eq!(img.get_pixel(0, 0).0, [10, 20, 30, 255]);
    }

    #[test]
    fn build_basemap_stitches_mocked_tiles_offline() {
        let l = loc(52.5163, 13.3777);
        // Pre-compute the grid the builder will use so we can seed exactly it.
        let z = pick_zoom(l.lat_deg, 400.0, 1024.0);
        let half = 200.0;
        let dlat = half / 111_320.0;
        let dlon = half / (111_320.0 * l.lat_deg.to_radians().cos());
        let grid = TileGrid::covering(
            l.lon_deg - dlon,
            l.lat_deg - dlat,
            l.lon_deg + dlon,
            l.lat_deg + dlat,
            z,
            0,
        );
        let mut src = MockTileSource::new();
        src.fill_solid(&grid, [200, 180, 140, 255]);

        let b = build_basemap(l, 400.0, 0.7, "osm", &src).unwrap();
        let (cw, ch) = grid.canvas_px();
        assert_eq!(b.width_px, cw);
        assert_eq!(b.height_px, ch);
        assert_eq!(b.rgba.len(), (cw * ch * 4) as usize);
        // Solid fill survives the stitch + flip.
        assert_eq!(&b.rgba[0..4], &[200, 180, 140, 255]);
        // Georeferenced about the origin: the site square straddles (0,0), so
        // the lower-left corner is negative in both axes and the span positive.
        assert!(b.corner.x < 0.0 && b.corner.y < 0.0, "{:?}", b.corner);
        assert!(b.width > 0.0 && b.height > 0.0);
        assert_eq!(b.opacity, 0.7);
        assert_eq!(b.label, format!("osm z{z}"));
    }

    #[test]
    fn build_basemap_georeference_matches_geojson_projection() {
        // The basemap MUST use the same GeoOrigin projection as GeoJSON import,
        // so its corner equals project_public of the grid's SW corner.
        let l = loc(40.0, -74.0);
        let z = pick_zoom(l.lat_deg, 600.0, 1024.0);
        let half = 300.0;
        let dlat = half / 111_320.0;
        let dlon = half / (111_320.0 * l.lat_deg.to_radians().cos());
        let grid = TileGrid::covering(
            l.lon_deg - dlon,
            l.lat_deg - dlat,
            l.lon_deg + dlon,
            l.lat_deg + dlat,
            z,
            0,
        );
        let mut src = MockTileSource::new();
        src.fill_solid(&grid, [1, 2, 3, 255]);
        let b = build_basemap(l, 600.0, 1.0, "osm", &src).unwrap();

        let origin = GeoOrigin {
            lat_deg: l.lat_deg,
            lon_deg: l.lon_deg,
        };
        let (west, _n, _e, south) = grid.bounds_deg();
        let sw = origin.project_public(west, south);
        assert!((b.corner.x - sw[0]).abs() < 1e-6);
        assert!((b.corner.y - sw[1]).abs() < 1e-6);
    }

    // ── cached HTTP source: offline behaviour (NO live network) ──────────────

    #[test]
    fn cached_source_offline_miss_is_an_error_not_a_fetch() {
        // allow_network=false → a cache miss must error, never reach the net.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = std::env::temp_dir().join("ijc_basemap_miss");
        let _ = std::fs::remove_dir_all(&dir);
        let src = CachedHttpTileSource::new(Box::new(OsmProvider), dir, rt.handle().clone(), false);
        assert!(src.fetch(TileId { z: 1, x: 0, y: 0 }).is_err());
    }

    #[test]
    fn cached_source_reads_a_warm_cache_offline() {
        // Seed the cache on disk, then fetch with network OFF: the bytes come
        // back straight from disk — proving a warm cache replays with no net.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dir = std::env::temp_dir().join("ijc_basemap_warm");
        let _ = std::fs::remove_dir_all(&dir);
        let t = TileId { z: 2, x: 1, y: 1 };
        let path = tile_cache_path(&dir, "osm", t);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let png = solid_tile_png([9, 8, 7, 255]);
        std::fs::write(&path, &png).unwrap();

        let src = CachedHttpTileSource::new(
            Box::new(OsmProvider),
            dir.clone(),
            rt.handle().clone(),
            false,
        );
        assert_eq!(src.fetch(t).unwrap(), png);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_basemap_missing_tile_is_an_error_not_a_panic() {
        let l = loc(0.0, 0.0);
        let src = MockTileSource::new(); // empty
        assert!(build_basemap(l, 500.0, 1.0, "osm", &src).is_err());
    }
}
