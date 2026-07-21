//! CYD Weather Station firmware entry point.
//!
//! Boot flow:
//!   1. bring up the display + touch panel,
//!   2. load stored config (or run the on-screen provisioning UI if there are
//!      no saved credentials, or the screen is being touched at boot to reset),
//!   3. connect to Wi-Fi and resolve our location,
//!   4. refresh weather + air quality every 30 minutes, keeping the last good
//!      data on transient failures.

mod config;
mod display;
mod http;
mod location;
mod provisioning;
mod touch;
mod ui;
mod weather;
mod wifi;

use anyhow::{Context, Result};
use esp_idf_hal::delay::FreeRtos;
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

use crate::config::{ConfigStore, StoredConfig, REFRESH_INTERVAL_SECS, RETRY_INTERVAL_SECS};
use crate::display::CydDisplay;
use crate::location::Location;
use crate::touch::{Calibration, Touch};
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
    let (mut disp, _backlight) = display::init(
        peripherals.spi2,
        pins.gpio14.degrade_input_output(), // SCLK
        pins.gpio13.degrade_input_output(), // MOSI
        pins.gpio12.degrade_input_output(), // MISO
        pins.gpio15.degrade_output(),       // CS
        pins.gpio2.degrade_output(),        // DC
        pins.gpio21.degrade_output(),       // Backlight
    )?;

    // -- Touch (SPI3 / VSPI) -----------------------------------------------
    let mut touch = Touch::init(
        peripherals.spi3,
        pins.gpio25.degrade_input_output(), // T_CLK
        pins.gpio32.degrade_input_output(), // T_MOSI
        pins.gpio39.degrade_input(),        // T_MISO (input only)
        pins.gpio33.degrade_output(),       // T_CS
        pins.gpio36.degrade_input(),        // T_IRQ (input only)
        Calibration::default(),
    )?;

    ui::draw_status(&mut disp, "CYD Weather", "Starting...").ok();

    // -- Configuration ------------------------------------------------------
    let mut store = ConfigStore::new(nvs_part.clone())?;
    let mut cfg = store.load().unwrap_or_default();

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
    }

    // Connect, falling back to provisioning if the saved credentials fail.
    loop {
        let ssid = cfg.ssid.clone().unwrap_or_default();
        ui::draw_status(&mut disp, "Connecting", &ssid).ok();
        match wifi.connect(&ssid, cfg.password.as_deref().unwrap_or("")) {
            Ok(()) => {
                log::info!("connected, ip = {:?}", wifi.ip_info());
                break;
            }
            Err(e) => {
                log::warn!("connect failed: {e:#}");
                ui::draw_status(&mut disp, "Wi-Fi failed", "Tap to reconfigure").ok();
                wait_for_touch(&mut touch);
                cfg = provision(&mut disp, &mut touch, &mut wifi, &mut store)?;
            }
        }
    }

    // -- Location -----------------------------------------------------------
    let location = resolve_location(&mut disp, &cfg);

    // -- Refresh loop -------------------------------------------------------
    run_refresh_loop(
        &mut disp, &mut touch, &mut wifi, &mut store, &mut cfg, location,
    )
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

fn run_refresh_loop(
    disp: &mut CydDisplay,
    touch: &mut Touch,
    wifi: &mut Wifi,
    store: &mut ConfigStore,
    cfg: &mut StoredConfig,
    location: Location,
) -> Result<()> {
    let mut last_good: Option<WeatherData> = None;

    loop {
        ensure_connected(disp, touch, wifi, store, cfg);

        let sleep_secs = match weather::fetch(location.latitude, location.longitude) {
            Ok(data) => {
                ui::draw_weather(disp, &data, location.label.as_deref()).ok();
                last_good = Some(data);
                REFRESH_INTERVAL_SECS
            }
            Err(e) => {
                log::warn!("weather fetch failed: {e:#}");
                match &last_good {
                    Some(prev) => {
                        ui::draw_weather(disp, prev, location.label.as_deref()).ok();
                        ui::draw_error_banner(disp, "Update failed - retrying").ok();
                    }
                    None => {
                        ui::draw_status(disp, "API error", "Retrying shortly...").ok();
                    }
                }
                RETRY_INTERVAL_SECS
            }
        };

        sleep_seconds(sleep_secs);
    }
}

/// Reconnect if the link dropped between refreshes.
fn ensure_connected(
    disp: &mut CydDisplay,
    _touch: &mut Touch,
    wifi: &mut Wifi,
    _store: &mut ConfigStore,
    cfg: &mut StoredConfig,
) {
    if wifi.is_connected() {
        return;
    }
    log::info!("link down, reconnecting");
    ui::draw_status(disp, "Reconnecting", "Wi-Fi link lost").ok();
    let ssid = cfg.ssid.clone().unwrap_or_default();
    if let Err(e) = wifi.connect(&ssid, cfg.password.as_deref().unwrap_or("")) {
        log::warn!("reconnect failed: {e:#}");
    }
}

/// Sleep in short increments so the FreeRTOS watchdog stays happy.
fn sleep_seconds(total: u64) {
    let mut remaining = total;
    while remaining > 0 {
        let chunk = remaining.min(5);
        FreeRtos::delay_ms((chunk * 1000) as u32);
        remaining -= chunk;
    }
}
