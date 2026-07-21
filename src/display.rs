//! ILI9341 display driver setup for the Cheap Yellow Display (CYD).
//!
//! Pin mapping (ESP32-2432S028R, confirmed against the Random Nerd Tutorials
//! CYD guide and the community pinout):
//!
//! | Signal        | GPIO |
//! |---------------|------|
//! | TFT SCLK      | 14   |
//! | TFT MOSI      | 13   |
//! | TFT MISO      | 12   |
//! | TFT CS        | 15   |
//! | TFT DC        | 2    |
//! | TFT backlight | 21   |
//! | TFT reset     | none (tied to the ESP32 EN/reset line) |
//!
//! NOTE: the two-USB "CYD2USB" revision uses an ST7789 controller with the
//! backlight on GPIO 27 instead of 21. See the README for how to adapt.

use anyhow::{Context, Result};
use display_interface_spi::SPIInterface;
use esp_idf_hal::delay::Ets;
use esp_idf_hal::gpio::{AnyIOPin, AnyOutputPin, Output, PinDriver};
use esp_idf_hal::spi::config::{Config as SpiConfig, DriverConfig};
use esp_idf_hal::spi::{SpiAnyPins, SpiDeviceDriver, SpiDriver};
use esp_idf_hal::units::FromValueType;
use mipidsi::models::ILI9341Rgb565;
use mipidsi::options::{ColorInversion, Orientation, Rotation};
use mipidsi::{Builder, NoResetPin};

/// Native (portrait) resolution of the ILI9341 panel on the CYD.
pub const WIDTH: u16 = 240;
pub const HEIGHT: u16 = 320;

type DisplaySpi = SpiDeviceDriver<'static, SpiDriver<'static>>;
type DisplayDc = PinDriver<'static, Output>;
type DisplayIf = SPIInterface<DisplaySpi, DisplayDc>;

/// Fully initialised CYD display, ready for `embedded-graphics` drawing.
pub type CydDisplay = mipidsi::Display<DisplayIf, ILI9341Rgb565, NoResetPin>;

/// Owns the backlight pin so it stays driven for the lifetime of the program.
pub struct Backlight {
    pin: PinDriver<'static, Output>,
}

impl Backlight {
    pub fn on(&mut self) -> Result<()> {
        self.pin.set_high().context("failed to enable backlight")?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn off(&mut self) -> Result<()> {
        self.pin.set_low().context("failed to disable backlight")?;
        Ok(())
    }
}

/// Initialise the ILI9341 over its dedicated SPI bus and switch the backlight on.
///
/// The pins are downgraded to `AnyIOPin`/`AnyOutputPin` in `main` so this
/// function has a single concrete signature.
pub fn init<SPI: SpiAnyPins + 'static>(
    spi: SPI,
    sclk: AnyIOPin<'static>,
    mosi: AnyIOPin<'static>,
    miso: AnyIOPin<'static>,
    cs: AnyOutputPin<'static>,
    dc: AnyOutputPin<'static>,
    backlight: AnyOutputPin<'static>,
) -> Result<(CydDisplay, Backlight)> {
    let driver = SpiDriver::new(spi, sclk, mosi, Some(miso), &DriverConfig::default())
        .context("failed to create display SPI driver")?;

    // The ILI9341 is happy at 40 MHz on the short CYD traces.
    let spi_config = SpiConfig::new().baudrate(40.MHz().into());
    let spi_device = SpiDeviceDriver::new(driver, Some(cs), &spi_config)
        .context("failed to create display SPI device")?;

    let dc = PinDriver::output(dc).context("failed to configure DC pin")?;
    let di = SPIInterface::new(spi_device, dc);

    let mut delay = Ets;
    let display = Builder::new(ILI9341Rgb565, di)
        .display_size(WIDTH, HEIGHT)
        .orientation(Orientation::new().rotate(Rotation::Deg0))
        .invert_colors(ColorInversion::Inverted)
        .init(&mut delay)
        .map_err(|e| anyhow::anyhow!("display init failed: {e:?}"))?;

    let mut bl = Backlight {
        pin: PinDriver::output(backlight).context("failed to configure backlight pin")?,
    };
    bl.on()?;

    Ok((display, bl))
}
