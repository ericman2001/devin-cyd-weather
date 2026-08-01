//! Weather dashboard + status-screen rendering with `embedded-graphics`.
//!
//! All drawing functions are generic over any `DrawTarget` producing `Rgb565`
//! pixels, which keeps them testable and decoupled from the concrete display.

use core::fmt::Debug;

use embedded_graphics::mono_font::iso_8859_1::{FONT_10X20, FONT_6X10, FONT_6X13};
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{Circle, Line, PrimitiveStyle, Rectangle, Triangle};
use embedded_graphics::text::{Alignment, Baseline, Text, TextStyleBuilder};

use crate::display::{HEIGHT, WIDTH};
use crate::touch::TouchPoint;
use crate::weather::{aqi_category, uv_category, WeatherData, WeatherIcon};

// -- Palette ----------------------------------------------------------------
const BG: Rgb565 = Rgb565::new(2, 4, 8); // near-black navy
const FG: Rgb565 = Rgb565::WHITE;
const MUTED: Rgb565 = Rgb565::new(18, 38, 22);
const ACCENT: Rgb565 = Rgb565::new(8, 45, 31); // cyan-ish
const SUN: Rgb565 = Rgb565::new(31, 50, 0);
const CLOUD: Rgb565 = Rgb565::new(24, 48, 24);
const RAIN: Rgb565 = Rgb565::new(10, 30, 31);
const SNOW: Rgb565 = Rgb565::WHITE;
const WARN: Rgb565 = Rgb565::new(31, 30, 0);

/// Convenience alias so the module compiles cleanly for any error type.
type R<E> = Result<(), E>;

fn draw_text<D>(
    display: &mut D,
    s: &str,
    x: i32,
    y: i32,
    font: &embedded_graphics::mono_font::MonoFont<'_>,
    color: Rgb565,
    align: Alignment,
) -> R<D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let style = TextStyleBuilder::new()
        .alignment(align)
        .baseline(Baseline::Top)
        .build();
    Text::with_text_style(s, Point::new(x, y), MonoTextStyle::new(font, color), style)
        .draw(display)?;
    Ok(())
}

/// Fill the whole screen with the background colour.
pub fn clear<D>(display: &mut D) -> R<D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    display.clear(BG)
}

/// Render the full weather dashboard.
pub fn draw_weather<D>(display: &mut D, data: &WeatherData, place: Option<&str>) -> R<D::Error>
where
    D: DrawTarget<Color = Rgb565>,
    D::Error: Debug,
{
    clear(display)?;

    // Location / header.
    if let Some(p) = place {
        draw_text(
            display,
            p,
            WIDTH as i32 / 2,
            4,
            &FONT_6X13,
            ACCENT,
            Alignment::Center,
        )?;
    }

    // Weather icon (top-left) + big current temperature.
    draw_icon(display, data.condition.icon, Point::new(20, 30), 44)?;

    let temp = format!("{:.0}\u{00B0}F", data.temperature_f);
    draw_text(
        display,
        &temp,
        WIDTH as i32 - 10,
        28,
        &FONT_10X20,
        FG,
        Alignment::Right,
    )?;
    draw_text(
        display,
        data.condition.label,
        WIDTH as i32 - 10,
        52,
        &FONT_6X13,
        MUTED,
        Alignment::Right,
    )?;
    let feels = format!("Feels like {:.0}\u{00B0}F", data.feels_like_f);
    draw_text(
        display,
        &feels,
        WIDTH as i32 - 10,
        68,
        &FONT_6X10,
        MUTED,
        Alignment::Right,
    )?;

    // Divider.
    Line::new(Point::new(8, 90), Point::new(WIDTH as i32 - 8, 90))
        .into_styled(PrimitiveStyle::with_stroke(MUTED, 1))
        .draw(display)?;

    // Metrics row: humidity, wind, AQI.
    let hum = format!("Humidity: {:.0}%", data.humidity_pct);
    let wind = format!("Wind: {:.0} mph", data.wind_mph);
    draw_text(display, &hum, 12, 100, &FONT_6X13, FG, Alignment::Left)?;
    draw_text(display, &wind, 12, 120, &FONT_6X13, FG, Alignment::Left)?;

    match data.us_aqi {
        Some(aqi) => {
            let s = format!("AQI: {aqi} ({})", aqi_category(aqi));
            let color = aqi_color(aqi);
            draw_text(display, &s, 12, 140, &FONT_6X13, color, Alignment::Left)?;
        }
        None => {
            draw_text(
                display,
                "AQI: n/a",
                12,
                140,
                &FONT_6X13,
                MUTED,
                Alignment::Left,
            )?;
        }
    }

    match data.uv_index {
        Some(uv) => {
            let s = format!("UV: {:.0} ({})", uv, uv_category(uv));
            draw_text(display, &s, 12, 154, &FONT_6X10, FG, Alignment::Left)?;
        }
        None => {
            draw_text(
                display,
                "UV: n/a",
                12,
                154,
                &FONT_6X10,
                MUTED,
                Alignment::Left,
            )?;
        }
    }

    // Divider.
    Line::new(Point::new(8, 168), Point::new(WIDTH as i32 - 8, 168))
        .into_styled(PrimitiveStyle::with_stroke(MUTED, 1))
        .draw(display)?;

    draw_text(
        display,
        "Forecast",
        12,
        176,
        &FONT_6X10,
        ACCENT,
        Alignment::Left,
    )?;
    draw_forecast_row(display, data, 194)?;

    Ok(())
}

fn draw_forecast_row<D>(display: &mut D, data: &WeatherData, top: i32) -> R<D::Error>
where
    D: DrawTarget<Color = Rgb565>,
    D::Error: Debug,
{
    let days = data.daily.len().max(1);
    let col_w = WIDTH as i32 / days as i32;
    for (i, day) in data.daily.iter().enumerate() {
        let cx = col_w * i as i32 + col_w / 2;
        let label = weekday_label(&day.date, i);
        draw_text(
            display,
            label,
            cx,
            top,
            &FONT_6X10,
            MUTED,
            Alignment::Center,
        )?;
        draw_icon(
            display,
            day.condition.icon,
            Point::new(cx - 11, top + 16),
            22,
        )?;
        let hi = format!("{:.0}\u{00B0}", day.high_f);
        let lo = format!("{:.0}\u{00B0}", day.low_f);
        draw_text(
            display,
            &hi,
            cx,
            top + 44,
            &FONT_6X13,
            FG,
            Alignment::Center,
        )?;
        draw_text(
            display,
            &lo,
            cx,
            top + 60,
            &FONT_6X10,
            MUTED,
            Alignment::Center,
        )?;
    }
    Ok(())
}

/// Render a centered status / error screen (e.g. "Connecting...", "No Wi-Fi").
pub fn draw_status<D>(display: &mut D, title: &str, message: &str) -> R<D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    clear(display)?;
    let cx = WIDTH as i32 / 2;
    draw_text(
        display,
        title,
        cx,
        HEIGHT as i32 / 2 - 24,
        &FONT_10X20,
        FG,
        Alignment::Center,
    )?;
    draw_text(
        display,
        message,
        cx,
        HEIGHT as i32 / 2 + 6,
        &FONT_6X13,
        MUTED,
        Alignment::Center,
    )?;
    Ok(())
}

// -- Screens + bottom toolbar ------------------------------------------------

/// Height of the bottom navigation toolbar.
pub const TOOLBAR_HEIGHT: i32 = 28;

/// Y coordinate of the toolbar's top edge.
pub const TOOLBAR_TOP: i32 = HEIGHT as i32 - TOOLBAR_HEIGHT;

/// Top of the radar image area on the radar screen.
pub const RADAR_VIEW_TOP: i32 = 24;

/// The screen currently shown on the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    #[default]
    Weather,
    Radar,
}

impl Screen {
    fn label(self) -> &'static str {
        match self {
            Screen::Weather => "Weather",
            Screen::Radar => "Radar",
        }
    }
}

/// Draw the bottom toolbar, highlighting the active screen's button.
pub fn draw_toolbar<D>(display: &mut D, active: Screen) -> R<D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let half = WIDTH as i32 / 2;
    for (i, screen) in [Screen::Weather, Screen::Radar].into_iter().enumerate() {
        let x = i as i32 * half;
        let selected = screen == active;
        Rectangle::new(
            Point::new(x, TOOLBAR_TOP),
            Size::new(half as u32, TOOLBAR_HEIGHT as u32),
        )
        .into_styled(PrimitiveStyle::with_fill(if selected {
            ACCENT
        } else {
            BG
        }))
        .draw(display)?;
        Rectangle::new(
            Point::new(x, TOOLBAR_TOP),
            Size::new(half as u32, TOOLBAR_HEIGHT as u32),
        )
        .into_styled(PrimitiveStyle::with_stroke(MUTED, 1))
        .draw(display)?;
        draw_text(
            display,
            screen.label(),
            x + half / 2,
            TOOLBAR_TOP + 9,
            &FONT_6X13,
            if selected { Rgb565::BLACK } else { FG },
            Alignment::Center,
        )?;
    }
    Ok(())
}

/// Map a touch point to the toolbar button it hit, if any.
pub fn toolbar_hit(point: TouchPoint) -> Option<Screen> {
    if point.y < TOOLBAR_TOP || point.y >= HEIGHT as i32 {
        return None;
    }
    if point.x < WIDTH as i32 / 2 {
        Some(Screen::Weather)
    } else {
        Some(Screen::Radar)
    }
}

/// Draw the static parts of the radar screen (title + status line area).
///
/// The radar frames themselves are streamed straight from the SD card into the
/// panel by `radar::blit_frame`, bypassing `embedded-graphics` entirely.
pub fn draw_radar_chrome<D>(display: &mut D, title: &str, status: &str) -> R<D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    clear(display)?;
    draw_text(
        display,
        title,
        WIDTH as i32 / 2,
        4,
        &FONT_6X13,
        ACCENT,
        Alignment::Center,
    )?;
    draw_radar_status(display, status)
}

/// Update just the status line below the radar view.
pub fn draw_radar_status<D>(display: &mut D, status: &str) -> R<D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let top = TOOLBAR_TOP - 20;
    Rectangle::new(
        Point::new(0, top),
        Size::new(WIDTH as u32, (TOOLBAR_TOP - top) as u32),
    )
    .into_styled(PrimitiveStyle::with_fill(BG))
    .draw(display)?;
    draw_text(
        display,
        status,
        WIDTH as i32 / 2,
        top + 4,
        &FONT_6X10,
        MUTED,
        Alignment::Center,
    )
}

/// Render an error banner over the (possibly stale) weather screen.
pub fn draw_error_banner<D>(display: &mut D, message: &str) -> R<D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    Rectangle::new(Point::new(0, 0), Size::new(WIDTH as u32, 16))
        .into_styled(PrimitiveStyle::with_fill(WARN))
        .draw(display)?;
    draw_text(
        display,
        message,
        WIDTH as i32 / 2,
        2,
        &FONT_6X10,
        Rgb565::BLACK,
        Alignment::Center,
    )?;
    Ok(())
}

fn aqi_color(aqi: u16) -> Rgb565 {
    match aqi {
        0..=50 => Rgb565::new(6, 50, 6),
        51..=100 => Rgb565::new(31, 50, 0),
        101..=150 => Rgb565::new(31, 32, 0),
        151..=200 => Rgb565::new(31, 8, 4),
        _ => Rgb565::new(24, 0, 12),
    }
}

/// Draw a small weather glyph inside a `size x size` box anchored at `origin`.
fn draw_icon<D>(display: &mut D, icon: WeatherIcon, origin: Point, size: u32) -> R<D::Error>
where
    D: DrawTarget<Color = Rgb565>,
    D::Error: Debug,
{
    let s = size as i32;
    let center = Point::new(origin.x + s / 2, origin.y + s / 2);
    match icon {
        WeatherIcon::Clear => {
            Circle::with_center(center, size / 2)
                .into_styled(PrimitiveStyle::with_fill(SUN))
                .draw(display)?;
        }
        WeatherIcon::PartlyCloudy => {
            Circle::with_center(Point::new(center.x + s / 6, center.y - s / 6), size / 3)
                .into_styled(PrimitiveStyle::with_fill(SUN))
                .draw(display)?;
            draw_cloud(display, origin, size, CLOUD)?;
        }
        WeatherIcon::Cloudy | WeatherIcon::Fog => {
            draw_cloud(display, origin, size, CLOUD)?;
        }
        WeatherIcon::Drizzle | WeatherIcon::Rain => {
            draw_cloud(display, origin, size, CLOUD)?;
            for i in 0..3 {
                let x = origin.x + s / 4 + i * s / 4;
                Line::new(
                    Point::new(x, origin.y + s - s / 4),
                    Point::new(x - 2, origin.y + s),
                )
                .into_styled(PrimitiveStyle::with_stroke(RAIN, 2))
                .draw(display)?;
            }
        }
        WeatherIcon::Snow => {
            draw_cloud(display, origin, size, CLOUD)?;
            for i in 0..3 {
                let x = origin.x + s / 4 + i * s / 4;
                Circle::with_center(Point::new(x, origin.y + s - s / 6), 3)
                    .into_styled(PrimitiveStyle::with_fill(SNOW))
                    .draw(display)?;
            }
        }
        WeatherIcon::Thunderstorm => {
            draw_cloud(display, origin, size, CLOUD)?;
            Triangle::new(
                Point::new(center.x, origin.y + s / 2),
                Point::new(center.x - s / 8, origin.y + s),
                Point::new(center.x + s / 8, origin.y + s - s / 4),
            )
            .into_styled(PrimitiveStyle::with_fill(WARN))
            .draw(display)?;
        }
        WeatherIcon::Unknown => {
            Rectangle::new(origin, Size::new(size, size))
                .into_styled(PrimitiveStyle::with_stroke(MUTED, 1))
                .draw(display)?;
        }
    }
    Ok(())
}

fn draw_cloud<D>(display: &mut D, origin: Point, size: u32, color: Rgb565) -> R<D::Error>
where
    D: DrawTarget<Color = Rgb565>,
{
    let s = size as i32;
    Circle::with_center(Point::new(origin.x + s / 3, origin.y + s * 2 / 3), size / 3)
        .into_styled(PrimitiveStyle::with_fill(color))
        .draw(display)?;
    Circle::with_center(
        Point::new(origin.x + s * 2 / 3, origin.y + s * 2 / 3),
        size / 3,
    )
    .into_styled(PrimitiveStyle::with_fill(color))
    .draw(display)?;
    Rectangle::new(
        Point::new(origin.x + s / 4, origin.y + s / 2),
        Size::new((size / 2).max(1), (size / 4).max(1)),
    )
    .into_styled(PrimitiveStyle::with_fill(color))
    .draw(display)?;
    Ok(())
}

/// Best-effort weekday label from an ISO date string; falls back to "D+n".
fn weekday_label(date: &str, index: usize) -> &'static str {
    // date is "YYYY-MM-DD". Compute weekday with Zeller's congruence.
    let parse = || -> Option<(i32, u32, u32)> {
        let mut parts = date.split('-');
        let y = parts.next()?.parse::<i32>().ok()?;
        let m = parts.next()?.parse::<u32>().ok()?;
        let d = parts.next()?.parse::<u32>().ok()?;
        Some((y, m, d))
    };
    match parse() {
        Some((y, m, d)) => WEEKDAYS[weekday_index(y, m, d)],
        None => match index {
            0 => "Today",
            _ => "Day",
        },
    }
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// Sakamoto's algorithm: 0 = Sunday .. 6 = Saturday.
fn weekday_index(mut y: i32, m: u32, d: u32) -> usize {
    const T: [i32; 12] = [0, 3, 2, 5, 0, 3, 5, 1, 4, 6, 2, 4];
    if m < 3 {
        y -= 1;
    }
    let m = m as i32;
    let d = d as i32;
    (((y + y / 4 - y / 100 + y / 400 + T[(m - 1) as usize] + d) % 7) as usize) % 7
}
