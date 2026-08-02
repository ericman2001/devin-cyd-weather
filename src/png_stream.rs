//! A scanline-at-a-time PNG reader that makes no large heap allocations.
//!
//! The `png` crate is unusable here: its zlib layer grows several 32 KiB
//! buffers on the heap, and on an ESP32 running Wi-Fi that allocation fails —
//! which aborts the firmware instead of returning an error. This module drives
//! `miniz_oxide`'s raw inflate core over a **statically reserved** 32 KiB
//! sliding window (in `.bss`, sized at link time), so decoding costs no heap
//! at all beyond two scanline buffers.
//!
//! Only the subset needed for radar tiles is supported: non-interlaced images
//! with 8-bit channels, or palette images at 1/2/4/8 bits.

use std::cell::UnsafeCell;
use std::io::Read;
use std::mem::{size_of, MaybeUninit};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use miniz_oxide::inflate::core::inflate_flags::{
    TINFL_FLAG_HAS_MORE_INPUT, TINFL_FLAG_IGNORE_ADLER32, TINFL_FLAG_PARSE_ZLIB_HEADER,
};
use miniz_oxide::inflate::core::{decompress, DecompressorOxide};
use miniz_oxide::inflate::TINFLStatus;

/// DEFLATE's sliding window. Must stay a power of two: `decompress` uses
/// `len - 1` as the wrap-around mask.
const WINDOW: usize = 32 * 1024;
const WINDOW_MASK: usize = WINDOW - 1;

/// Bytes of compressed data handed to the inflater per call.
const IN_CHUNK: usize = 512;

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

// ---------------------------------------------------------------------------
// Statically reserved decoder scratch
// ---------------------------------------------------------------------------

struct Scratch {
    window: UnsafeCell<[u8; WINDOW]>,
    decomp: UnsafeCell<MaybeUninit<DecompressorOxide>>,
}

// Access is serialised by `SCRATCH_BUSY`; only one `ScratchGuard` can exist.
unsafe impl Sync for Scratch {}

static SCRATCH: Scratch = Scratch {
    window: UnsafeCell::new([0; WINDOW]),
    decomp: UnsafeCell::new(MaybeUninit::uninit()),
};

static SCRATCH_BUSY: AtomicBool = AtomicBool::new(false);

/// Exclusive handle to the static window + inflate state.
struct ScratchGuard {
    window: &'static mut [u8; WINDOW],
    decomp: &'static mut DecompressorOxide,
}

impl ScratchGuard {
    fn acquire() -> Result<Self> {
        if SCRATCH_BUSY.swap(true, Ordering::Acquire) {
            bail!("the PNG decoder scratch buffer is already in use");
        }
        // SAFETY: we hold the only outstanding claim on the statics. An
        // all-zero `DecompressorOxide` is its `Default` value (`State::Start`
        // is discriminant 0), so zeroing the storage produces a valid value;
        // `init` then puts the state machine at the start of a stream.
        let decomp = unsafe {
            let ptr = (*SCRATCH.decomp.get()).as_mut_ptr();
            std::ptr::write_bytes(ptr.cast::<u8>(), 0, size_of::<DecompressorOxide>());
            &mut *ptr
        };
        decomp.init();
        let window = unsafe { &mut *SCRATCH.window.get() };
        Ok(Self { window, decomp })
    }
}

impl Drop for ScratchGuard {
    fn drop(&mut self) {
        SCRATCH_BUSY.store(false, Ordering::Release);
    }
}

// ---------------------------------------------------------------------------
// Image description
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorType {
    Grayscale,
    Rgb,
    Indexed,
    GrayscaleAlpha,
    Rgba,
}

impl ColorType {
    fn from_byte(value: u8) -> Result<Self> {
        Ok(match value {
            0 => Self::Grayscale,
            2 => Self::Rgb,
            3 => Self::Indexed,
            4 => Self::GrayscaleAlpha,
            6 => Self::Rgba,
            other => bail!("unsupported PNG colour type {other}"),
        })
    }

    fn channels(self) -> usize {
        match self {
            Self::Grayscale | Self::Indexed => 1,
            Self::GrayscaleAlpha => 2,
            Self::Rgb => 3,
            Self::Rgba => 4,
        }
    }
}

/// Everything the per-pixel accessor needs about the image.
struct Format {
    color: ColorType,
    depth: u8,
    /// RGB triples, for indexed images.
    palette: Vec<u8>,
    /// Per-index alpha, for indexed images.
    transparency: Vec<u8>,
}

impl Format {
    /// Bytes per pixel, rounded up; the distance filters look back by.
    fn filter_bpp(&self) -> usize {
        (self.color.channels() * self.depth as usize)
            .div_ceil(8)
            .max(1)
    }

    fn stride(&self, width: u32) -> usize {
        (width as usize * self.color.channels() * self.depth as usize).div_ceil(8)
    }
}

/// One unfiltered source scanline, with pixel access resolved lazily.
pub struct Row<'a> {
    data: &'a [u8],
    format: &'a Format,
}

impl Row<'_> {
    /// The pixel at `x` as straight (non-premultiplied) RGBA.
    pub fn pixel(&self, x: usize) -> (u8, u8, u8, u8) {
        let f = self.format;
        match f.color {
            ColorType::Indexed => {
                let index = sub_byte_sample(self.data, x, f.depth) as usize;
                let rgb = f
                    .palette
                    .get(index * 3..index * 3 + 3)
                    .unwrap_or(&[0, 0, 0]);
                let alpha = f.transparency.get(index).copied().unwrap_or(255);
                (rgb[0], rgb[1], rgb[2], alpha)
            }
            ColorType::Grayscale => {
                let raw = sub_byte_sample(self.data, x, f.depth);
                // Scale the sample up so 1/2/4-bit greys span the full range.
                let v = (u16::from(raw) * 255 / u16::from(u8::MAX >> (8 - f.depth))) as u8;
                (v, v, v, 255)
            }
            ColorType::GrayscaleAlpha => {
                let px = &self.data[x * 2..x * 2 + 2];
                (px[0], px[0], px[0], px[1])
            }
            ColorType::Rgb => {
                let px = &self.data[x * 3..x * 3 + 3];
                (px[0], px[1], px[2], 255)
            }
            ColorType::Rgba => {
                let px = &self.data[x * 4..x * 4 + 4];
                (px[0], px[1], px[2], px[3])
            }
        }
    }
}

/// Read one 1/2/4/8-bit sample, scaled up to a full byte.
fn sub_byte_sample(data: &[u8], x: usize, depth: u8) -> u8 {
    match depth {
        8 => data.get(x).copied().unwrap_or(0),
        4 => (data.get(x / 2).copied().unwrap_or(0) >> (4 * (1 - x % 2))) & 0x0f,
        2 => (data.get(x / 4).copied().unwrap_or(0) >> (2 * (3 - x % 4))) & 0x03,
        _ => (data.get(x / 8).copied().unwrap_or(0) >> (7 - x % 8)) & 0x01,
    }
}

/// Receives the decoded image, one scanline at a time.
pub trait RowSink {
    /// Called once, before any row, with the image dimensions.
    fn start(&mut self, width: u32, height: u32) -> Result<()>;
    /// Called per scanline top to bottom. Return `Ok(false)` to stop early.
    fn row(&mut self, y: u32, row: &Row) -> Result<bool>;
}

// ---------------------------------------------------------------------------
// Chunk layer
// ---------------------------------------------------------------------------

/// Walks the PNG chunk stream, exposing the concatenated IDAT payload.
struct Chunks<R: Read> {
    src: R,
    /// Bytes left in the IDAT chunk being read.
    remaining: u32,
    done: bool,
}

impl<R: Read> Chunks<R> {
    fn new(src: R) -> Self {
        Self {
            src,
            remaining: 0,
            done: false,
        }
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.src
            .read_exact(buf)
            .context("unexpected end of PNG data")
    }

    fn skip(&mut self, mut count: u64) -> Result<()> {
        let mut sink = [0u8; 64];
        while count > 0 {
            let n = (count as usize).min(sink.len());
            self.read_exact(&mut sink[..n])?;
            count -= n as u64;
        }
        Ok(())
    }

    /// Read the leading metadata chunks, stopping at the first IDAT.
    fn read_header(&mut self) -> Result<(u32, u32, Format)> {
        let mut signature = [0u8; 8];
        self.read_exact(&mut signature)?;
        if signature != SIGNATURE {
            bail!("not a PNG file");
        }

        let mut width = 0u32;
        let mut height = 0u32;
        let mut format: Option<Format> = None;
        let mut palette = Vec::new();
        let mut transparency = Vec::new();

        loop {
            let mut header = [0u8; 8];
            self.read_exact(&mut header)?;
            let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
            let kind = [header[4], header[5], header[6], header[7]];

            match &kind {
                b"IHDR" => {
                    if len != 13 {
                        bail!("malformed PNG IHDR chunk");
                    }
                    let mut ihdr = [0u8; 13];
                    self.read_exact(&mut ihdr)?;
                    self.skip(4)?; // CRC
                    width = u32::from_be_bytes([ihdr[0], ihdr[1], ihdr[2], ihdr[3]]);
                    height = u32::from_be_bytes([ihdr[4], ihdr[5], ihdr[6], ihdr[7]]);
                    let depth = ihdr[8];
                    let color = ColorType::from_byte(ihdr[9])?;
                    if ihdr[12] != 0 {
                        bail!("interlaced PNGs are not supported");
                    }
                    let depth_ok = match color {
                        ColorType::Indexed | ColorType::Grayscale => matches!(depth, 1 | 2 | 4 | 8),
                        _ => depth == 8,
                    };
                    if !depth_ok {
                        bail!("unsupported PNG bit depth {depth} for colour type {color:?}");
                    }
                    if width == 0 || height == 0 {
                        bail!("PNG declares an empty image");
                    }
                    format = Some(Format {
                        color,
                        depth,
                        palette: Vec::new(),
                        transparency: Vec::new(),
                    });
                }
                b"PLTE" => {
                    palette = vec![0u8; len as usize];
                    self.read_exact(&mut palette)?;
                    self.skip(4)?;
                }
                b"tRNS" => {
                    transparency = vec![0u8; len as usize];
                    self.read_exact(&mut transparency)?;
                    self.skip(4)?;
                }
                b"IDAT" => {
                    self.remaining = len;
                    let mut format = format.context("PNG is missing its IHDR chunk")?;
                    if format.color == ColorType::Indexed && palette.is_empty() {
                        bail!("indexed PNG is missing its palette");
                    }
                    format.palette = palette;
                    format.transparency = transparency;
                    return Ok((width, height, format));
                }
                b"IEND" => bail!("PNG contains no image data"),
                _ => self.skip(u64::from(len) + 4)?,
            }
        }
    }

    /// Fill `buf` with the next slice of IDAT payload; 0 means end of image
    /// data.
    fn read_idat(&mut self, buf: &mut [u8]) -> Result<usize> {
        loop {
            if self.done {
                return Ok(0);
            }
            if self.remaining == 0 {
                let mut header = [0u8; 8];
                self.read_exact(&mut header)?;
                let len = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
                match &[header[4], header[5], header[6], header[7]] {
                    b"IDAT" => self.remaining = len,
                    b"IEND" => {
                        self.done = true;
                        return Ok(0);
                    }
                    _ => self.skip(u64::from(len) + 4)?,
                }
                continue;
            }

            let n = buf.len().min(self.remaining as usize);
            self.read_exact(&mut buf[..n])?;
            self.remaining -= n as u32;
            if self.remaining == 0 {
                self.skip(4)?; // CRC
            }
            return Ok(n);
        }
    }
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// Decode `src`, handing every scanline to `sink` as it becomes available.
///
/// Peak allocation is two scanline buffers; the 32 KiB inflate window and the
/// inflate state live in static memory.
pub fn decode<R: Read>(src: R, sink: &mut dyn RowSink) -> Result<()> {
    let mut chunks = Chunks::new(src);
    let (width, height, format) = chunks.read_header()?;
    sink.start(width, height)?;

    let scratch = ScratchGuard::acquire()?;
    let mut rows = Rows::new(width, height, format, sink);

    let mut in_buf = [0u8; IN_CHUNK];
    let mut in_pos = 0usize;
    let mut in_len = 0usize;
    let mut input_done = false;
    let mut out_pos = 0usize;

    loop {
        if in_pos == in_len && !input_done {
            in_len = chunks.read_idat(&mut in_buf)?;
            in_pos = 0;
            input_done = in_len == 0;
        }

        let flags = TINFL_FLAG_PARSE_ZLIB_HEADER
            | TINFL_FLAG_IGNORE_ADLER32
            | if input_done {
                0
            } else {
                TINFL_FLAG_HAS_MORE_INPUT
            };

        let (status, consumed, produced) = decompress(
            scratch.decomp,
            &in_buf[in_pos..in_len],
            scratch.window,
            out_pos,
            flags,
        );
        in_pos += consumed;

        // Consume everything produced before the next call can wrap over it.
        let mut taken = 0usize;
        while taken < produced {
            let at = (out_pos + taken) & WINDOW_MASK;
            let run = (produced - taken).min(WINDOW - at);
            if !rows.feed(&scratch.window[at..at + run])? {
                return Ok(());
            }
            taken += run;
        }
        out_pos = (out_pos + produced) & WINDOW_MASK;

        match status {
            TINFLStatus::Done => break,
            TINFLStatus::HasMoreOutput | TINFLStatus::NeedsMoreInput => continue,
            other => bail!("PNG data stream is corrupt ({other:?})"),
        }
    }

    if rows.y < height {
        bail!("PNG ended after {} of {height} scanlines", rows.y);
    }
    Ok(())
}

/// Reassembles scanlines from the inflated byte stream and unfilters them.
struct Rows<'a> {
    height: u32,
    format: Format,
    sink: &'a mut dyn RowSink,
    /// Filter byte plus `stride` data bytes for the row being assembled.
    current: Vec<u8>,
    previous: Vec<u8>,
    filled: usize,
    y: u32,
}

impl<'a> Rows<'a> {
    fn new(width: u32, height: u32, format: Format, sink: &'a mut dyn RowSink) -> Self {
        let stride = format.stride(width);
        Self {
            height,
            format,
            sink,
            current: vec![0u8; stride + 1],
            previous: vec![0u8; stride],
            filled: 0,
            y: 0,
        }
    }

    /// Absorb inflated bytes, emitting rows as they complete. Returns false
    /// once the sink (or the image height) says we are done.
    fn feed(&mut self, mut data: &[u8]) -> Result<bool> {
        while !data.is_empty() {
            if self.y >= self.height {
                return Ok(false);
            }
            let want = self.current.len() - self.filled;
            let n = want.min(data.len());
            self.current[self.filled..self.filled + n].copy_from_slice(&data[..n]);
            self.filled += n;
            data = &data[n..];

            if self.filled == self.current.len() {
                self.filled = 0;
                if !self.emit_row()? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    fn emit_row(&mut self) -> Result<bool> {
        let filter = self.current[0];
        unfilter(
            filter,
            self.format.filter_bpp(),
            &mut self.current[1..],
            &self.previous,
        )?;

        let keep_going = {
            let row = Row {
                data: &self.current[1..],
                format: &self.format,
            };
            self.sink.row(self.y, &row)?
        };

        self.previous.copy_from_slice(&self.current[1..]);
        self.y += 1;
        Ok(keep_going && self.y < self.height)
    }
}

/// Reverse one of the five PNG scanline filters, in place.
fn unfilter(filter: u8, bpp: usize, row: &mut [u8], prev: &[u8]) -> Result<()> {
    match filter {
        0 => {}
        1 => {
            for i in bpp..row.len() {
                row[i] = row[i].wrapping_add(row[i - bpp]);
            }
        }
        2 => {
            for i in 0..row.len() {
                row[i] = row[i].wrapping_add(prev[i]);
            }
        }
        3 => {
            for i in 0..row.len() {
                let left = if i >= bpp { u16::from(row[i - bpp]) } else { 0 };
                let above = u16::from(prev[i]);
                row[i] = row[i].wrapping_add(((left + above) / 2) as u8);
            }
        }
        4 => {
            for i in 0..row.len() {
                let left = if i >= bpp { row[i - bpp] } else { 0 };
                let up_left = if i >= bpp { prev[i - bpp] } else { 0 };
                row[i] = row[i].wrapping_add(paeth(left, prev[i], up_left));
            }
        }
        other => bail!("unknown PNG row filter {other}"),
    }
    Ok(())
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let pa = (p - i16::from(a)).abs();
    let pb = (p - i16::from(b)).abs();
    let pc = (p - i16::from(c)).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}
