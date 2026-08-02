//! IP-based geolocation.
//!
//! After Wi-Fi connects, if the user has not set a manual latitude/longitude
//! override we ask a public IP-geolocation API where we are.

use anyhow::{bail, Context, Result};
use esp_idf_hal::delay::FreeRtos;
use serde::Deserialize;

use crate::config::GEOLOCATION_API;

/// DNS is not necessarily usable the moment the station gets its lease, so the
/// lookup is retried rather than falling back to the wrong side of the country.
const ATTEMPTS: usize = 4;
const RETRY_DELAY_MS: u32 = 3000;

#[derive(Debug, Deserialize)]
struct IpApiResponse {
    status: String,
    lat: Option<f64>,
    lon: Option<f64>,
    city: Option<String>,
    #[serde(rename = "regionName")]
    region_name: Option<String>,
    message: Option<String>,
}

/// A resolved geographic location.
#[derive(Debug, Clone)]
pub struct Location {
    pub latitude: f64,
    pub longitude: f64,
    /// Human readable label (e.g. "Austin, Texas"), when available.
    pub label: Option<String>,
}

/// Resolve the current location from the public IP address, retrying while the
/// resolver settles after the connection comes up.
pub fn resolve_from_ip() -> Result<Location> {
    let mut last = None;
    for attempt in 1..=ATTEMPTS {
        match lookup() {
            Ok(location) => return Ok(location),
            Err(e) => {
                log::warn!("geolocation attempt {attempt}/{ATTEMPTS} failed: {e:#}");
                last = Some(e);
            }
        }
        if attempt < ATTEMPTS {
            FreeRtos::delay_ms(RETRY_DELAY_MS);
        }
    }
    Err(last.expect("at least one attempt is made"))
}

fn lookup() -> Result<Location> {
    let body = crate::http::get(GEOLOCATION_API, 4096).context("geolocation request failed")?;
    let resp: IpApiResponse =
        serde_json::from_str(&body).context("failed to parse geolocation response")?;

    if resp.status != "success" {
        bail!(
            "geolocation API error: {}",
            resp.message.unwrap_or_else(|| "unknown".into())
        );
    }

    let latitude = resp.lat.context("geolocation response missing latitude")?;
    let longitude = resp.lon.context("geolocation response missing longitude")?;

    let label = match (resp.city, resp.region_name) {
        (Some(c), Some(r)) => Some(format!("{c}, {r}")),
        (Some(c), None) => Some(c),
        (None, Some(r)) => Some(r),
        (None, None) => None,
    };

    Ok(Location {
        latitude,
        longitude,
        label,
    })
}
