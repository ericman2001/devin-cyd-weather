# CYD Weather Station

Weather-station firmware for the **Cheap Yellow Display** (ESP32-2432S028R): a
$10-ish board with an ESP32-WROOM-32, a 2.8" 240x320 ILI9341 SPI display, and an
XPT2046 resistive touchscreen.

Written in **Rust (std) on top of ESP-IDF** using `esp-idf-svc` / `esp-idf-hal`.

Hardware reference:
<https://randomnerdtutorials.com/cheap-yellow-display-esp32-2432s028r/>

## Features

- On-screen, **touch-driven Wi-Fi setup** — scan for networks, pick one, and
  type the password on an on-screen QWERTY keyboard. Supports WPA2/WPA3-hybrid
  networks (connects with `WPA2WPA3Personal`).
- Optional **manual latitude/longitude** override (entered via an on-screen
  numeric keypad) in case IP geolocation is inaccurate.
- Credentials + location persisted in **NVS flash**, so setup only happens once.
- **IP geolocation** (via `ip-api.com`) when no manual location is set.
- Current conditions + **4-day forecast** from **Open-Meteo**, plus **US AQI**
  from the Open-Meteo air-quality API, in American customary units
  (°F, mph, inches).
- Weather dashboard rendered with `embedded-graphics`, with drawn weather icons
  and graceful error/status screens.
- Refreshes every **30 minutes**, keeping the last good reading on transient
  network failures and showing a retry banner.

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
| Touch T_CLK     | 25   | SPI3 (touch)   |
| Touch T_MOSI    | 32   | SPI3           |
| Touch T_MISO    | 39   | SPI3 (in only) |
| Touch T_CS      | 33   | SPI3           |
| Touch T_IRQ     | 36   | GPIO in only   |

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
3. **Manual location?** — choose *Yes* to enter latitude/longitude on a numeric
   keypad, or *No* to use automatic IP geolocation.

The settings are saved to NVS and the device connects and starts displaying
weather.

## Resetting saved config

**Touch and hold the screen while the board boots** (~1.5 s). The stored
credentials/location are erased and the setup UI is shown again. If a saved
network later fails to connect, the device also drops back into setup and waits
for a tap.

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
| `src/touch.rs` | XPT2046 SPI reader + calibration |
| `src/wifi.rs` | Station scan/connect |
| `src/provisioning.rs` | Touch setup UI (list, keyboard, keypad) |
| `src/location.rs` | IP geolocation |
| `src/weather.rs` | Open-Meteo forecast + AQI, WMO code mapping |
| `src/ui.rs` | Weather dashboard + status/error rendering |
| `src/http.rs` | HTTPS GET helper (mbedTLS cert bundle) |

## License

GPL-3.0-only. See [LICENSE](LICENSE).
