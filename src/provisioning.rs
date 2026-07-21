//! On-screen, touch-driven Wi-Fi provisioning UI.
//!
//! Rendered with `embedded-graphics` and driven by the XPT2046 touch panel.
//! The flow is:
//!   1. pick an SSID from the scanned list (scrollable),
//!   2. type the password on an on-screen QWERTY keyboard,
//!   3. optionally enter a manual latitude/longitude override,
//!   4. return a [`StoredConfig`] for the caller to persist + connect with.

use anyhow::Result;
use embedded_graphics::mono_font::iso_8859_1::{FONT_6X13, FONT_9X15};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle, RoundedRectangle};
use embedded_graphics::text::{Alignment, Baseline, Text, TextStyleBuilder};
use esp_idf_hal::delay::FreeRtos;

use crate::config::StoredConfig;
use crate::display::{CydDisplay, HEIGHT, WIDTH};
use crate::touch::{Touch, TouchPoint};

const BG: Rgb565 = Rgb565::new(2, 4, 8);
const FG: Rgb565 = Rgb565::WHITE;
const KEY_BG: Rgb565 = Rgb565::new(8, 16, 10);
const KEY_ACTIVE: Rgb565 = Rgb565::new(8, 45, 31);
const MUTED: Rgb565 = Rgb565::new(18, 38, 22);
const ACCENT: Rgb565 = Rgb565::new(8, 45, 31);

const W: i32 = WIDTH as i32;
const H: i32 = HEIGHT as i32;

/// A tappable rectangular region on screen.
#[derive(Clone, Copy)]
struct Hit {
    rect: Rectangle,
}

impl Hit {
    fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self {
            rect: Rectangle::new(
                Point::new(x, y),
                Size::new(w.max(0) as u32, h.max(0) as u32),
            ),
        }
    }

    fn contains(&self, p: TouchPoint) -> bool {
        let tl = self.rect.top_left;
        let br = tl + self.rect.size;
        p.x >= tl.x && p.x < br.x && p.y >= tl.y && p.y < br.y
    }
}

/// The provisioning UI. Borrows the display + touch for the duration.
pub struct Provisioner<'a> {
    display: &'a mut CydDisplay,
    touch: &'a mut Touch,
}

impl<'a> Provisioner<'a> {
    pub fn new(display: &'a mut CydDisplay, touch: &'a mut Touch) -> Self {
        Self { display, touch }
    }

    /// Run the full setup flow and return the entered configuration.
    pub fn run(&mut self, ssids: &[String]) -> Result<StoredConfig> {
        let ssid = self.select_ssid(ssids)?;
        let password = self.enter_text("Wi-Fi password", Keyboard::Full)?;
        let manual_location = if self.confirm("Set manual lat/long?", "Yes", "No, use IP")? {
            let lat = self.enter_text("Latitude (e.g. 30.27)", Keyboard::Numeric)?;
            let lon = self.enter_text("Longitude (e.g. -97.74)", Keyboard::Numeric)?;
            match (lat.trim().parse::<f64>(), lon.trim().parse::<f64>()) {
                (Ok(la), Ok(lo)) => Some((la, lo)),
                _ => None,
            }
        } else {
            None
        };

        Ok(StoredConfig {
            ssid: Some(ssid),
            password: Some(password),
            manual_location,
        })
    }

    // -- Input primitives ---------------------------------------------------

    /// Block until a touch is pressed and then released, returning the press
    /// point. Debounces by requiring a clean release.
    fn wait_tap(&mut self) -> Result<TouchPoint> {
        // Wait for a press.
        let point = loop {
            if let Some(p) = self.touch.read()? {
                break p;
            }
            FreeRtos::delay_ms(20);
        };
        // Wait for release.
        loop {
            if self.touch.read()?.is_none() {
                break;
            }
            FreeRtos::delay_ms(20);
        }
        Ok(point)
    }

    fn header(&mut self, title: &str) -> Result<()> {
        self.display.clear(BG).ok();
        Rectangle::new(Point::new(0, 0), Size::new(WIDTH as u32, 22))
            .into_styled(PrimitiveStyle::with_fill(ACCENT))
            .draw(self.display)
            .ok();
        self.text(
            title,
            W / 2,
            3,
            &FONT_9X15,
            Rgb565::BLACK,
            Alignment::Center,
        );
        Ok(())
    }

    fn text(
        &mut self,
        s: &str,
        x: i32,
        y: i32,
        font: &embedded_graphics::mono_font::MonoFont<'_>,
        color: Rgb565,
        align: Alignment,
    ) {
        let style = TextStyleBuilder::new()
            .alignment(align)
            .baseline(Baseline::Top)
            .build();
        let _ = Text::with_text_style(s, Point::new(x, y), MonoTextStyle::new(font, color), style)
            .draw(self.display);
    }

    fn button(&mut self, hit: &Hit, label: &str, active: bool) {
        let bg = if active { KEY_ACTIVE } else { KEY_BG };
        RoundedRectangle::with_equal_corners(hit.rect, Size::new(4, 4))
            .into_styled(PrimitiveStyle::with_fill(bg))
            .draw(self.display)
            .ok();
        let c = hit.rect.top_left + hit.rect.size / 2;
        self.text(label, c.x, c.y - 6, &FONT_6X13, FG, Alignment::Center);
    }

    // -- Screens ------------------------------------------------------------

    /// Scrollable SSID picker. Shows up to `PAGE` entries with Up/Down paging.
    fn select_ssid(&mut self, ssids: &[String]) -> Result<String> {
        const PAGE: usize = 8;
        const ROW_H: i32 = 26;
        let top = 30;
        let mut offset = 0usize;

        if ssids.is_empty() {
            // No networks found: fall back to manual SSID entry.
            return self.enter_text("Enter SSID", Keyboard::Full);
        }

        loop {
            self.header("Select Wi-Fi")?;
            let end = (offset + PAGE).min(ssids.len());
            let mut rows: Vec<(Hit, usize)> = Vec::new();
            for (row, idx) in (offset..end).enumerate() {
                let y = top + row as i32 * ROW_H;
                let hit = Hit::new(6, y, W - 12, ROW_H - 4);
                self.button(&hit, "", false);
                self.text(&ssids[idx], 14, y + 4, &FONT_6X13, FG, Alignment::Left);
                rows.push((hit, idx));
            }

            // Paging + manual entry controls along the bottom.
            let up = Hit::new(6, H - 34, 60, 28);
            let down = Hit::new(72, H - 34, 60, 28);
            let manual = Hit::new(W - 96, H - 34, 90, 28);
            self.button(&up, "Up", offset > 0);
            self.button(&down, "Down", end < ssids.len());
            self.button(&manual, "Manual", false);

            let tap = self.wait_tap()?;
            if up.contains(tap) && offset > 0 {
                offset -= PAGE.min(offset);
                continue;
            }
            if down.contains(tap) && end < ssids.len() {
                offset += PAGE;
                continue;
            }
            if manual.contains(tap) {
                return self.enter_text("Enter SSID", Keyboard::Full);
            }
            for (hit, idx) in rows {
                if hit.contains(tap) {
                    return Ok(ssids[idx].clone());
                }
            }
        }
    }

    /// Two-choice confirmation screen.
    fn confirm(&mut self, question: &str, yes: &str, no: &str) -> Result<bool> {
        self.header("Location")?;
        self.text(question, W / 2, 60, &FONT_6X13, FG, Alignment::Center);
        let yes_hit = Hit::new(20, 120, W - 40, 40);
        let no_hit = Hit::new(20, 175, W - 40, 40);
        self.button(&yes_hit, yes, true);
        self.button(&no_hit, no, false);
        loop {
            let tap = self.wait_tap()?;
            if yes_hit.contains(tap) {
                return Ok(true);
            }
            if no_hit.contains(tap) {
                return Ok(false);
            }
        }
    }

    /// On-screen keyboard text entry.
    fn enter_text(&mut self, title: &str, kind: Keyboard) -> Result<String> {
        let mut buffer = String::new();
        let mut shift = false;
        loop {
            self.header(title)?;
            // Text field showing the current buffer (password shown as typed).
            Rectangle::new(Point::new(6, 28), Size::new((W - 12) as u32, 26))
                .into_styled(PrimitiveStyle::with_stroke(MUTED, 1))
                .draw(self.display)
                .ok();
            let shown = if buffer.is_empty() {
                "_"
            } else {
                buffer.as_str()
            };
            self.text(shown, 12, 34, &FONT_6X13, FG, Alignment::Left);

            let keys = kind.rows(shift);
            let hits = self.draw_keyboard(&keys);
            let (backspace, done, shift_hit, space) = self.draw_key_controls(kind);

            let tap = self.wait_tap()?;
            if done.contains(tap) {
                return Ok(buffer);
            }
            if backspace.contains(tap) {
                buffer.pop();
                continue;
            }
            if matches!(kind, Keyboard::Full) && shift_hit.contains(tap) {
                shift = !shift;
                continue;
            }
            if space.contains(tap) {
                buffer.push(' ');
                continue;
            }
            for (hit, ch) in hits {
                if hit.contains(tap) {
                    buffer.push(ch);
                    break;
                }
            }
        }
    }

    /// Draw the character grid and return the hit-boxes with their chars.
    fn draw_keyboard(&mut self, rows: &[Vec<char>]) -> Vec<(Hit, char)> {
        const TOP: i32 = 70;
        const KEY_H: i32 = 30;
        const GAP: i32 = 3;
        let mut hits = Vec::new();
        for (r, row) in rows.iter().enumerate() {
            let n = row.len() as i32;
            let key_w = (W - GAP) / n.max(1) - GAP;
            let y = TOP + r as i32 * (KEY_H + GAP);
            for (c, &ch) in row.iter().enumerate() {
                let x = GAP + c as i32 * (key_w + GAP);
                let hit = Hit::new(x, y, key_w, KEY_H);
                let mut tmp = [0u8; 4];
                let s = ch.encode_utf8(&mut tmp);
                self.button(&hit, s, false);
                hits.push((hit, ch));
            }
        }
        hits
    }

    /// Draw the bottom control row (shift / space / backspace / done).
    fn draw_key_controls(&mut self, kind: Keyboard) -> (Hit, Hit, Hit, Hit) {
        let y = H - 38;
        let shift_hit = Hit::new(3, y, 44, 32);
        let space = Hit::new(50, y, W - 50 - 3 - 94, 32);
        let backspace = Hit::new(W - 94, y, 44, 32);
        let done = Hit::new(W - 47, y, 44, 32);
        if matches!(kind, Keyboard::Full) {
            self.button(&shift_hit, "Sh", false);
            self.button(&space, "space", false);
        } else {
            // Numeric keyboards get a wider space-less layout.
            self.button(&space, "space", false);
        }
        self.button(&backspace, "<-", false);
        self.button(&done, "OK", true);
        (backspace, done, shift_hit, space)
    }
}

/// Keyboard layouts.
#[derive(Clone, Copy)]
enum Keyboard {
    Full,
    Numeric,
}

impl Keyboard {
    fn rows(self, shift: bool) -> Vec<Vec<char>> {
        match self {
            Keyboard::Full => {
                let letters = ["1234567890", "qwertyuiop", "asdfghjkl", "zxcvbnm-_."];
                letters
                    .iter()
                    .map(|r| {
                        r.chars()
                            .map(|c| if shift { c.to_ascii_uppercase() } else { c })
                            .collect()
                    })
                    .collect()
            }
            Keyboard::Numeric => {
                vec![
                    "123".chars().collect(),
                    "456".chars().collect(),
                    "789".chars().collect(),
                    "-0.".chars().collect(),
                ]
            }
        }
    }
}
