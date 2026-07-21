//! XPT2046 resistive touch controller driver for the CYD.
//!
//! The touch controller lives on its own SPI bus (separate from the display):
//!
//! | Signal | GPIO |
//! |--------|------|
//! | T_CLK  | 25   |
//! | T_MOSI | 32   |
//! | T_MISO | 39 (input only) |
//! | T_CS   | 33   |
//! | T_IRQ  | 36 (input only) |
//!
//! This is a small hand-written SPI reader rather than an external crate so the
//! calibration mapping is easy to tune against a specific panel.

use anyhow::{Context, Result};
use esp_idf_hal::gpio::{AnyIOPin, AnyInputPin, AnyOutputPin, Input, PinDriver, Pull};
use esp_idf_hal::spi::config::{Config as SpiConfig, DriverConfig};
use esp_idf_hal::spi::{SpiAnyPins, SpiDeviceDriver, SpiDriver};
use esp_idf_hal::units::FromValueType;

// XPT2046 control bytes: start bit + channel select, 12-bit, differential mode.
const CMD_READ_X: u8 = 0x90;
const CMD_READ_Y: u8 = 0xD0;

/// Raw-ADC-to-screen calibration. The defaults are typical for the CYD panel;
/// tune them with the reported raw values if touches land in the wrong place.
#[derive(Debug, Clone, Copy)]
pub struct Calibration {
    pub x_min: u16,
    pub x_max: u16,
    pub y_min: u16,
    pub y_max: u16,
    pub width: u16,
    pub height: u16,
    /// Swap the raw X/Y axes (needed depending on panel orientation).
    pub swap_xy: bool,
    pub invert_x: bool,
    pub invert_y: bool,
}

impl Default for Calibration {
    fn default() -> Self {
        Self {
            x_min: 200,
            x_max: 3900,
            y_min: 200,
            y_max: 3900,
            width: crate::display::WIDTH,
            height: crate::display::HEIGHT,
            swap_xy: false,
            invert_x: true,
            invert_y: true,
        }
    }
}

impl Calibration {
    fn map(&self, raw_x: u16, raw_y: u16) -> (i32, i32) {
        let (rx, ry) = if self.swap_xy {
            (raw_y, raw_x)
        } else {
            (raw_x, raw_y)
        };
        let mut x = map_range(rx, self.x_min, self.x_max, self.width);
        let mut y = map_range(ry, self.y_min, self.y_max, self.height);
        if self.invert_x {
            x = self.width as i32 - 1 - x;
        }
        if self.invert_y {
            y = self.height as i32 - 1 - y;
        }
        (x, y)
    }
}

fn map_range(raw: u16, min: u16, max: u16, span: u16) -> i32 {
    let raw = raw.clamp(min, max) as i32;
    let min = min as i32;
    let max = max as i32;
    let span = span as i32;
    ((raw - min) * (span - 1)) / (max - min).max(1)
}

type TouchSpi = SpiDeviceDriver<'static, SpiDriver<'static>>;

/// A calibrated touch point in the 240x320 screen coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchPoint {
    pub x: i32,
    pub y: i32,
}

pub struct Touch {
    spi: TouchSpi,
    irq: PinDriver<'static, Input>,
    cal: Calibration,
}

impl Touch {
    /// Initialise the touch controller on its dedicated SPI bus.
    pub fn init<SPI: SpiAnyPins + 'static>(
        spi: SPI,
        sclk: AnyIOPin<'static>,
        mosi: AnyIOPin<'static>,
        miso: AnyInputPin<'static>,
        cs: AnyOutputPin<'static>,
        irq: AnyInputPin<'static>,
        cal: Calibration,
    ) -> Result<Self> {
        let driver = SpiDriver::new(spi, sclk, mosi, Some(miso), &DriverConfig::default())
            .context("failed to create touch SPI driver")?;
        // The XPT2046 requires a low SPI clock (<= 2.5 MHz).
        let cfg = SpiConfig::new().baudrate(2.MHz().into());
        let spi = SpiDeviceDriver::new(driver, Some(cs), &cfg)
            .context("failed to create touch SPI device")?;
        let irq = PinDriver::input(irq, Pull::Up).context("failed to configure touch IRQ pin")?;
        Ok(Self { spi, irq, cal })
    }

    fn read_channel(&mut self, cmd: u8) -> Result<u16> {
        let mut buf = [cmd, 0x00, 0x00];
        self.spi
            .transfer_in_place(&mut buf)
            .map_err(|e| anyhow::anyhow!("touch SPI transfer failed: {e:?}"))?;
        // 12-bit result is in bits [14:3] of the two returned bytes.
        let value = (((buf[1] as u16) << 8) | buf[2] as u16) >> 3;
        Ok(value & 0x0FFF)
    }

    /// Read the raw (uncalibrated) ADC values, averaging several samples to
    /// reduce jitter. Returns `None` when the screen is not being touched.
    fn read_raw(&mut self) -> Result<Option<(u16, u16)>> {
        // The IRQ line is pulled low while the panel is being pressed.
        if self.irq.is_high() {
            return Ok(None);
        }
        const SAMPLES: usize = 8;
        let mut xs: u32 = 0;
        let mut ys: u32 = 0;
        for _ in 0..SAMPLES {
            xs += self.read_channel(CMD_READ_X)? as u32;
            ys += self.read_channel(CMD_READ_Y)? as u32;
        }
        let x = (xs / SAMPLES as u32) as u16;
        let y = (ys / SAMPLES as u32) as u16;
        // Reject clearly invalid samples (e.g. a released press mid-read).
        if x < 50 || y < 50 {
            return Ok(None);
        }
        Ok(Some((x, y)))
    }

    /// Return the current calibrated touch point, if any.
    pub fn read(&mut self) -> Result<Option<TouchPoint>> {
        match self.read_raw()? {
            Some((rx, ry)) => {
                let (x, y) = self.cal.map(rx, ry);
                Ok(Some(TouchPoint { x, y }))
            }
            None => Ok(None),
        }
    }
}
