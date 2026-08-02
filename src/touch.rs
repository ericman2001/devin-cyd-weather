//! XPT2046 resistive touch controller driver for the CYD.
//!
//! | Signal | GPIO |
//! |--------|------|
//! | T_CLK  | 25   |
//! | T_MOSI | 32   |
//! | T_MISO | 39 (input only) |
//! | T_CS   | 33   |
//! | T_IRQ  | 36 (input only) |
//!
//! The controller is driven with a **bit-banged** SPI mode-0 transfer rather
//! than a hardware SPI host. The ESP32 only exposes two general-purpose SPI
//! hosts, and the CYD needs three SPI peripherals: the display owns SPI2 and
//! the SD card (added for the radar slideshow, see `src/storage.rs`) owns SPI3.
//! The XPT2046 tops out at 2 MHz anyway, so software SPI costs nothing here
//! and it keeps the calibration mapping easy to tune against a specific panel.

use anyhow::{Context, Result};
use esp_idf_hal::delay::Ets;
use esp_idf_hal::gpio::{AnyInputPin, AnyOutputPin, Input, Output, PinDriver, Pull};

// XPT2046 control bytes: start bit + channel select, 12-bit, differential mode.
const CMD_READ_X: u8 = 0x90;
const CMD_READ_Y: u8 = 0xD0;

/// Half clock period of the bit-banged bus (~250 kHz, well under the 2 MHz max).
const CLK_HALF_US: u32 = 2;

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
            swap_xy: true,
            invert_x: true,
            invert_y: false,
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

/// A calibrated touch point in the 240x320 screen coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchPoint {
    pub x: i32,
    pub y: i32,
}

pub struct Touch {
    clk: PinDriver<'static, Output>,
    mosi: PinDriver<'static, Output>,
    miso: PinDriver<'static, Input>,
    cs: PinDriver<'static, Output>,
    irq: PinDriver<'static, Input>,
    cal: Calibration,
}

impl Touch {
    /// Initialise the touch controller on its bit-banged SPI pins.
    pub fn init(
        sclk: AnyOutputPin<'static>,
        mosi: AnyOutputPin<'static>,
        miso: AnyInputPin<'static>,
        cs: AnyOutputPin<'static>,
        irq: AnyInputPin<'static>,
        cal: Calibration,
    ) -> Result<Self> {
        let mut clk = PinDriver::output(sclk).context("failed to configure touch CLK pin")?;
        let mut mosi = PinDriver::output(mosi).context("failed to configure touch MOSI pin")?;
        let miso =
            PinDriver::input(miso, Pull::Floating).context("failed to configure touch MISO pin")?;
        let mut cs = PinDriver::output(cs).context("failed to configure touch CS pin")?;
        let irq = PinDriver::input(irq, Pull::Up).context("failed to configure touch IRQ pin")?;

        clk.set_low().context("failed to idle touch CLK")?;
        mosi.set_low().context("failed to idle touch MOSI")?;
        cs.set_high().context("failed to deselect touch")?;

        Ok(Self {
            clk,
            mosi,
            miso,
            cs,
            irq,
            cal,
        })
    }

    /// Clock one byte out on MOSI while clocking one byte in on MISO (mode 0,
    /// MSB first): data is presented while CLK is low and sampled on its
    /// rising edge.
    fn transfer_byte(&mut self, out: u8) -> Result<u8> {
        let mut input = 0u8;
        for bit in (0..8).rev() {
            if (out >> bit) & 1 == 1 {
                self.mosi.set_high()
            } else {
                self.mosi.set_low()
            }
            .context("touch MOSI write failed")?;
            Ets::delay_us(CLK_HALF_US);

            self.clk.set_high().context("touch CLK write failed")?;
            input = (input << 1) | u8::from(self.miso.is_high());
            Ets::delay_us(CLK_HALF_US);

            self.clk.set_low().context("touch CLK write failed")?;
        }
        Ok(input)
    }

    fn read_channel(&mut self, cmd: u8) -> Result<u16> {
        self.cs.set_low().context("failed to select touch")?;
        let result = (|| {
            self.transfer_byte(cmd)?;
            let hi = self.transfer_byte(0x00)?;
            let lo = self.transfer_byte(0x00)?;
            Ok::<u16, anyhow::Error>(((hi as u16) << 8) | lo as u16)
        })();
        self.cs.set_high().context("failed to deselect touch")?;

        // 12-bit result is in bits [14:3] of the two returned bytes.
        Ok((result? >> 3) & 0x0FFF)
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
