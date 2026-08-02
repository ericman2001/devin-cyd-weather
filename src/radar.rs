//! Radar slideshow pipeline: download -> SD, row-wise decode -> SD, stream -> panel.
//!
//! The ESP32 has far too little RAM to hold a decoded radar image (a single
//! 256x256 RGBA tile is 256 KB), so every stage of this module is streaming:
//!
//! 1. [`download_tiles`] downloads each radar tile straight to the SD card with
//!    [`crate::http::get_to_writer`] (512-byte chunks, nothing buffered).
//! 2. [`decode_tiles`] decodes each PNG **one scanline at a time**, converts
//!    the scanline to Rgb565 while cropping/downsampling it to the display
//!    region, and appends it to a compact `frame_{i}.rgb565` file. Only one
//!    source row plus one output row (a few hundred bytes) is ever in RAM.
//! 3. [`blit_frame`] reads a staged frame back in small row bands and pushes
//!    them into a `mipidsi` address window, so the panel is fed directly from
//!    the SD card without a framebuffer.
//!
//! The tile source is pluggable via [`RadarSource`] because Open-Meteo does not
//! serve radar imagery; [`RainViewer`] is the bundled implementation.
//!
//! Decoding goes through [`crate::png_stream`], which inflates through a
//! statically reserved 32 KiB window instead of allocating one, so the whole
//! pipeline runs without competing with Wi-Fi/TLS for the heap.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use embedded_graphics::pixelcolor::raw::RawU16;
use embedded_graphics::pixelcolor::Rgb565;
use serde::Deserialize;

use crate::config::{
    BASEMAP_TILE_URL, RADAR_COLOR_SCHEME, RADAR_FRAME_COUNT, RADAR_TILE_MAX_BYTES,
    RADAR_VIEW_HEIGHT, RADAR_VIEW_WIDTH, RADAR_ZOOM, RAINVIEWER_INDEX_API,
    RAINVIEWER_INDEX_MAX_BYTES,
};
use crate::display::CydDisplay;
use crate::png_stream::{self, Row, RowSink};
use crate::storage;

/// Directory (under the SD mount point) holding the staged radar frames.
const FRAME_DIR: &str = "radar";

/// Magic + dimensions prefixed to every `.rgb565` frame file.
const MAGIC: [u8; 4] = *b"R565";
const HEADER_LEN: usize = 8;

/// Radar tiles are always 256x256 pixels.
const TILE_SIZE: u32 = 256;

/// Background the (mostly transparent) radar tiles are composited onto where
/// no basemap is staged.
const RADAR_BG: (u8, u8, u8) = (8, 12, 20);

/// Colour and half-length of the crosshair marking the viewer's position.
const MARKER: (u8, u8, u8) = (255, 214, 0);
const MARKER_ARM: u32 = 5;

/// Rows pushed to the panel per `set_pixels` call when streaming a frame.
const BLIT_ROWS: usize = 4;

// ---------------------------------------------------------------------------
// Frame files
// ---------------------------------------------------------------------------

/// Path of the `i`-th staged frame on the SD card.
pub fn frame_path(index: usize) -> PathBuf {
    storage::path(&format!("{FRAME_DIR}/frame_{index}.rgb565"))
}

/// Path of the `i`-th downloaded (still encoded) radar tile.
fn tile_path(index: usize) -> PathBuf {
    storage::path(&format!("{FRAME_DIR}/tile_{index}.png"))
}

/// Path of the downloaded (still encoded) basemap tile.
fn basemap_tile_path() -> PathBuf {
    storage::path(&format!("{FRAME_DIR}/base.png"))
}

/// Path of the decoded basemap frame. The geometry is baked into the name so a
/// staged basemap is only reused for the location and zoom it was cut for.
fn basemap_frame_path(geom: &Geometry) -> PathBuf {
    storage::path(&format!(
        "{FRAME_DIR}/{BASEMAP_PREFIX}{}_{}_{}_{}_{}.rgb565",
        geom.zoom, geom.tile_x, geom.tile_y, geom.crop_x, geom.crop_y
    ))
}

const BASEMAP_PREFIX: &str = "base_";

/// Log the heap headroom around the memory-hungry stages.
fn log_heap(stage: &str) {
    let (free, largest) = heap_stats();
    log::info!("heap[{stage}]: free={free} B, largest block={largest} B");
}

/// Total free heap and the largest contiguous internal block, in bytes.
fn heap_stats() -> (u32, usize) {
    use esp_idf_svc::sys::{
        esp_get_free_heap_size, heap_caps_get_largest_free_block, MALLOC_CAP_8BIT,
        MALLOC_CAP_INTERNAL,
    };
    unsafe {
        (
            esp_get_free_heap_size(),
            heap_caps_get_largest_free_block(MALLOC_CAP_8BIT | MALLOC_CAP_INTERNAL),
        )
    }
}

/// Number of frames currently staged on the SD card.
pub fn staged_frames() -> usize {
    (0..RADAR_FRAME_COUNT)
        .take_while(|i| frame_path(*i).is_file())
        .count()
}

// ---------------------------------------------------------------------------
// Tile sources
// ---------------------------------------------------------------------------

/// A pluggable source of radar tile URLs, newest last.
pub trait RadarSource {
    /// Return up to `max_frames` tile URLs covering the given position,
    /// ordered oldest-first so they animate forwards.
    fn frame_urls(&self, lat: f64, lon: f64, max_frames: usize) -> Result<Vec<String>>;
}

/// RainViewer public radar tiles (past observations + nowcast).
pub struct RainViewer {
    pub zoom: u8,
    pub color_scheme: u8,
}

impl Default for RainViewer {
    fn default() -> Self {
        Self {
            zoom: RADAR_ZOOM,
            color_scheme: RADAR_COLOR_SCHEME,
        }
    }
}

#[derive(Deserialize)]
struct RvIndex {
    host: String,
    radar: RvRadar,
}

#[derive(Deserialize)]
struct RvRadar {
    #[serde(default)]
    past: Vec<RvFrame>,
    #[serde(default)]
    nowcast: Vec<RvFrame>,
}

#[derive(Deserialize)]
struct RvFrame {
    path: String,
}

impl RadarSource for RainViewer {
    fn frame_urls(&self, lat: f64, lon: f64, max_frames: usize) -> Result<Vec<String>> {
        let body = crate::http::get(RAINVIEWER_INDEX_API, RAINVIEWER_INDEX_MAX_BYTES)
            .context("failed to fetch the RainViewer frame index")?;
        let index: RvIndex =
            serde_json::from_str(&body).context("failed to parse the RainViewer frame index")?;

        // Newest observations first, then the nowcast, then trim to the frame
        // budget and flip back to chronological order.
        let mut paths: Vec<&str> = Vec::new();
        paths.extend(index.radar.nowcast.iter().rev().map(|f| f.path.as_str()));
        paths.extend(index.radar.past.iter().rev().map(|f| f.path.as_str()));
        paths.truncate(max_frames);
        paths.reverse();

        if paths.is_empty() {
            bail!("the RainViewer index contained no radar frames");
        }

        let (x, y) = tile_index(lat, lon, self.zoom);
        Ok(paths
            .into_iter()
            .map(|path| {
                format!(
                    "{}{}/{}/{}/{}/{}/{}/1_1.png",
                    index.host, path, TILE_SIZE, self.zoom, x, y, self.color_scheme
                )
            })
            .collect())
    }
}

/// Fractional slippy-map tile coordinates for a position (Web Mercator).
fn tile_position(lat: f64, lon: f64, zoom: u8) -> (f64, f64) {
    let n = (1u32 << zoom) as f64;
    let x = (lon + 180.0) / 360.0 * n;
    let lat_rad = lat.to_radians();
    let y = (1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n;
    (x, y)
}

/// Integer tile indices containing a position.
fn tile_index(lat: f64, lon: f64, zoom: u8) -> (u32, u32) {
    let (x, y) = tile_position(lat, lon, zoom);
    let max = (1u32 << zoom) - 1;
    (
        (x.floor().max(0.0) as u32).min(max),
        (y.floor().max(0.0) as u32).min(max),
    )
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// Source crop plus target size for the row-wise decoder.
#[derive(Debug, Clone, Copy)]
pub struct Geometry {
    pub crop_x: u32,
    pub crop_y: u32,
    pub crop_w: u32,
    pub crop_h: u32,
    pub out_w: u32,
    pub out_h: u32,
    /// Tile the crop was taken from, for naming the cached basemap frame.
    pub zoom: u8,
    pub tile_x: u32,
    pub tile_y: u32,
    /// The viewer's position in output coordinates.
    pub marker_x: u32,
    pub marker_y: u32,
}

impl Geometry {
    /// A crop of the display's size, centred on the position within its tile so
    /// the viewer's location stays in the middle of the radar view.
    pub fn centered_on(lat: f64, lon: f64, zoom: u8) -> Self {
        let out_w = u32::from(RADAR_VIEW_WIDTH).min(TILE_SIZE);
        let out_h = u32::from(RADAR_VIEW_HEIGHT).min(TILE_SIZE);
        let (fx, fy) = tile_position(lat, lon, zoom);
        let px = (fx.fract() * TILE_SIZE as f64) as u32;
        let py = (fy.fract() * TILE_SIZE as f64) as u32;
        let crop_x = px.saturating_sub(out_w / 2).min(TILE_SIZE - out_w);
        let crop_y = py.saturating_sub(out_h / 2).min(TILE_SIZE - out_h);
        Self {
            crop_x,
            crop_y,
            crop_w: out_w,
            crop_h: out_h,
            out_w,
            out_h,
            zoom,
            tile_x: fx.floor().max(0.0) as u32,
            tile_y: fy.floor().max(0.0) as u32,
            marker_x: px.saturating_sub(crop_x).min(out_w - 1),
            marker_y: py.saturating_sub(crop_y).min(out_h - 1),
        }
    }
}

// ---------------------------------------------------------------------------
// Stage 3: incremental decode -> compact Rgb565 frame
// ---------------------------------------------------------------------------

/// Decode `src` (a PNG on the SD card) into a raw Rgb565 frame at `dst`.
///
/// The image is processed one scanline at a time and the inflate window lives
/// in static memory, so peak heap is two source rows plus one output row.
pub fn decode_to_frame(
    src: &Path,
    dst: &Path,
    geom: &Geometry,
    base: Option<BufReader<File>>,
    marker: bool,
) -> Result<()> {
    let file = File::open(src).with_context(|| format!("failed to open {}", src.display()))?;

    let mut out = BufWriter::with_capacity(
        1024,
        File::create(dst).with_context(|| format!("failed to create {}", dst.display()))?,
    );
    write_header(&mut out, geom.out_w as u16, geom.out_h as u16)?;

    let mut base_row = vec![0u8; geom.out_w as usize * 2];
    let flat = rgb565(RADAR_BG.0, RADAR_BG.1, RADAR_BG.2).to_le_bytes();
    for px in base_row.chunks_exact_mut(2) {
        px.copy_from_slice(&flat);
    }

    let mut sink = FrameSink {
        out: &mut out,
        geom,
        row_out: vec![0u8; geom.out_w as usize * 2],
        out_y: 0,
        base,
        base_row,
        marker,
    };
    png_stream::decode(BufReader::with_capacity(512, file), &mut sink)
        .with_context(|| format!("failed to decode {}", src.display()))?;

    // Pad if the source ran short so the frame always has its declared size.
    let mut out_y = sink.out_y;
    let blank = vec![0u8; geom.out_w as usize * 2];
    while out_y < geom.out_h {
        out.write_all(&blank)
            .with_context(|| format!("failed to write {}", dst.display()))?;
        out_y += 1;
    }

    out.flush()
        .with_context(|| format!("failed to flush {}", dst.display()))?;
    Ok(())
}

/// Turns decoded scanlines into the cropped, resampled Rgb565 frame body.
struct FrameSink<'a, W: Write> {
    out: &'a mut W,
    geom: &'a Geometry,
    row_out: Vec<u8>,
    out_y: u32,
    /// Staged basemap frame, positioned just past its header, read one row at a
    /// time so the radar can be composited over coastlines and place labels.
    base: Option<BufReader<File>>,
    /// The current basemap row, or a flat background when there is no basemap.
    base_row: Vec<u8>,
    /// Whether to draw the viewer's crosshair over the finished rows.
    marker: bool,
}

impl<W: Write> RowSink for FrameSink<'_, W> {
    fn start(&mut self, width: u32, height: u32) -> Result<()> {
        let g = self.geom;
        if g.crop_x + g.crop_w > width || g.crop_y + g.crop_h > height {
            bail!(
                "crop {}x{}+{}+{} does not fit the {width}x{height} source image",
                g.crop_w,
                g.crop_h,
                g.crop_x,
                g.crop_y
            );
        }
        Ok(())
    }

    fn row(&mut self, y: u32, row: &Row) -> Result<bool> {
        // Nearest-neighbour vertical resampling: emit every output row that
        // maps onto the source row we are currently holding.
        let g = self.geom;
        while self.out_y < g.out_h && g.crop_y + self.out_y * g.crop_h / g.out_h == y {
            self.load_base_row();
            convert_row(row, g, &self.base_row, &mut self.row_out);
            if self.marker {
                draw_marker(&mut self.row_out, self.out_y, g);
            }
            self.out
                .write_all(&self.row_out)
                .context("failed to write a frame row")?;
            self.out_y += 1;
        }
        Ok(self.out_y < g.out_h)
    }
}

impl<W: Write> FrameSink<'_, W> {
    /// Pull the next basemap row. A short or unreadable basemap simply drops
    /// back to the flat background already in `base_row`.
    fn load_base_row(&mut self) {
        let Some(base) = self.base.as_mut() else {
            return;
        };
        if let Err(e) = base.read_exact(&mut self.base_row) {
            log::warn!("basemap row {} unreadable: {e:#}", self.out_y);
            self.base = None;
        }
    }
}

fn write_header<W: Write>(out: &mut W, width: u16, height: u16) -> Result<()> {
    let mut header = [0u8; HEADER_LEN];
    header[..4].copy_from_slice(&MAGIC);
    header[4..6].copy_from_slice(&width.to_le_bytes());
    header[6..8].copy_from_slice(&height.to_le_bytes());
    out.write_all(&header)
        .context("failed to write the frame header")
}

/// Convert one decoded source scanline into a horizontally resampled Rgb565
/// row, compositing it over the matching `base` row.
fn convert_row(src: &Row, geom: &Geometry, base: &[u8], dst: &mut [u8]) {
    for out_x in 0..geom.out_w as usize {
        let src_x = geom.crop_x as usize + out_x * geom.crop_w as usize / geom.out_w as usize;
        let (r, g, b, a) = src.pixel(src_x);
        let (br, bg, bb) = unpack565(u16::from_le_bytes([base[out_x * 2], base[out_x * 2 + 1]]));
        let raw = rgb565(blend(r, a, br), blend(g, a, bg), blend(b, a, bb));
        dst[out_x * 2..out_x * 2 + 2].copy_from_slice(&raw.to_le_bytes());
    }
}

/// Stamp the viewer's crosshair into an output row.
fn draw_marker(dst: &mut [u8], y: u32, geom: &Geometry) {
    let raw = rgb565(MARKER.0, MARKER.1, MARKER.2).to_le_bytes();
    let mut put = |x: u32| {
        if x < geom.out_w {
            dst[x as usize * 2..x as usize * 2 + 2].copy_from_slice(&raw);
        }
    };
    if y == geom.marker_y {
        for x in geom.marker_x.saturating_sub(MARKER_ARM)..=geom.marker_x + MARKER_ARM {
            put(x);
        }
    } else if y.abs_diff(geom.marker_y) <= MARKER_ARM {
        put(geom.marker_x);
    }
}

/// Composite one channel of a (possibly transparent) radar pixel onto the
/// radar background.
fn blend(value: u8, alpha: u8, background: u8) -> u8 {
    let a = alpha as u16;
    ((value as u16 * a + background as u16 * (255 - a)) / 255) as u8
}

fn rgb565(r: u8, g: u8, b: u8) -> u16 {
    ((r as u16 & 0xF8) << 8) | ((g as u16 & 0xFC) << 3) | (b as u16 >> 3)
}

fn unpack565(raw: u16) -> (u8, u8, u8) {
    (
        ((raw >> 8) & 0xF8) as u8,
        ((raw >> 3) & 0xFC) as u8,
        ((raw << 3) & 0xF8) as u8,
    )
}

// ---------------------------------------------------------------------------
// Orchestration: fetch N frames onto the SD card
// ---------------------------------------------------------------------------

/// Phase 1 (network): download up to [`RADAR_FRAME_COUNT`] radar tiles for
/// `lat`/`lon` onto the SD card. Returns how many tiles were staged.
pub fn download_tiles(source: &dyn RadarSource, lat: f64, lon: f64) -> Result<usize> {
    storage::ensure_dir(FRAME_DIR)?;
    log_heap("before download");

    let urls = source.frame_urls(lat, lon, RADAR_FRAME_COUNT)?;

    let mut staged = 0usize;
    for url in urls.iter() {
        let path = tile_path(staged);
        match download_tile(url, &path) {
            Ok(()) => staged += 1,
            Err(e) => {
                log::warn!("radar tile {url} failed: {e:#}");
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    if staged == 0 {
        bail!("no radar tiles could be downloaded");
    }
    log::info!("downloaded {staged} radar tiles to the SD card");

    // The basemap never changes, so it is only fetched when the cached frame
    // for this location is missing. Its absence only costs the labels.
    if let Err(e) = download_basemap(lat, lon) {
        log::warn!("basemap unavailable: {e:#}");
    }
    Ok(staged)
}

/// Download the basemap tile for `lat`/`lon` unless its frame is already staged.
fn download_basemap(lat: f64, lon: f64) -> Result<()> {
    let geom = Geometry::centered_on(lat, lon, RADAR_ZOOM);
    if basemap_frame_path(&geom).is_file() {
        return Ok(());
    }
    let url = BASEMAP_TILE_URL
        .replace("{z}", &geom.zoom.to_string())
        .replace("{x}", &geom.tile_x.to_string())
        .replace("{y}", &geom.tile_y.to_string());
    download_tile(&url, &basemap_tile_path())
}

fn download_tile(url: &str, path: &Path) -> Result<()> {
    let mut file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    crate::http::get_to_writer(url, &mut file, RADAR_TILE_MAX_BYTES)?;
    Ok(())
}

/// Decode the downloaded basemap tile (if any) into its cached frame and
/// discard basemaps cut for a different location.
fn prepare_basemap(geom: &Geometry) {
    let frame = basemap_frame_path(geom);
    let tile = basemap_tile_path();
    if tile.is_file() {
        // The basemap is opaque, so it composites over the flat background.
        match decode_to_frame(&tile, &frame, geom, None, false) {
            Ok(()) => log::info!("staged the radar basemap"),
            Err(e) => log::warn!("decoding the basemap failed: {e:#}"),
        }
        let _ = std::fs::remove_file(&tile);
    }

    // Basemaps cut for a previous location are dead weight on the card.
    if let Ok(entries) = std::fs::read_dir(storage::path(FRAME_DIR)) {
        for entry in entries.flatten() {
            let path = entry.path();
            let stale = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(BASEMAP_PREFIX))
                && path != frame;
            if stale {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Open the cached basemap frame, positioned at its first pixel row.
fn open_basemap(geom: &Geometry) -> Option<BufReader<File>> {
    let path = basemap_frame_path(geom);
    let mut reader = BufReader::with_capacity(512, File::open(&path).ok()?);
    let mut header = [0u8; HEADER_LEN];
    reader.read_exact(&mut header).ok()?;
    (header[..4] == MAGIC).then_some(reader)
}

/// Phase 2: decode the `count` staged tiles into compact `frame_{i}.rgb565`
/// files and delete the encoded originals.
pub fn decode_tiles(count: usize, lat: f64, lon: f64) -> Result<usize> {
    log_heap("before decode");
    let geom = Geometry::centered_on(lat, lon, RADAR_ZOOM);
    prepare_basemap(&geom);

    let mut staged = 0usize;
    for i in 0..count {
        let tile = tile_path(i);
        match decode_to_frame(&tile, &frame_path(staged), &geom, open_basemap(&geom), true) {
            Ok(()) => staged += 1,
            Err(e) => log::warn!("decoding {} failed: {e:#}", tile.display()),
        }
        let _ = std::fs::remove_file(&tile);
    }

    // Drop any stale frames left over from a longer previous run.
    for i in staged..RADAR_FRAME_COUNT {
        let _ = std::fs::remove_file(frame_path(i));
    }

    log_heap("after decode");
    if staged == 0 {
        bail!("no radar frames could be decoded");
    }
    log::info!("staged {staged} radar frames on the SD card");
    Ok(staged)
}

// ---------------------------------------------------------------------------
// Stage 4: stream a staged frame from the SD card to the panel
// ---------------------------------------------------------------------------

/// Stream the frame at `path` onto the display with its top-left corner at
/// (`x`, `y`), a few rows at a time. The frame is never fully in RAM.
pub fn blit_frame(display: &mut CydDisplay, path: &Path, x: u16, y: u16) -> Result<()> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;

    let mut header = [0u8; HEADER_LEN];
    file.read_exact(&mut header)
        .with_context(|| format!("failed to read the header of {}", path.display()))?;
    if header[..4] != MAGIC {
        bail!("{} is not an rgb565 frame", path.display());
    }
    let width = u16::from_le_bytes([header[4], header[5]]);
    let height = u16::from_le_bytes([header[6], header[7]]);
    if width == 0 || height == 0 {
        bail!("{} declares an empty frame", path.display());
    }

    let row_bytes = width as usize * 2;
    let mut band = vec![0u8; row_bytes * BLIT_ROWS];

    let mut row = 0u16;
    while row < height {
        let rows = BLIT_ROWS.min((height - row) as usize);
        let chunk = &mut band[..row_bytes * rows];
        file.read_exact(chunk)
            .with_context(|| format!("failed to read pixel data from {}", path.display()))?;

        let pixels = chunk
            .chunks_exact(2)
            .map(|p| Rgb565::from(RawU16::new(u16::from_le_bytes([p[0], p[1]]))));
        display
            .set_pixels(x, y + row, x + width - 1, y + row + rows as u16 - 1, pixels)
            .map_err(|e| anyhow::anyhow!("failed to blit a frame band: {e:?}"))?;

        row += rows as u16;
    }

    Ok(())
}
