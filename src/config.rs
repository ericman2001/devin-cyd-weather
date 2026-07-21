//! Persistent configuration and tunable constants.
//!
//! Wi-Fi credentials and an optional manual latitude/longitude override are
//! stored in the ESP32 NVS flash partition so they survive reboots. All of the
//! API endpoints and the refresh cadence live here as constants so they are
//! easy to tune in one place.

use anyhow::{Context, Result};
use esp_idf_svc::nvs::{EspNvs, EspNvsPartition, NvsDefault};

// ---------------------------------------------------------------------------
// Tunable constants
// ---------------------------------------------------------------------------

/// How often the weather + air-quality data is refreshed.
pub const REFRESH_INTERVAL_SECS: u64 = 30 * 60; // 30 minutes

/// How long to wait before retrying after a failed refresh.
pub const RETRY_INTERVAL_SECS: u64 = 60; // 1 minute

/// How long the backlight stays lit after a screen tap on the weather display.
pub const BACKLIGHT_ON_SECS: u64 = 60; // 1 minute

/// Open-Meteo forecast API base URL (HTTPS / TLS).
pub const FORECAST_API_BASE: &str = "https://api.open-meteo.com/v1/forecast";

/// Open-Meteo air-quality API base URL (HTTPS / TLS).
pub const AIR_QUALITY_API_BASE: &str = "https://air-quality-api.open-meteo.com/v1/air-quality";

/// IP-based geolocation API (plain HTTP). Returns lat/long for the public IP.
pub const GEOLOCATION_API: &str = "http://ip-api.com/json/";

/// Number of forecast days to request / render.
pub const FORECAST_DAYS: usize = 4;

// NVS namespace + key names.
const NVS_NAMESPACE: &str = "cydweather";
const KEY_SSID: &str = "ssid";
const KEY_PASSWORD: &str = "password";
const KEY_LAT: &str = "lat";
const KEY_LON: &str = "lon";

// ---------------------------------------------------------------------------
// Persisted configuration
// ---------------------------------------------------------------------------

/// User configuration persisted across reboots.
#[derive(Debug, Clone, Default)]
pub struct StoredConfig {
    pub ssid: Option<String>,
    pub password: Option<String>,
    /// Optional manual latitude/longitude override. When present it takes
    /// precedence over IP-based geolocation.
    pub manual_location: Option<(f64, f64)>,
}

impl StoredConfig {
    /// Returns `true` when we have enough to attempt a Wi-Fi connection.
    pub fn has_wifi(&self) -> bool {
        self.ssid.as_deref().map(|s| !s.is_empty()).unwrap_or(false)
    }
}

/// Thin wrapper around the NVS handle used to persist [`StoredConfig`].
pub struct ConfigStore {
    nvs: EspNvs<NvsDefault>,
}

impl ConfigStore {
    /// Open (or create) the configuration namespace in the default NVS
    /// partition.
    pub fn new(partition: EspNvsPartition<NvsDefault>) -> Result<Self> {
        let nvs =
            EspNvs::new(partition, NVS_NAMESPACE, true).context("failed to open NVS namespace")?;
        Ok(Self { nvs })
    }

    fn get_string(&self, key: &str) -> Result<Option<String>> {
        // 256 bytes comfortably covers an SSID (<=32) or a password (<=64).
        let mut buf = [0u8; 256];
        let value = self
            .nvs
            .get_str(key, &mut buf)
            .with_context(|| format!("failed to read NVS key {key}"))?;
        Ok(value.map(|s| s.to_owned()))
    }

    /// Load the stored configuration, returning defaults when nothing is set.
    pub fn load(&self) -> Result<StoredConfig> {
        let ssid = self.get_string(KEY_SSID)?.filter(|s| !s.is_empty());
        let password = self.get_string(KEY_PASSWORD)?;
        let lat = self
            .get_string(KEY_LAT)?
            .and_then(|s| s.parse::<f64>().ok());
        let lon = self
            .get_string(KEY_LON)?
            .and_then(|s| s.parse::<f64>().ok());
        let manual_location = match (lat, lon) {
            (Some(la), Some(lo)) => Some((la, lo)),
            _ => None,
        };
        Ok(StoredConfig {
            ssid,
            password,
            manual_location,
        })
    }

    /// Persist Wi-Fi credentials and an optional manual location.
    pub fn save(&mut self, cfg: &StoredConfig) -> Result<()> {
        self.nvs
            .set_str(KEY_SSID, cfg.ssid.as_deref().unwrap_or(""))
            .context("failed to write SSID")?;
        self.nvs
            .set_str(KEY_PASSWORD, cfg.password.as_deref().unwrap_or(""))
            .context("failed to write password")?;
        match cfg.manual_location {
            Some((lat, lon)) => {
                self.nvs.set_str(KEY_LAT, &lat.to_string())?;
                self.nvs.set_str(KEY_LON, &lon.to_string())?;
            }
            None => {
                let _ = self.nvs.remove(KEY_LAT);
                let _ = self.nvs.remove(KEY_LON);
            }
        }
        Ok(())
    }

    /// Erase all stored configuration (used by the "reset config" flow).
    pub fn clear(&mut self) -> Result<()> {
        for key in [KEY_SSID, KEY_PASSWORD, KEY_LAT, KEY_LON] {
            let _ = self.nvs.remove(key);
        }
        Ok(())
    }
}
