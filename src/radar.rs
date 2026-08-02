//! Radar slideshow pipeline: download -> SD, row-wise decode -> SD, stream -> panel.
//!
//! The ESP32 has far too little RAM to hold a decoded radar image (a single
//! 256x256 RGBA tile is 256 KB), so every stage of this module is streaming:
//!
//! 1. [`download_tiles`] downloads each radar tile straight to the SD card with
//!    [`crate::http::get_to_writer`] (512-byte chunks, nothing buffered).
//! 2. [`decode_tiles`] decodes each PNG **one scanline at a time**, blending the
//!    scanline over the basemap already in the frame file and writing it back as
//!    Rgb565. Only a couple of rows are ever in RAM.
//! 3. [`blit_frame`] reads a staged frame back in small row bands and pushes
//!    them into a `mipidsi` address window, so the panel is fed directly from
//!    the SD card without a framebuffer.
//!
//! The view is a 240x240 window in slippy-map pixel space centred on the
//! viewer, so it generally straddles a 2x2 block of tiles: each frame is
//! assembled from up to four tiles, each decoded into its own sub-rectangle of
//! the frame file (see [`View::tiles`]).
//!
//! The tile source is pluggable via [`RadarSource`] because Open-Meteo does not
//! serve radar imagery; [`RainViewer`] is the bundled implementation.
//!
//! Decoding goes through [`crate::png_stream`], which inflates through a
//! statically reserved 32 KiB window instead of allocating one, so the whole
//! pipeline runs without competing with Wi-Fi/TLS for the heap.

use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
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
const HEADER_LEN: u64 = 8;

/// Slippy-map tiles are always 256x256 pixels.
const TILE_SIZE: u32 = 256;

/// Background the radar is composited onto where no basemap is staged.
const RADAR_BG: (u8, u8, u8) = (8, 12, 20);

/// Colour and half-length of the crosshair marking the viewer's position.
const MARKER: (u8, u8, u8) = (255, 214, 0);
const MARKER_ARM: u32 = 5;

/// Rows pushed to the panel per `set_pixels` call when streaming a frame.
const BLIT_ROWS: usize = 4;

/// Prefix of the cached basemap frame, whose name carries the view it was cut
/// for so a stale one is never reused.
const BASEMAP_PREFIX: &str = "base_";

// ---------------------------------------------------------------------------
// Frame files
// ---------------------------------------------------------------------------

/// Path of the `i`-th staged frame on the SD card.
pub fn frame_path(index: usize) -> PathBuf {
    storage::path(&format!("{FRAME_DIR}/frame_{index}.rgb565"))
}

/// Path of a downloaded (still encoded) tile: quadrant `part` of frame `index`.
fn tile_path(index: usize, part: usize) -> PathBuf {
    storage::path(&format!("{FRAME_DIR}/tile_{index}_{part}.png"))
}

/// Path of a downloaded (still encoded) basemap quadrant.
fn basemap_tile_path(part: usize) -> PathBuf {
    storage::path(&format!("{FRAME_DIR}/base_{part}.png"))
}

/// Path of the sidecar listing the timestamp of each staged frame.
fn times_path() -> PathBuf {
    storage::path(&format!("{FRAME_DIR}/times.txt"))
}

/// When a staged frame was observed, or is forecast for.
#[derive(Debug, Clone, Copy)]
pub struct FrameTime {
    /// Unix timestamp of the frame.
    pub time: i64,
    /// True for nowcast frames, which are a prediction rather than a scan.
    pub forecast: bool,
}

/// Times of the staged frames, in animation order. Empty when they are
/// unknown, e.g. for frames staged by an older firmware.
pub fn frame_times() -> Vec<FrameTime> {
    let Ok(body) = std::fs::read_to_string(times_path()) else {
        return Vec::new();
    };
    body.lines()
        .filter_map(|line| {
            let (time, forecast) = line.trim().split_once(',')?;
            Some(FrameTime {
                time: time.parse().ok()?,
                forecast: forecast == "1",
            })
        })
        .collect()
}

fn write_times(times: &[FrameTime]) {
    let body: String = times
        .iter()
        .map(|t| format!("{},{}\n", t.time, u8::from(t.forecast)))
        .collect();
    if let Err(e) = std::fs::write(times_path(), body) {
        log::warn!("failed to record the radar frame times: {e:#}");
    }
}

/// Path of the decoded basemap frame for `view`.
fn basemap_frame_path(view: &View) -> PathBuf {
    storage::path(&format!(
        "{FRAME_DIR}/{BASEMAP_PREFIX}{}_{}_{}.rgb565",
        view.zoom, view.x0, view.y0
    ))
}

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

/// One animation frame offered by a [`RadarSource`].
pub struct FrameSpec {
    /// Tile URL template containing the `{z}`, `{x}` and `{y}` placeholders,
    /// because one frame is stitched from several tiles.
    pub url_template: String,
    /// When the frame was observed, or is forecast for.
    pub time: FrameTime,
}

/// A pluggable source of radar frames, oldest first.
pub trait RadarSource {
    /// Return up to `max_frames` frames, ordered oldest-first so they animate
    /// forwards.
    fn frames(&self, max_frames: usize) -> Result<Vec<FrameSpec>>;
}

/// RainViewer public radar tiles (past observations + nowcast).
pub struct RainViewer {
    pub color_scheme: u8,
}

impl Default for RainViewer {
    fn default() -> Self {
        Self {
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
    #[serde(default)]
    time: i64,
}

impl RadarSource for RainViewer {
    fn frames(&self, max_frames: usize) -> Result<Vec<FrameSpec>> {
        let body = crate::http::get(RAINVIEWER_INDEX_API, RAINVIEWER_INDEX_MAX_BYTES)
            .context("failed to fetch the RainViewer frame index")?;
        let index: RvIndex =
            serde_json::from_str(&body).context("failed to parse the RainViewer frame index")?;

        // Newest observations first, then the nowcast, then trim to the frame
        // budget and flip back to chronological order.
        let mut frames: Vec<(&RvFrame, bool)> = Vec::new();
        frames.extend(index.radar.nowcast.iter().rev().map(|f| (f, true)));
        frames.extend(index.radar.past.iter().rev().map(|f| (f, false)));
        frames.truncate(max_frames);
        frames.reverse();

        if frames.is_empty() {
            bail!("the RainViewer index contained no radar frames");
        }

        Ok(frames
            .into_iter()
            .map(|(frame, forecast)| FrameSpec {
                url_template: format!(
                    "{}{}/{}/{{z}}/{{x}}/{{y}}/{}/1_1.png",
                    index.host, frame.path, TILE_SIZE, self.color_scheme
                ),
                time: FrameTime {
                    time: frame.time,
                    forecast,
                },
            })
            .collect())
    }
}

/// Substitute the slippy-map coordinates into a tile URL template.
fn tile_url(template: &str, zoom: u8, x: u32, y: u32) -> String {
    template
        .replace("{z}", &zoom.to_string())
        .replace("{x}", &x.to_string())
        .replace("{y}", &y.to_string())
}

/// Fractional slippy-map tile coordinates for a position (Web Mercator).
fn tile_position(lat: f64, lon: f64, zoom: u8) -> (f64, f64) {
    let n = (1u32 << zoom) as f64;
    let x = (lon + 180.0) / 360.0 * n;
    let lat_rad = lat.to_radians();
    let y = (1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / std::f64::consts::PI) / 2.0 * n;
    (x, y)
}

// ---------------------------------------------------------------------------
// View geometry
// ---------------------------------------------------------------------------

/// The window of slippy-map pixel space shown on the panel, centred on the
/// viewer. Coordinates are global pixels at [`View::zoom`] (tile `t` spans
/// `t * 256 .. (t + 1) * 256`).
#[derive(Debug, Clone, Copy)]
pub struct View {
    pub zoom: u8,
    pub x0: u32,
    pub y0: u32,
    pub w: u32,
    pub h: u32,
    /// The viewer's position, in view coordinates.
    pub marker_x: u32,
    pub marker_y: u32,
}

/// One tile's contribution to a view: which part of the tile to decode and
/// where it lands in the frame.
#[derive(Debug, Clone, Copy)]
struct Placement {
    tile_x: u32,
    tile_y: u32,
    crop_x: u32,
    crop_y: u32,
    crop_w: u32,
    crop_h: u32,
    dst_x: u32,
    dst_y: u32,
}

impl View {
    /// The display-sized window centred on `lat`/`lon`, clamped to the world.
    pub fn centered_on(lat: f64, lon: f64, zoom: u8) -> Self {
        let w = u32::from(RADAR_VIEW_WIDTH);
        let h = u32::from(RADAR_VIEW_HEIGHT);
        let world = (1u32 << zoom) * TILE_SIZE;
        let (fx, fy) = tile_position(lat, lon, zoom);
        let gx = (fx * TILE_SIZE as f64) as u32;
        let gy = (fy * TILE_SIZE as f64) as u32;
        let x0 = gx.saturating_sub(w / 2).min(world.saturating_sub(w));
        let y0 = gy.saturating_sub(h / 2).min(world.saturating_sub(h));
        Self {
            zoom,
            x0,
            y0,
            w,
            h,
            marker_x: (gx - x0).min(w - 1),
            marker_y: (gy - y0).min(h - 1),
        }
    }

    /// The tiles the view straddles (up to 2x2), with their crops and offsets.
    fn tiles(&self) -> Vec<Placement> {
        let mut out = Vec::new();
        let first_x = self.x0 / TILE_SIZE;
        let last_x = (self.x0 + self.w - 1) / TILE_SIZE;
        let first_y = self.y0 / TILE_SIZE;
        let last_y = (self.y0 + self.h - 1) / TILE_SIZE;

        for tile_y in first_y..=last_y {
            for tile_x in first_x..=last_x {
                let (crop_x, crop_w, dst_x) = span(self.x0, self.w, tile_x);
                let (crop_y, crop_h, dst_y) = span(self.y0, self.h, tile_y);
                out.push(Placement {
                    tile_x,
                    tile_y,
                    crop_x,
                    crop_y,
                    crop_w,
                    crop_h,
                    dst_x,
                    dst_y,
                });
            }
        }
        out
    }

    fn row_bytes(&self) -> usize {
        self.w as usize * 2
    }
}

/// Intersect one axis of the view with one tile: returns the crop offset inside
/// the tile, its length, and where it starts in the frame.
fn span(origin: u32, len: u32, tile: u32) -> (u32, u32, u32) {
    let tile_origin = tile * TILE_SIZE;
    let start = origin.max(tile_origin);
    let end = (origin + len).min(tile_origin + TILE_SIZE);
    (start - tile_origin, end - start, start - origin)
}

// ---------------------------------------------------------------------------
// Stage 3: incremental decode -> compact Rgb565 frame
// ---------------------------------------------------------------------------

/// Build one frame at `dst`: start from the basemap (or a flat background),
/// then blend each downloaded tile into its own sub-rectangle.
///
/// Every tile is decoded a scanline at a time and each scanline is merged
/// straight into the frame file, so RAM never holds more than a row.
fn compose_frame(
    dst: &Path,
    view: &View,
    tiles: &[(Placement, PathBuf)],
    base: Option<&Path>,
    marker: bool,
) -> Result<usize> {
    match base {
        Some(base) => {
            std::fs::copy(base, dst)
                .with_context(|| format!("failed to seed {} from the basemap", dst.display()))?;
        }
        None => create_blank_frame(dst, view)?,
    }

    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dst)
        .with_context(|| format!("failed to open {}", dst.display()))?;

    let mut merged = 0usize;
    for (placement, tile) in tiles {
        if !tile.is_file() {
            continue;
        }
        match merge_tile(&mut file, view, placement, tile) {
            Ok(()) => merged += 1,
            Err(e) => log::warn!("merging {} failed: {e:#}", tile.display()),
        }
    }

    if marker {
        draw_marker(&mut file, view).context("failed to draw the location marker")?;
    }
    file.flush()
        .with_context(|| format!("failed to flush {}", dst.display()))?;
    Ok(merged)
}

/// Write a frame file of `view`'s size filled with the flat radar background.
fn create_blank_frame(dst: &Path, view: &View) -> Result<()> {
    let mut out = BufWriter::with_capacity(
        1024,
        File::create(dst).with_context(|| format!("failed to create {}", dst.display()))?,
    );
    let mut header = [0u8; HEADER_LEN as usize];
    header[..4].copy_from_slice(&MAGIC);
    header[4..6].copy_from_slice(&(view.w as u16).to_le_bytes());
    header[6..8].copy_from_slice(&(view.h as u16).to_le_bytes());
    out.write_all(&header)
        .context("failed to write the frame header")?;

    let mut row = vec![0u8; view.row_bytes()];
    let bg = rgb565(RADAR_BG.0, RADAR_BG.1, RADAR_BG.2).to_le_bytes();
    for px in row.chunks_exact_mut(2) {
        px.copy_from_slice(&bg);
    }
    for _ in 0..view.h {
        out.write_all(&row)
            .with_context(|| format!("failed to write {}", dst.display()))?;
    }
    out.flush()
        .with_context(|| format!("failed to flush {}", dst.display()))?;
    Ok(())
}

/// Decode `tile` and blend it over the pixels already in `frame`.
fn merge_tile(frame: &mut File, view: &View, placement: &Placement, tile: &Path) -> Result<()> {
    let png = File::open(tile).with_context(|| format!("failed to open {}", tile.display()))?;
    let mut sink = TileSink {
        frame,
        view_w: view.w,
        placement: *placement,
        row: vec![0u8; placement.crop_w as usize * 2],
    };
    png_stream::decode(BufReader::with_capacity(512, png), &mut sink)
        .with_context(|| format!("failed to decode {}", tile.display()))
}

/// Blends decoded scanlines into one sub-rectangle of an open frame file.
struct TileSink<'a> {
    frame: &'a mut File,
    view_w: u32,
    placement: Placement,
    row: Vec<u8>,
}

impl RowSink for TileSink<'_> {
    fn start(&mut self, width: u32, height: u32) -> Result<()> {
        let p = &self.placement;
        if p.crop_x + p.crop_w > width || p.crop_y + p.crop_h > height {
            bail!(
                "crop {}x{}+{}+{} does not fit the {width}x{height} tile",
                p.crop_w,
                p.crop_h,
                p.crop_x,
                p.crop_y
            );
        }
        Ok(())
    }

    fn row(&mut self, y: u32, row: &Row) -> Result<bool> {
        let p = self.placement;
        if y < p.crop_y {
            return Ok(true);
        }
        let dy = y - p.crop_y;
        if dy >= p.crop_h {
            return Ok(false);
        }

        let offset = HEADER_LEN + ((p.dst_y + dy) as u64 * self.view_w as u64 + p.dst_x as u64) * 2;
        self.frame.seek(SeekFrom::Start(offset))?;
        self.frame
            .read_exact(&mut self.row)
            .context("failed to read the frame row being blended into")?;

        for i in 0..p.crop_w as usize {
            let (r, g, b, a) = row.pixel(p.crop_x as usize + i);
            let (br, bg, bb) =
                unpack565(u16::from_le_bytes([self.row[i * 2], self.row[i * 2 + 1]]));
            let raw = rgb565(blend(r, a, br), blend(g, a, bg), blend(b, a, bb));
            self.row[i * 2..i * 2 + 2].copy_from_slice(&raw.to_le_bytes());
        }

        self.frame.seek(SeekFrom::Start(offset))?;
        self.frame
            .write_all(&self.row)
            .context("failed to write a blended frame row")?;
        Ok(dy + 1 < p.crop_h)
    }
}

/// Stamp the viewer's crosshair into a finished frame.
fn draw_marker(frame: &mut File, view: &View) -> Result<()> {
    let raw = rgb565(MARKER.0, MARKER.1, MARKER.2).to_le_bytes();
    let pixel_at = |x: u32, y: u32| HEADER_LEN + (y as u64 * view.w as u64 + x as u64) * 2;

    let x_start = view.marker_x.saturating_sub(MARKER_ARM);
    let x_end = (view.marker_x + MARKER_ARM).min(view.w - 1);
    let arm = raw.repeat((x_end - x_start + 1) as usize);
    frame.seek(SeekFrom::Start(pixel_at(x_start, view.marker_y)))?;
    frame.write_all(&arm)?;

    let y_start = view.marker_y.saturating_sub(MARKER_ARM);
    let y_end = (view.marker_y + MARKER_ARM).min(view.h - 1);
    for y in y_start..=y_end {
        frame.seek(SeekFrom::Start(pixel_at(view.marker_x, y)))?;
        frame.write_all(&raw)?;
    }
    Ok(())
}

/// Composite one channel of a (possibly transparent) radar pixel onto the
/// pixel already in the frame.
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

/// Phase 1 (network): download the tiles for up to [`RADAR_FRAME_COUNT`] radar
/// frames covering `lat`/`lon`. Returns how many frames were staged.
pub fn download_tiles(source: &dyn RadarSource, lat: f64, lon: f64) -> Result<usize> {
    storage::ensure_dir(FRAME_DIR)?;
    log_heap("before download");

    let view = View::centered_on(lat, lon, RADAR_ZOOM);
    let placements = view.tiles();
    let frames = source.frames(RADAR_FRAME_COUNT)?;

    let mut times = Vec::with_capacity(frames.len());
    let mut staged = 0usize;
    for frame in frames.iter() {
        let mut parts = 0usize;
        for (part, placement) in placements.iter().enumerate() {
            let url = tile_url(
                &frame.url_template,
                view.zoom,
                placement.tile_x,
                placement.tile_y,
            );
            let path = tile_path(staged, part);
            match download_tile(&url, &path) {
                Ok(()) => parts += 1,
                Err(e) => {
                    log::warn!("radar tile {url} failed: {e:#}");
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        if parts > 0 {
            staged += 1;
            times.push(frame.time);
        }
    }

    if staged == 0 {
        bail!("no radar tiles could be downloaded");
    }
    write_times(&times);
    log::info!("downloaded {staged} radar frames to the SD card");

    // The basemap never changes, so it is only fetched when the frame cached
    // for this view is missing. Its absence only costs the labels.
    if let Err(e) = download_basemap(&view, &placements) {
        log::warn!("basemap unavailable: {e:#}");
    }
    Ok(staged)
}

/// Download the basemap tiles for `view` unless its frame is already staged.
fn download_basemap(view: &View, placements: &[Placement]) -> Result<()> {
    if basemap_frame_path(view).is_file() {
        return Ok(());
    }
    for (part, placement) in placements.iter().enumerate() {
        let url = tile_url(
            BASEMAP_TILE_URL,
            view.zoom,
            placement.tile_x,
            placement.tile_y,
        );
        download_tile(&url, &basemap_tile_path(part))
            .with_context(|| format!("failed to download the basemap tile {url}"))?;
    }
    Ok(())
}

fn download_tile(url: &str, path: &Path) -> Result<()> {
    let mut file =
        File::create(path).with_context(|| format!("failed to create {}", path.display()))?;
    crate::http::get_to_writer(url, &mut file, RADAR_TILE_MAX_BYTES)?;
    Ok(())
}

/// Phase 2: compose the `count` staged frames into `frame_{i}.rgb565` files and
/// delete the encoded tiles.
pub fn decode_tiles(count: usize, lat: f64, lon: f64) -> Result<usize> {
    log_heap("before decode");
    let view = View::centered_on(lat, lon, RADAR_ZOOM);
    let placements = view.tiles();
    let base = prepare_basemap(&view, &placements);

    let downloaded = frame_times();
    let mut times = Vec::with_capacity(count);
    let mut staged = 0usize;
    for i in 0..count {
        let tiles: Vec<(Placement, PathBuf)> = placements
            .iter()
            .enumerate()
            .map(|(part, placement)| (*placement, tile_path(i, part)))
            .collect();

        match compose_frame(&frame_path(staged), &view, &tiles, base.as_deref(), true) {
            Ok(0) => log::warn!("radar frame {i} had no usable tiles"),
            Ok(_) => {
                staged += 1;
                if let Some(time) = downloaded.get(i) {
                    times.push(*time);
                }
            }
            Err(e) => log::warn!("composing radar frame {i} failed: {e:#}"),
        }
        for (_, tile) in tiles {
            let _ = std::fs::remove_file(tile);
        }
    }

    // Drop any stale frames left over from a longer previous run.
    for i in staged..RADAR_FRAME_COUNT {
        let _ = std::fs::remove_file(frame_path(i));
    }
    write_times(&times);

    log_heap("after decode");
    if staged == 0 {
        bail!("no radar frames could be decoded");
    }
    log::info!("staged {staged} radar frames on the SD card");
    Ok(staged)
}

/// Compose the downloaded basemap tiles (if any) into the cached basemap frame
/// and discard basemaps cut for a different view. Returns the frame to use as
/// the background, when one is available.
fn prepare_basemap(view: &View, placements: &[Placement]) -> Option<PathBuf> {
    let frame = basemap_frame_path(view);
    let tiles: Vec<(Placement, PathBuf)> = placements
        .iter()
        .enumerate()
        .map(|(part, placement)| (*placement, basemap_tile_path(part)))
        .collect();

    if tiles.iter().any(|(_, tile)| tile.is_file()) {
        // The basemap is opaque, so it simply replaces the flat background.
        match compose_frame(&frame, view, &tiles, None, false) {
            Ok(_) => log::info!("staged the radar basemap"),
            Err(e) => log::warn!("composing the basemap failed: {e:#}"),
        }
        for (_, tile) in &tiles {
            let _ = std::fs::remove_file(tile);
        }
    }

    // Basemaps cut for a previous view are dead weight on the card.
    if let Ok(entries) = std::fs::read_dir(storage::path(FRAME_DIR)) {
        for entry in entries.flatten() {
            let path = entry.path();
            let stale = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(BASEMAP_PREFIX) && n.ends_with(".rgb565"))
                && path != frame;
            if stale {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    frame.is_file().then_some(frame)
}

// ---------------------------------------------------------------------------
// Stage 4: stream a staged frame from the SD card to the panel
// ---------------------------------------------------------------------------

/// Stream the frame at `path` onto the display with its top-left corner at
/// (`x`, `y`), a few rows at a time. The frame is never fully in RAM.
pub fn blit_frame(display: &mut CydDisplay, path: &Path, x: u16, y: u16) -> Result<()> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open {}", path.display()))?;

    let mut header = [0u8; HEADER_LEN as usize];
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
