//! CYD Weather Station firmware entry point.
//!
//! Boot flow:
//!   1. bring up the display + touch panel,
//!   2. load stored config (or run the on-screen provisioning UI if there are
//!      no saved credentials, or the screen is being touched at boot to reset),
//!   3. mount the SD card used to stage radar frames,
//!   4. connect to Wi-Fi and resolve our location,
//!   5. refresh weather + air quality every 30 minutes, keeping the last good
//!      data on transient failures, while the bottom toolbar switches between
//!      the weather summary and the radar slideshow.

mod config;
mod display;
mod http;
mod location;
mod png_stream;
mod provisioning;
mod radar;
mod storage;
mod touch;
mod ui;
mod weather;
mod wifi;

use std::time::Instant;

use anyhow::{Context, Result};
use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

use crate::config::{
    ConfigStore, StoredConfig, BACKLIGHT_ON_SECS, RADAR_ATTRIBUTION, RADAR_FRAME_MS,
    RADAR_REFRESH_SECS, REFRESH_INTERVAL_SECS, RETRY_INTERVAL_SECS,
};
use crate::display::{Backlight, CydDisplay};
use crate::location::Location;
use crate::radar::RainViewer;
use crate::touch::{Calibration, Touch};
use crate::ui::Screen;
use crate::weather::WeatherData;
use crate::wifi::Wifi;

fn main() -> Result<()> {
    // Required for linking the ESP-IDF runtime patches.
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    log::info!("CYD Weather Station starting");

    let peripherals = Peripherals::take().context("failed to take peripherals")?;
    let sysloop = EspSystemEventLoop::take().context("failed to take system event loop")?;
    let nvs_part = EspDefaultNvsPartition::take().context("failed to take NVS partition")?;

    let pins = peripherals.pins;

    // -- Display (SPI2 / HSPI) ---------------------------------------------
    let (mut disp, mut backlight) = display::init(
        peripherals.spi2,
        pins.gpio14.degrade_input_output(), // SCLK
        pins.gpio13.degrade_input_output(), // MOSI
        pins.gpio12.degrade_input_output(), // MISO
        pins.gpio15.degrade_output(),       // CS
        pins.gpio2.degrade_output(),        // DC
        pins.gpio21.degrade_output(),       // Backlight
    )?;

    // -- Touch (bit-banged SPI; the hardware hosts are taken by the display
    //    and the SD card) ----------------------------------------------------
    let mut touch = Touch::init(
        pins.gpio25.degrade_output(), // T_CLK
        pins.gpio32.degrade_output(), // T_MOSI
        pins.gpio39.degrade_input(),  // T_MISO (input only)
        pins.gpio33.degrade_output(), // T_CS
        pins.gpio36.degrade_input(),  // T_IRQ (input only)
        Calibration::default(),
    )?;

    ui::draw_status(&mut disp, "CYD Weather", "Starting...").ok();

    // -- SD card (SPI3 / VSPI) ---------------------------------------------
    // Optional: without a card the weather screen still works, only the radar
    // slideshow (which stages its frames on the card) is unavailable.
    let _storage = match storage::Storage::mount(
        peripherals.spi3,
        pins.gpio18.degrade_input_output(), // SD SCK
        pins.gpio23.degrade_input_output(), // SD MOSI
        pins.gpio19.degrade_input_output(), // SD MISO
        pins.gpio5.degrade_output(),        // SD CS
    ) {
        Ok(storage) => Some(storage),
        Err(e) => {
            log::warn!("SD card unavailable: {e:#}; the radar screen is disabled");
            None
        }
    };
    let sd_ready = _storage.is_some();

    // -- Configuration ------------------------------------------------------
    let mut store = ConfigStore::new(nvs_part.clone())?;
    let mut cfg = store.load().unwrap_or_default();
    // `initialize_default()` above runs before config is available, so re-apply
    // the desired verbosity now that we know the user's preference.
    apply_log_level(cfg.serial_debug);

    // Touch-and-hold at boot forces re-provisioning.
    if touch_held_at_boot(&mut touch) {
        log::info!("touch held at boot -> resetting configuration");
        let _ = store.clear();
        cfg = StoredConfig::default();
    }

    // -- Wi-Fi --------------------------------------------------------------
    let mut wifi = Wifi::new(peripherals.modem, sysloop, nvs_part)?;

    if !cfg.has_wifi() {
        cfg = provision(&mut disp, &mut touch, &mut wifi, &mut store)?;
        apply_log_level(cfg.serial_debug);
    }

    // Connect, falling back to provisioning if the saved credentials fail.
    loop {
        let ssid = cfg.ssid.clone().unwrap_or_default();
        log::info!(
            "attempting Wi-Fi connect to {ssid:?} using auth {}",
            cfg.auth_method.label()
        );
        ui::draw_status(&mut disp, "Connecting", &ssid).ok();
        match wifi.connect(
            &ssid,
            cfg.password.as_deref().unwrap_or(""),
            cfg.auth_method,
        ) {
            Ok(()) => {
                log::info!("connected, ip = {:?}", wifi.ip_info());
                break;
            }
            Err(e) => {
                log::warn!("connect to {ssid:?} failed: {e:#}");
                ui::draw_status(&mut disp, "Wi-Fi failed", "Tap to reconfigure").ok();
                wait_for_touch(&mut touch);
                cfg = provision(&mut disp, &mut touch, &mut wifi, &mut store)?;
                apply_log_level(cfg.serial_debug);
            }
        }
    }

    // -- Location -----------------------------------------------------------
    let location = resolve_location(&mut disp, &cfg);

    // -- Refresh loop -------------------------------------------------------
    let mut devices = Devices {
        disp: &mut disp,
        touch: &mut touch,
        backlight: &mut backlight,
        wifi: &mut wifi,
    };
    run_refresh_loop(&mut devices, &mut cfg, location, sd_ready)
}

/// The peripherals the refresh loop and the screens share.
struct Devices<'a> {
    disp: &'a mut CydDisplay,
    touch: &'a mut Touch,
    backlight: &'a mut Backlight,
    wifi: &'a mut Wifi,
}

/// Poll the touch panel briefly at boot; returns true if held for ~1.5s.
fn touch_held_at_boot(touch: &mut Touch) -> bool {
    const REQUIRED_MS: u32 = 1500;
    const STEP_MS: u32 = 50;
    let mut held = 0;
    for _ in 0..(REQUIRED_MS / STEP_MS) {
        match touch.read() {
            Ok(Some(_)) => held += STEP_MS,
            _ => return false,
        }
        FreeRtos::delay_ms(STEP_MS);
    }
    held >= REQUIRED_MS
}

/// Apply the serial/USB debug logging preference by adjusting the global log
/// max level. When enabled we allow `Debug` and below; when disabled logging
/// is turned `Off`.
fn apply_log_level(serial_debug: bool) {
    let level = if serial_debug {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Off
    };
    log::set_max_level(level);
    log::info!(
        "serial debug logging {} (max level {level})",
        if serial_debug { "enabled" } else { "disabled" }
    );
}

fn wait_for_touch(touch: &mut Touch) {
    loop {
        if matches!(touch.read(), Ok(Some(_))) {
            return;
        }
        FreeRtos::delay_ms(30);
    }
}

/// Run the on-screen provisioning flow, scan first, then persist the result.
fn provision(
    disp: &mut CydDisplay,
    touch: &mut Touch,
    wifi: &mut Wifi,
    store: &mut ConfigStore,
) -> Result<StoredConfig> {
    ui::draw_status(disp, "Wi-Fi setup", "Scanning...").ok();
    let ssids = wifi.scan_ssids().unwrap_or_default();

    let mut prov = provisioning::Provisioner::new(disp, touch);
    let cfg = prov.run(&ssids)?;

    if let Err(e) = store.save(&cfg) {
        log::warn!("failed to persist config: {e:#}");
    }
    Ok(cfg)
}

/// Resolve the location: manual override wins, otherwise IP geolocation.
fn resolve_location(disp: &mut CydDisplay, cfg: &StoredConfig) -> Location {
    if let Some((lat, lon)) = cfg.manual_location {
        return Location {
            latitude: lat,
            longitude: lon,
            label: None,
        };
    }
    ui::draw_status(disp, "Locating", "Resolving position...").ok();
    match location::resolve_from_ip() {
        Ok(loc) => loc,
        Err(e) => {
            log::warn!("geolocation failed: {e:#}; using fallback (Austin, TX)");
            // Fallback so the UI still shows something useful.
            Location {
                latitude: 30.2672,
                longitude: -97.7431,
                label: Some("Austin, TX (fallback)".into()),
            }
        }
    }
}

/// Everything the interactive wait needs to keep the screens in sync.
struct AppState {
    screen: Screen,
    /// Remaining time the tap-activated backlight should stay lit, in ms.
    /// Kept across refresh boundaries so the 60s window survives a refresh.
    remaining_on_ms: u64,
    /// Set while the previous touch poll saw a press, so a held finger only
    /// produces one tap event.
    touch_down: bool,
    radar: RadarState,
}

/// Radar slideshow bookkeeping.
struct RadarState {
    source: RainViewer,
    /// False when no SD card is mounted; the radar screen then just says so.
    sd_ready: bool,
    frames: usize,
    index: usize,
    /// Time left on the current frame's dwell, in ms.
    dwell_ms: u64,
    staged_at: Option<Instant>,
}

impl RadarState {
    fn is_stale(&self) -> bool {
        match self.staged_at {
            Some(at) => at.elapsed().as_secs() >= RADAR_REFRESH_SECS,
            None => true,
        }
    }
}

fn run_refresh_loop(
    dev: &mut Devices,
    cfg: &mut StoredConfig,
    location: Location,
    sd_ready: bool,
) -> Result<()> {
    let mut last_good: Option<WeatherData> = None;

    // The config/boot screens are shown with the backlight lit (display::init
    // turns it on). The weather display is dark by default, so switch the
    // backlight off exactly once as we transition into the refresh loop.
    let _ = dev.backlight.off();

    let mut state = AppState {
        screen: Screen::Weather,
        remaining_on_ms: 0,
        touch_down: false,
        radar: RadarState {
            source: RainViewer::default(),
            sd_ready,
            frames: 0,
            index: 0,
            dwell_ms: 0,
            staged_at: None,
        },
    };

    loop {
        ensure_connected(dev.disp, dev.wifi, cfg);

        let sleep_secs = match weather::fetch(location.latitude, location.longitude) {
            Ok(data) => {
                last_good = Some(data);
                if state.screen == Screen::Weather {
                    draw_weather_screen(dev.disp, last_good.as_ref(), &location, false);
                }
                REFRESH_INTERVAL_SECS
            }
            Err(e) => {
                log::warn!("weather fetch failed: {e:#}");
                if state.screen == Screen::Weather {
                    draw_weather_screen(dev.disp, last_good.as_ref(), &location, true);
                }
                RETRY_INTERVAL_SECS
            }
        };

        wait_with_backlight(dev, &mut state, last_good.as_ref(), &location, sleep_secs);
    }
}

/// Draw the weather summary plus the navigation toolbar, optionally with the
/// "stale data" banner over it.
fn draw_weather_screen(
    disp: &mut CydDisplay,
    data: Option<&WeatherData>,
    location: &Location,
    failed: bool,
) {
    match data {
        Some(data) => {
            ui::draw_weather(disp, data, location.label.as_deref()).ok();
            if failed {
                ui::draw_error_banner(disp, "Update failed - retrying").ok();
            }
        }
        None if failed => {
            ui::draw_status(disp, "API error", "Retrying shortly...").ok();
        }
        None => {
            ui::draw_status(disp, "CYD Weather", "Loading...").ok();
        }
    }
    ui::draw_toolbar(disp, Screen::Weather).ok();
}

/// Enter the radar screen: (re-)stage the frames if they are missing or stale,
/// then show the first one.
fn enter_radar_screen(dev: &mut Devices, state: &mut AppState, location: &Location) {
    ui::draw_radar_chrome(dev.disp, "Radar", "Loading...").ok();
    ui::draw_toolbar(dev.disp, Screen::Radar).ok();

    if !state.radar.sd_ready {
        ui::draw_radar_status(dev.disp, "No SD card - radar unavailable").ok();
        return;
    }

    if state.radar.is_stale() {
        match refresh_radar(dev, state, location) {
            Ok(frames) => {
                state.radar.frames = frames;
                state.radar.staged_at = Some(Instant::now());
            }
            Err(e) => {
                log::warn!("radar refresh failed: {e:#}");
                state.radar.frames = radar::staged_frames();
            }
        }
    }

    if state.radar.frames == 0 {
        ui::draw_radar_status(dev.disp, "Radar unavailable").ok();
        return;
    }

    state.radar.index = 0;
    state.radar.dwell_ms = 0;
    show_radar_frame(dev.disp, state);
}

/// Download the radar tiles, then decode them into displayable frames.
fn refresh_radar(dev: &mut Devices, state: &mut AppState, location: &Location) -> Result<usize> {
    let tiles = radar::download_tiles(&state.radar.source, location.latitude, location.longitude)?;
    ui::draw_radar_status(dev.disp, "Decoding...").ok();
    radar::decode_tiles(tiles, location.latitude, location.longitude)
}

/// Stream the current radar frame from the SD card onto the panel.
fn show_radar_frame(disp: &mut CydDisplay, state: &mut AppState) {
    let index = state.radar.index;
    let path = radar::frame_path(index);
    if let Err(e) = radar::blit_frame(disp, &path, 0, ui::RADAR_VIEW_TOP as u16) {
        log::warn!("failed to show radar frame {index}: {e:#}");
        ui::draw_radar_status(disp, "Frame read failed").ok();
        state.radar.frames = 0;
        return;
    }
    let label = format!(
        "Frame {}/{}  {RADAR_ATTRIBUTION}",
        index + 1,
        state.radar.frames
    );
    ui::draw_radar_status(disp, &label).ok();
}

/// Wait for `total` seconds while servicing the tap-activated backlight.
///
/// The backlight timer is a *saturating countdown* (`remaining_on_ms`), never
/// an absolute `activation_ms + threshold` deadline. An absolute deadline can
/// overflow/wrap if computed near the counter's max value and, in a `--release`
/// build, Rust wraps silently rather than panicking -- which would leave the
/// backlight stuck ON (fail-unsafe). Here the only timer arithmetic is
/// subtraction saturating toward zero, so the fail-safe direction is always
/// "backlight off": it can never get stuck on. All counters are `u64` ms
/// (~584M years to overflow) rather than `u32` (~49.7 days, reachable on an
/// always-on device).
fn wait_with_backlight(
    dev: &mut Devices,
    state: &mut AppState,
    last_good: Option<&WeatherData>,
    location: &Location,
    total_secs: u64,
) {
    const CHUNK_MS: u64 = 100;
    let mut sleep_remaining_ms = total_secs.saturating_mul(1000);

    while sleep_remaining_ms > 0 {
        let point = dev.touch.read().unwrap_or_default();
        // Only the press edge counts, so a resting finger cannot flip screens
        // repeatedly.
        let tap = match (point, state.touch_down) {
            (Some(p), false) => Some(p),
            _ => None,
        };
        state.touch_down = point.is_some();

        if let Some(p) = tap {
            if state.remaining_on_ms == 0 {
                // The screen is dark: the first tap only wakes it.
                if dev.backlight.on().is_ok() {
                    state.remaining_on_ms = BACKLIGHT_ON_SECS * 1000;
                }
            } else if let Some(target) = ui::toolbar_hit(p) {
                if target != state.screen {
                    state.screen = target;
                    match target {
                        Screen::Weather => {
                            draw_weather_screen(dev.disp, last_good, location, false)
                        }
                        Screen::Radar => enter_radar_screen(dev, state, location),
                    }
                }
            }
        }

        // Animate the slideshow only while the panel is actually lit.
        if state.screen == Screen::Radar && state.remaining_on_ms > 0 && state.radar.frames > 0 {
            state.radar.dwell_ms = state.radar.dwell_ms.saturating_sub(CHUNK_MS);
            if state.radar.dwell_ms == 0 {
                state.radar.index = (state.radar.index + 1) % state.radar.frames;
                show_radar_frame(dev.disp, state);
                state.radar.dwell_ms = RADAR_FRAME_MS;
            }
        }

        FreeRtos::delay_ms(CHUNK_MS as u32);

        if state.remaining_on_ms > 0 {
            state.remaining_on_ms = state.remaining_on_ms.saturating_sub(CHUNK_MS);
            if state.remaining_on_ms == 0 {
                let _ = dev.backlight.off();
            }
        }

        sleep_remaining_ms = sleep_remaining_ms.saturating_sub(CHUNK_MS);
    }
}

/// Reconnect if the link dropped between refreshes.
fn ensure_connected(disp: &mut CydDisplay, wifi: &mut Wifi, cfg: &mut StoredConfig) {
    if wifi.is_connected() {
        return;
    }
    log::info!("link down, reconnecting");
    ui::draw_status(disp, "Reconnecting", "Wi-Fi link lost").ok();
    let ssid = cfg.ssid.clone().unwrap_or_default();
    log::info!(
        "reconnecting to {ssid:?} using auth {}",
        cfg.auth_method.label()
    );
    match wifi.connect(
        &ssid,
        cfg.password.as_deref().unwrap_or(""),
        cfg.auth_method,
    ) {
        Ok(()) => log::info!("reconnected to {ssid:?}"),
        Err(e) => log::warn!("reconnect failed: {e:#}"),
    }
}
