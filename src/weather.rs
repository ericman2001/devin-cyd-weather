//! Open-Meteo weather + air-quality client (American customary units).

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::config::{AIR_QUALITY_API_BASE, FORECAST_API_BASE, FORECAST_DAYS};

// ---------------------------------------------------------------------------
// Raw API response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ForecastResponse {
    current: CurrentBlock,
    daily: DailyBlock,
    /// Offset of the location's local time from UTC (`timezone=auto`).
    #[serde(default)]
    utc_offset_seconds: i64,
}

#[derive(Debug, Deserialize)]
struct CurrentBlock {
    temperature_2m: f64,
    relative_humidity_2m: f64,
    apparent_temperature: f64,
    weather_code: u16,
    wind_speed_10m: f64,
}

#[derive(Debug, Deserialize)]
struct DailyBlock {
    time: Vec<String>,
    weather_code: Vec<u16>,
    temperature_2m_max: Vec<f64>,
    temperature_2m_min: Vec<f64>,
}

#[derive(Debug, Deserialize)]
struct AirQualityResponse {
    current: AqiBlock,
}

#[derive(Debug, Deserialize)]
struct AqiBlock {
    us_aqi: Option<f64>,
    uv_index: Option<f64>,
}

// ---------------------------------------------------------------------------
// Domain types rendered by the UI
// ---------------------------------------------------------------------------

/// A single day in the multi-day forecast row.
#[derive(Debug, Clone)]
pub struct DailyForecast {
    /// ISO date string (`YYYY-MM-DD`).
    pub date: String,
    pub high_f: f32,
    pub low_f: f32,
    pub condition: WeatherCondition,
}

/// The full weather snapshot rendered on screen.
#[derive(Debug, Clone)]
pub struct WeatherData {
    pub temperature_f: f32,
    pub feels_like_f: f32,
    pub humidity_pct: f32,
    pub wind_mph: f32,
    pub condition: WeatherCondition,
    /// US AQI (EPA scale). `None` when the air-quality API had no value.
    pub us_aqi: Option<u16>,
    /// Current UV index. `None` when the air-quality API had no value.
    pub uv_index: Option<f32>,
    pub daily: Vec<DailyForecast>,
    /// Offset of the location's local time from UTC, used to label radar frames
    /// without a synchronised clock on the device.
    pub utc_offset_seconds: i64,
}

/// Human-readable weather condition mapped from a WMO weather code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WeatherCondition {
    /// The raw WMO weather interpretation code.
    pub code: u16,
    /// Short human-readable label (e.g. "Partly cloudy").
    pub label: &'static str,
    /// A compact icon/emoji-style glyph key the UI can render.
    pub icon: WeatherIcon,
}

/// Icon buckets. The `ui` module maps these to drawn glyphs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeatherIcon {
    Clear,
    PartlyCloudy,
    Cloudy,
    Fog,
    Drizzle,
    Rain,
    Snow,
    Thunderstorm,
    Unknown,
}

/// Map a WMO weather interpretation code to a human-readable condition.
///
/// Reference: <https://open-meteo.com/en/docs> ("Weather variable documentation"
/// -> WMO Weather interpretation codes).
pub fn describe_weather_code(code: u16) -> WeatherCondition {
    let (label, icon) = match code {
        0 => ("Clear sky", WeatherIcon::Clear),
        1 => ("Mainly clear", WeatherIcon::Clear),
        2 => ("Partly cloudy", WeatherIcon::PartlyCloudy),
        3 => ("Overcast", WeatherIcon::Cloudy),
        45 | 48 => ("Fog", WeatherIcon::Fog),
        51 => ("Light drizzle", WeatherIcon::Drizzle),
        53 => ("Drizzle", WeatherIcon::Drizzle),
        55 => ("Dense drizzle", WeatherIcon::Drizzle),
        56 | 57 => ("Freezing drizzle", WeatherIcon::Drizzle),
        61 => ("Light rain", WeatherIcon::Rain),
        63 => ("Rain", WeatherIcon::Rain),
        65 => ("Heavy rain", WeatherIcon::Rain),
        66 | 67 => ("Freezing rain", WeatherIcon::Rain),
        71 => ("Light snow", WeatherIcon::Snow),
        73 => ("Snow", WeatherIcon::Snow),
        75 => ("Heavy snow", WeatherIcon::Snow),
        77 => ("Snow grains", WeatherIcon::Snow),
        80 => ("Light showers", WeatherIcon::Rain),
        81 => ("Showers", WeatherIcon::Rain),
        82 => ("Violent showers", WeatherIcon::Rain),
        85 | 86 => ("Snow showers", WeatherIcon::Snow),
        95 => ("Thunderstorm", WeatherIcon::Thunderstorm),
        96 | 99 => ("Thunderstorm w/ hail", WeatherIcon::Thunderstorm),
        _ => ("Unknown", WeatherIcon::Unknown),
    };
    WeatherCondition { code, label, icon }
}

/// US AQI category label for a given AQI value (EPA breakpoints).
pub fn aqi_category(aqi: u16) -> &'static str {
    match aqi {
        0..=50 => "Good",
        51..=100 => "Moderate",
        101..=150 => "Unhealthy (SG)",
        151..=200 => "Unhealthy",
        201..=300 => "Very Unhealthy",
        _ => "Hazardous",
    }
}

/// UV exposure category label for a given UV index (WHO scale).
pub fn uv_category(uv: f32) -> &'static str {
    match uv {
        u if u < 3.0 => "Low",
        u if u < 6.0 => "Moderate",
        u if u < 8.0 => "High",
        u if u < 11.0 => "Very High",
        _ => "Extreme",
    }
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

fn build_forecast_url(lat: f64, lon: f64) -> String {
    format!(
        "{base}?latitude={lat:.4}&longitude={lon:.4}\
         &current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m\
         &daily=weather_code,temperature_2m_max,temperature_2m_min\
         &temperature_unit=fahrenheit&wind_speed_unit=mph&precipitation_unit=inch\
         &timezone=auto&forecast_days={days}",
        base = FORECAST_API_BASE,
        days = FORECAST_DAYS,
    )
}

fn build_air_quality_url(lat: f64, lon: f64) -> String {
    format!(
        "{base}?latitude={lat:.4}&longitude={lon:.4}&current=us_aqi,uv_index",
        base = AIR_QUALITY_API_BASE,
    )
}

/// Fetch the current weather + multi-day forecast and the US AQI.
///
/// The AQI call is best-effort: if it fails we still return the forecast with
/// `us_aqi = None` so a partial air-quality outage doesn't blank the screen.
pub fn fetch(lat: f64, lon: f64) -> Result<WeatherData> {
    let forecast_body = crate::http::get(&build_forecast_url(lat, lon), 16 * 1024)
        .context("forecast request failed")?;
    let forecast: ForecastResponse =
        serde_json::from_str(&forecast_body).context("failed to parse forecast response")?;

    let (us_aqi, uv_index) = fetch_aqi(lat, lon).unwrap_or_else(|e| {
        log::warn!("air-quality fetch failed: {e:#}");
        (None, None)
    });

    let mut daily = Vec::new();
    let days = forecast
        .daily
        .time
        .len()
        .min(forecast.daily.weather_code.len())
        .min(forecast.daily.temperature_2m_max.len())
        .min(forecast.daily.temperature_2m_min.len());
    for i in 0..days {
        daily.push(DailyForecast {
            date: forecast.daily.time[i].clone(),
            high_f: forecast.daily.temperature_2m_max[i] as f32,
            low_f: forecast.daily.temperature_2m_min[i] as f32,
            condition: describe_weather_code(forecast.daily.weather_code[i]),
        });
    }

    Ok(WeatherData {
        temperature_f: forecast.current.temperature_2m as f32,
        feels_like_f: forecast.current.apparent_temperature as f32,
        humidity_pct: forecast.current.relative_humidity_2m as f32,
        wind_mph: forecast.current.wind_speed_10m as f32,
        condition: describe_weather_code(forecast.current.weather_code),
        us_aqi,
        uv_index,
        daily,
        utc_offset_seconds: forecast.utc_offset_seconds,
    })
}

fn fetch_aqi(lat: f64, lon: f64) -> Result<(Option<u16>, Option<f32>)> {
    let body = crate::http::get(&build_air_quality_url(lat, lon), 8 * 1024)?;
    let resp: AirQualityResponse =
        serde_json::from_str(&body).context("failed to parse air-quality response")?;
    Ok((
        resp.current.us_aqi.map(|v| v.round() as u16),
        resp.current.uv_index.map(|v| v as f32),
    ))
}
