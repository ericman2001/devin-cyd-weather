//! SD-card bring-up: SDSPI host + FAT filesystem mounted into the ESP-IDF VFS.
//!
//! Once [`Storage::mount`] succeeds, ordinary `std::fs` calls work against
//! paths under [`MOUNT_POINT`] (`/sdcard`), which is what the radar pipeline
//! uses to stage downloads and decoded frames without ever holding a whole
//! image in RAM.
//!
//! Pin mapping (ESP32-2432S028R, the micro-SD slot on the back of the board):
//!
//! | Signal  | GPIO |
//! |---------|------|
//! | SD SCK  | 18   |
//! | SD MOSI | 23   |
//! | SD MISO | 19   |
//! | SD CS   | 5    |
//!
//! These are the ESP32 "VSPI" default pins, and on the CYD they are wired
//! *only* to the SD slot. The ESP32 has just two general-purpose SPI hosts
//! (SPI2/HSPI and SPI3/VSPI): the display owns SPI2, so the SD card takes
//! SPI3 and the XPT2046 touch controller is bit-banged in software (see
//! `src/touch.rs`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use esp_idf_hal::gpio::{AnyIOPin, AnyInputPin, AnyOutputPin};
use esp_idf_hal::sd::spi::SdSpiHostDriver;
use esp_idf_hal::sd::{SdCardConfiguration, SdCardDriver};
use esp_idf_hal::spi::config::DriverConfig;
use esp_idf_hal::spi::{Dma, SpiAnyPins, SpiDriver};
use esp_idf_svc::fs::fatfs::Fatfs;
use esp_idf_svc::io::vfs::MountedFatfs;

/// VFS path the SD card's FAT filesystem is mounted at.
pub const MOUNT_POINT: &str = "/sdcard";

/// Maximum number of simultaneously open files on the mount.
const MAX_FDS: usize = 4;

/// SPI clock for the card. 20 MHz is comfortably within spec for the short CYD
/// traces while still being fast enough to stream frames.
const SPEED_KHZ: u32 = 20_000;

type SdCard = SdCardDriver<SdSpiHostDriver<'static, SpiDriver<'static>>>;

/// A mounted SD card. Dropping this unmounts the filesystem, so keep it alive
/// for as long as `/sdcard` paths are in use.
pub struct Storage {
    _fatfs: MountedFatfs<Fatfs<SdCard>>,
}

impl Storage {
    /// Bring up the SD card over SDSPI and mount its FAT filesystem at
    /// [`MOUNT_POINT`].
    ///
    /// The card must already be formatted as FAT (the firmware deliberately
    /// never formats, so a card with user data is never wiped).
    pub fn mount<SPI: SpiAnyPins + 'static>(
        spi: SPI,
        sclk: AnyIOPin<'static>,
        mosi: AnyIOPin<'static>,
        miso: AnyIOPin<'static>,
        cs: AnyOutputPin<'static>,
    ) -> Result<Self> {
        let driver = SpiDriver::new(
            spi,
            sclk,
            mosi,
            Some(miso),
            &DriverConfig::default().dma(Dma::Auto(4096)),
        )
        .context("failed to create SD SPI driver")?;

        let mut card_config = SdCardConfiguration::new();
        card_config.speed_khz = SPEED_KHZ;

        let host = SdSpiHostDriver::new(
            driver,
            Some(cs),
            AnyInputPin::none(),
            AnyInputPin::none(),
            AnyInputPin::none(),
            None,
        )
        .context("failed to create SDSPI host")?;

        let card = SdCardDriver::new_spi(host, &card_config).context("no SD card detected")?;

        let fatfs = Fatfs::new_sdcard(0, card).context("failed to create FAT filesystem")?;
        let mounted = MountedFatfs::mount(fatfs, MOUNT_POINT, MAX_FDS)
            .context("failed to mount FAT filesystem (is the card formatted FAT32?)")?;

        log::info!("SD card mounted at {MOUNT_POINT}");
        Ok(Self { _fatfs: mounted })
    }
}

/// Build an absolute path under the SD-card mount point.
pub fn path(relative: &str) -> PathBuf {
    Path::new(MOUNT_POINT).join(relative.trim_start_matches('/'))
}

/// Create `dir` (relative to the mount point) if it does not exist yet.
pub fn ensure_dir(relative: &str) -> Result<PathBuf> {
    let dir = path(relative);
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create {}", dir.display()))?;
    }
    Ok(dir)
}
