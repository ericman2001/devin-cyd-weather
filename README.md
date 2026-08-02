# CYD Weather Station

Weather-station firmware for the **Cheap Yellow Display** (ESP32-2432S028R): a
$10-ish board with an ESP32-WROOM-32, a 2.8" 240x320 ILI9341 SPI display, and an
XPT2046 resistive touchscreen.

Written in **Rust (std) on top of ESP-IDF** using `esp-idf-svc` / `esp-idf-hal`.

Hardware reference:
<https://randomnerdtutorials.com/cheap-yellow-display-esp32-2432s028r/>

## Features

- On-screen, **touch-driven Wi-Fi setup** — scan for networks, pick one, and
  type the password on an on-screen QWERTY keyboard.
- Selectable **Wi-Fi security type** during setup — *Auto* (WPA2/WPA3, the
  default), **WPA/WPA2 Personal** (fixes older APs that timed out under the
  WPA2/WPA3 default), WPA2 Personal, WPA2/WPA3 Personal, or Open.
- Toggleable **serial/USB debug logging** — enable or disable verbose
  connect-path logging during setup; the choice is persisted.
- Optional **manual latitude/longitude** override (entered via an on-screen
  numeric keypad) in case IP geolocation is inaccurate.
- Credentials, security type, debug-logging preference, and location persisted
  in **NVS flash**, so setup only happens once.
- **IP geolocation** (via `ip-api.com`) when no manual location is set.
- Current conditions + **4-day forecast** from **Open-Meteo**, plus **US AQI**
  from the Open-Meteo air-quality API, in American customary units
  (°F, mph, inches).
- Weather dashboard rendered with `embedded-graphics`, with drawn weather icons
  and graceful error/status screens.
- Refreshes every **30 minutes**, keeping the last good reading on transient
  network failures and showing a retry banner.
- **Animated radar slideshow** on a second screen, reachable from the bottom
  toolbar, built on a fully streaming SD-card pipeline (see below).

## Hardware pin mapping (ESP32-2432S028R)

| Function        | GPIO | Bus            |
|-----------------|------|----------------|
| TFT SCLK        | 14   | SPI2 (display) |
| TFT MOSI        | 13   | SPI2           |
| TFT MISO        | 12   | SPI2           |
| TFT CS          | 15   | SPI2           |
| TFT DC          | 2    | SPI2           |
| TFT backlight   | 21   | GPIO out       |
| TFT reset       | —    | tied to EN     |
| Touch T_CLK     | 25   | software SPI   |
| Touch T_MOSI    | 32   | software SPI   |
| Touch T_MISO    | 39   | software SPI (in only) |
| Touch T_CS      | 33   | software SPI   |
| Touch T_IRQ     | 36   | GPIO in only   |
| SD SCK          | 18   | SPI3 (SD card) |
| SD MOSI         | 23   | SPI3           |
| SD MISO         | 19   | SPI3           |
| SD CS           | 5    | SPI3           |

> **Three SPI devices, two SPI hosts.** The ESP32 exposes only SPI2 and SPI3
> for general use, and the CYD has three SPI peripherals. The display keeps
> SPI2 and the SD card takes SPI3, so the XPT2046 touch controller is driven by
> a **bit-banged mode-0 SPI** in `src/touch.rs` (it tops out at 2 MHz anyway,
> so nothing is lost). The SD pins above are the standard CYD micro-SD slot on
> the back of the board — verify them against your board revision before
> concluding the radar screen is broken.

> **CYD variant note:** the two-USB "CYD2USB" revision ships an **ST7789**
> controller with the backlight on **GPIO 27** (not 21). If your board shows a
> blank/white screen, switch the model in `src/display.rs` to `mipidsi`'s
> `ST7789` and change the backlight pin in `src/main.rs` to `gpio27`.

Pin assignments and API URLs/refresh cadence are centralized as constants in
`src/config.rs` and the `display`/`touch` module headers for easy tuning.

## Prerequisites

This is an Xtensa ESP32 target, so it needs the Espressif Rust toolchain rather
than plain `rustup`:

```bash
# 1. Install the ESP Rust toolchain manager and the toolchain itself
cargo install espup --locked
espup install
# espup writes an env file you must source in every new shell:
. $HOME/export-esp.sh          # (Windows: . $HOME/export-esp.ps1)

# 2. Install the flashing + link tools
cargo install ldproxy espflash cargo-espflash --locked
```

The first build also clones and compiles ESP-IDF (v5.2.2, pinned in
`.cargo/config.toml`); expect it to take several minutes and require network
access.

## Build

```bash
. $HOME/export-esp.sh
cargo build --release
```

## Flash

Connect the board via its **micro-USB (data) port** — this is the CH340
USB-serial bridge. A charge-only cable, or the USB-C power port on 2-USB
variants, will *not* enumerate a serial device.

```bash
./flash.sh                 # auto-detect port, build + flash + monitor (release)
./flash.sh /dev/ttyUSB0    # explicit port
BAUD=921600 ./flash.sh     # override baud
```

On Windows (PowerShell):

```powershell
./flash.ps1                 # auto-detect
./flash.ps1 -Port COM5      # explicit port
./flash.ps1 -Baud 921600
```

Both scripts wrap `cargo espflash flash --release --monitor`.

### Partition table

The firmware is ~1.35 MB, which does not fit the default 1 MB app partition, so
this project ships a custom `partitions.csv` (3 MB app, plus NVS) and an
`espflash.toml` that points `espflash`/`cargo espflash` at it (and sets the
4 MB flash size). Both are picked up automatically — no extra flags needed.

## On-screen Wi-Fi setup flow

On first boot (no saved credentials) the device shows the setup UI:

1. **Select Wi-Fi** — tap a network from the scanned list (use *Up*/*Down* to
   page, or *Manual* to type an SSID by hand).
2. **Password** — type it on the on-screen QWERTY keyboard (`Sh` toggles case,
   `<-` backspaces, `OK` confirms).
3. **Wi-Fi security** — tap the security type for your network: *Auto*
   (WPA2/WPA3, works for most), **WPA/WPA2 Personal** (older/mixed-mode APs
   that would otherwise time out), *WPA2 Personal*, *WPA2/WPA3 Personal*, or
   *Open*.
4. **Serial debug** — choose *Enable* or *Disable* to turn USB serial debug
   logging on or off (helpful when diagnosing Wi-Fi connection issues over the
   USB monitor).
5. **Manual location?** — choose *Yes* to enter latitude/longitude on a numeric
   keypad, or *No* to use automatic IP geolocation.

The settings are saved to NVS and the device connects and starts displaying
weather.

## Resetting saved config

**Touch and hold the screen while the board boots** (~1.5 s). The stored
credentials/location are erased and the setup UI is shown again. If a saved
network later fails to connect, the device also drops back into setup and waits
for a tap.

## Radar slideshow (SD card required)

The second screen animates recent + nowcast precipitation radar for your
location. It needs a **FAT32-formatted micro-SD card** in the slot on the back
of the board; without one the firmware logs a warning, keeps working, and the
radar screen reports that no card is present. The firmware never formats the
card, so existing data is safe.

Tap the bottom toolbar to switch between **Weather** and **Radar**. (When the
backlight is off, the first tap only wakes the screen.) On the radar screen the
staged frames cycle every 600 ms and are re-downloaded when older than 10
minutes; the animation pauses whenever the backlight is off. The 30-minute
weather refresh and the tap-activated backlight behave exactly as before.

### Memory-conscious streaming pipeline

A single decoded 256x256 RGBA radar tile is 256 KB — far more than the ESP32
can spare while Wi-Fi and TLS are up — so no stage ever holds a whole image:

1. **Download -> SD.** `radar::download_tiles` / `http::get_to_writer` stream
   the HTTP body straight into a file on the card through a 512-byte buffer.
2. **Row-wise decode -> SD.** `radar::decode_tiles` seeds each
   `/sdcard/radar/frame_{i}.rgb565` from the cached basemap frame, then decodes
   every tile of that frame one scanline at a time, blending each row straight
   into its sub-rectangle of the file (read-modify-write at a byte offset).
   Only a couple of rows are in RAM.
3. **Stream -> display.** `radar::blit_frame` reads a staged frame back in
   4-row bands and pushes each band into a `mipidsi` address window, so the
   panel is fed from the SD card with no framebuffer. This path bypasses the
   `embedded-graphics` vector rendering used by `src/ui.rs`.

Frame files are raw Rgb565 prefixed with an 8-byte header (`R565`, then width
and height as little-endian `u16`s).

The decoder is `src/png_stream.rs` rather than the `png` crate. Inflating needs
a 32 KiB sliding window, and `png` allocates several buffers that size on the
heap; with Wi-Fi up those allocations fail, and a failed allocation *aborts* the
firmware instead of returning an error. `png_stream` drives `miniz_oxide`'s
inflate core over a window and inflate state reserved statically in `.bss`, so
decoding costs no heap beyond two scanline buffers and cannot OOM. It handles
non-interlaced 8-bit greyscale/RGB/RGBA and 1/2/4/8-bit palette (with `tRNS`)
images — the shapes radar tiles come in.

### Basemap and location marker

Radar alone is hard to read, so each frame is composited over a basemap
(`config::BASEMAP_TILE_URL`, CARTO's dark style rendered from OpenStreetMap
data) — coastlines, roads and place labels. The basemap is composed once into
`/sdcard/radar/base_{z}_{x0}_{y0}.rgb565` and reused as the seed for every
radar frame, so it costs one file copy rather than a framebuffer; basemaps
composed for a previous view are deleted. If the download fails the radar still
renders, just over a flat background. A crosshair marks the configured
location, and the status line carries the `RainViewer / OSM / CARTO`
attribution.

### View geometry

`radar::View` is a 240x240 window in slippy-map *pixel* space centred on the
configured position, so the viewer is in the middle of the screen rather than
wherever they happen to fall inside one tile. That window normally straddles a
2x2 block of tiles: `View::tiles` intersects it with each tile and yields a
crop plus a destination offset, and every frame is assembled from those (up to
four downloads and four decodes per frame, plus a one-off set for the basemap).
`RadarSource` therefore returns URL *templates* containing `{z}`/`{x}`/`{y}`
rather than finished URLs.

Heap headroom is logged around both phases (`heap[before download]`,
`heap[before decode]`, `heap[after decode]`), and `sdkconfig.defaults` enables
mbedTLS dynamic buffers and trims the Wi-Fi buffer pools to keep the margin.

### Changing the radar source

Open-Meteo does not serve radar imagery, so the tile source is pluggable:
implement `radar::RadarSource` (one method returning tile URLs, oldest first)
and pass it to `radar::download_tiles`. The bundled implementation is
`radar::RainViewer`. Zoom level, colour scheme, frame count, dwell time and the
refresh interval are constants in `src/config.rs`.

## Touch calibration

Raw XPT2046 ADC values are mapped to screen coordinates by `Calibration` in
`src/touch.rs`. The defaults suit a typical CYD panel; if taps land in the wrong
spot, adjust `x_min/x_max/y_min/y_max` and the `swap_xy`/`invert_x`/`invert_y`
flags there.

## Project layout

| File | Responsibility |
|------|----------------|
| `src/main.rs` | Boot flow + 30-minute refresh loop |
| `src/config.rs` | NVS-backed config + tunable constants |
| `src/display.rs` | ILI9341 (`mipidsi`) SPI init + backlight |
| `src/touch.rs` | XPT2046 bit-banged SPI reader + calibration |
| `src/storage.rs` | SD card (SDSPI) + FAT filesystem mounted at `/sdcard` |
| `src/radar.rs` | Radar tile sources, row-wise decoder, frame streaming |
| `src/png_stream.rs` | Heap-free scanline PNG reader (static 32 KiB inflate window) |
| `src/wifi.rs` | Station scan/connect |
| `src/provisioning.rs` | Touch setup UI (list, keyboard, keypad) |
| `src/location.rs` | IP geolocation |
| `src/weather.rs` | Open-Meteo forecast + AQI, WMO code mapping |
| `src/ui.rs` | Weather dashboard, radar chrome, toolbar, status/error rendering |
| `src/http.rs` | HTTPS GET helpers (buffered + streaming, mbedTLS cert bundle) |

## License

GPL-3.0-only. See [LICENSE](LICENSE).
