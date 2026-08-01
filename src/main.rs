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

use crate::config::{
    ConfigStore, StoredConfig, BACKLIGHT_ON_SECS, REFRESH_INTERVAL_SECS, RETRY_INTERVAL_SECS,
};
use crate::display::{Backlight, CydDisplay};
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
    let (mut disp, mut backlight) = display::init(
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
    run_refresh_loop(
        &mut disp,
        &mut touch,
        &mut wifi,
        &mut store,
        &mut cfg,
        &mut backlight,
        location,
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

fn run_refresh_loop(
    disp: &mut CydDisplay,
    touch: &mut Touch,
    wifi: &mut Wifi,
    store: &mut ConfigStore,
    cfg: &mut StoredConfig,
    backlight: &mut Backlight,
    location: Location,
) -> Result<()> {
    let mut last_good: Option<WeatherData> = None;

    // The config/boot screens are shown with the backlight lit (display::init
    // turns it on). The weather display is dark by default, so switch the
    // backlight off exactly once as we transition into the refresh loop.
    let _ = backlight.off();

    // Remaining time the tap-activated backlight should stay lit, in ms. Kept
    // across refresh boundaries so the 60s window survives a refresh.
    let mut remaining_on_ms: u64 = 0;

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

        wait_with_backlight(touch, backlight, &mut remaining_on_ms, sleep_secs);
    }
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
    touch: &mut Touch,
    backlight: &mut Backlight,
    remaining_on_ms: &mut u64,
    total_secs: u64,
) {
    const CHUNK_MS: u64 = 100;
    let mut sleep_remaining_ms = total_secs.saturating_mul(1000);

    while sleep_remaining_ms > 0 {
        // A tap lights the backlight only when it is currently off; a tap while
        // the window is already active does not reset or extend it.
        if *remaining_on_ms == 0 && matches!(touch.read(), Ok(Some(_))) && backlight.on().is_ok() {
            *remaining_on_ms = BACKLIGHT_ON_SECS * 1000;
        }

        FreeRtos::delay_ms(CHUNK_MS as u32);

        if *remaining_on_ms > 0 {
            *remaining_on_ms = remaining_on_ms.saturating_sub(CHUNK_MS);
            if *remaining_on_ms == 0 {
                let _ = backlight.off();
            }
        }

        sleep_remaining_ms = sleep_remaining_ms.saturating_sub(CHUNK_MS);
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
