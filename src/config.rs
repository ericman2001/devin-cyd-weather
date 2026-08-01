//! Persistent configuration and tunable constants.
//!
//! Wi-Fi credentials and an optional manual latitude/longitude override are
//! stored in the ESP32 NVS flash partition so they survive reboots. All of the
//! API endpoints and the refresh cadence live here as constants so they are
//! easy to tune in one place.

use anyhow::{Context, Result};
use esp_idf_svc::nvs::{EspNvs, EspNvsPartition, NvsDefault};
use esp_idf_svc::wifi::AuthMethod;

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
const KEY_AUTH: &str = "auth";
const KEY_DEBUG: &str = "debug";
const KEY_LAT: &str = "lat";
const KEY_LON: &str = "lon";

// ---------------------------------------------------------------------------
// Wi-Fi security / authentication method
// ---------------------------------------------------------------------------

/// User-selectable Wi-Fi security type chosen during provisioning.
///
/// [`WifiAuth::Auto`] preserves the historical behaviour (WPA2/WPA3 Personal),
/// which negotiates cleanly on most hybrid networks. The explicit variants let
/// the user pin a specific mode -- notably [`WifiAuth::WpaWpa2Personal`] for
/// older APs that would otherwise time out under the WPA2/WPA3 default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WifiAuth {
    /// Let the driver negotiate; maps to WPA2/WPA3 Personal (legacy default).
    #[default]
    Auto,
    /// WPA/WPA2 Personal (mixed mode) -- for older access points.
    WpaWpa2Personal,
    /// WPA2 Personal only.
    Wpa2Personal,
    /// WPA2/WPA3 Personal (mixed mode).
    Wpa2Wpa3Personal,
    /// Open network (no password).
    Open,
}

impl WifiAuth {
    /// Human-readable label used on the provisioning screen.
    pub fn label(self) -> &'static str {
        match self {
            WifiAuth::Auto => "Auto",
            WifiAuth::WpaWpa2Personal => "WPA/WPA2 Personal",
            WifiAuth::Wpa2Personal => "WPA2 Personal",
            WifiAuth::Wpa2Wpa3Personal => "WPA2/WPA3 Personal",
            WifiAuth::Open => "Open",
        }
    }

    /// All variants in display order (used to build the selection screen).
    pub const ALL: [WifiAuth; 5] = [
        WifiAuth::Auto,
        WifiAuth::WpaWpa2Personal,
        WifiAuth::Wpa2Personal,
        WifiAuth::Wpa2Wpa3Personal,
        WifiAuth::Open,
    ];

    /// Stable string used for NVS persistence.
    fn as_str(self) -> &'static str {
        match self {
            WifiAuth::Auto => "auto",
            WifiAuth::WpaWpa2Personal => "wpa_wpa2",
            WifiAuth::Wpa2Personal => "wpa2",
            WifiAuth::Wpa2Wpa3Personal => "wpa2_wpa3",
            WifiAuth::Open => "open",
        }
    }

    fn from_str(s: &str) -> WifiAuth {
        match s {
            "wpa_wpa2" => WifiAuth::WpaWpa2Personal,
            "wpa2" => WifiAuth::Wpa2Personal,
            "wpa2_wpa3" => WifiAuth::Wpa2Wpa3Personal,
            "open" => WifiAuth::Open,
            _ => WifiAuth::Auto,
        }
    }

    /// Map to the esp-idf-svc [`AuthMethod`]. `Auto` resolves to WPA2/WPA3
    /// Personal to preserve the previous default behaviour.
    pub fn to_auth_method(self) -> AuthMethod {
        match self {
            WifiAuth::Auto | WifiAuth::Wpa2Wpa3Personal => AuthMethod::WPA2WPA3Personal,
            WifiAuth::WpaWpa2Personal => AuthMethod::WPAWPA2Personal,
            WifiAuth::Wpa2Personal => AuthMethod::WPA2Personal,
            WifiAuth::Open => AuthMethod::None,
        }
    }
}

// ---------------------------------------------------------------------------
// Persisted configuration
// ---------------------------------------------------------------------------

/// User configuration persisted across reboots.
#[derive(Debug, Clone)]
pub struct StoredConfig {
    pub ssid: Option<String>,
    pub password: Option<String>,
    /// Wi-Fi security type selected during provisioning.
    pub auth_method: WifiAuth,
    /// Whether serial/USB debug logging is enabled.
    pub serial_debug: bool,
    /// Optional manual latitude/longitude override. When present it takes
    /// precedence over IP-based geolocation.
    pub manual_location: Option<(f64, f64)>,
}

impl Default for StoredConfig {
    fn default() -> Self {
        Self {
            ssid: None,
            password: None,
            auth_method: WifiAuth::default(),
            serial_debug: true,
            manual_location: None,
        }
    }
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
        let auth_method = self
            .get_string(KEY_AUTH)?
            .map(|s| WifiAuth::from_str(&s))
            .unwrap_or_default();
        let serial_debug = self
            .get_string(KEY_DEBUG)?
            .map(|s| matches!(s.as_str(), "1" | "true"))
            .unwrap_or(true);
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
            auth_method,
            serial_debug,
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
        self.nvs
            .set_str(KEY_AUTH, cfg.auth_method.as_str())
            .context("failed to write auth method")?;
        self.nvs
            .set_str(KEY_DEBUG, if cfg.serial_debug { "1" } else { "0" })
            .context("failed to write debug flag")?;
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
        for key in [
            KEY_SSID,
            KEY_PASSWORD,
            KEY_AUTH,
            KEY_DEBUG,
            KEY_LAT,
            KEY_LON,
        ] {
            let _ = self.nvs.remove(key);
        }
        Ok(())
    }
}
