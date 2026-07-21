//! Wi-Fi station management: scanning and connecting.

use anyhow::{Context, Result};
use esp_idf_hal::modem::Modem;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{
    AuthMethod, BlockingWifi, ClientConfiguration, Configuration as WifiConfiguration, EspWifi,
};

/// Owns the Wi-Fi driver for the lifetime of the program.
pub struct Wifi {
    inner: BlockingWifi<EspWifi<'static>>,
}

impl Wifi {
    /// Create and start the Wi-Fi driver in station mode.
    pub fn new(
        modem: Modem<'static>,
        sysloop: EspSystemEventLoop,
        nvs: EspDefaultNvsPartition,
    ) -> Result<Self> {
        let esp_wifi =
            EspWifi::new(modem, sysloop.clone(), Some(nvs)).context("failed to create EspWifi")?;
        let mut inner = BlockingWifi::wrap(esp_wifi, sysloop).context("failed to wrap wifi")?;
        inner
            .set_configuration(&WifiConfiguration::Client(ClientConfiguration::default()))
            .context("failed to set initial wifi configuration")?;
        inner.start().context("failed to start wifi")?;
        Ok(Self { inner })
    }

    /// Scan for nearby access points, returning de-duplicated SSIDs sorted by
    /// descending signal strength.
    pub fn scan_ssids(&mut self) -> Result<Vec<String>> {
        let mut aps = self.inner.scan().context("wifi scan failed")?;
        aps.sort_by_key(|ap| core::cmp::Reverse(ap.signal_strength));
        let mut seen = std::collections::HashSet::new();
        let mut ssids = Vec::new();
        for ap in aps {
            let ssid = ap.ssid.as_str().to_owned();
            if !ssid.is_empty() && seen.insert(ssid.clone()) {
                ssids.push(ssid);
            }
        }
        Ok(ssids)
    }

    /// Connect to the given network and wait until an IP is acquired.
    ///
    /// `auth_method` defaults to WPA2/WPA3 so hybrid networks negotiate cleanly.
    pub fn connect(&mut self, ssid: &str, password: &str) -> Result<()> {
        let auth_method = if password.is_empty() {
            AuthMethod::None
        } else {
            AuthMethod::WPA2WPA3Personal
        };

        let client = ClientConfiguration {
            ssid: ssid
                .try_into()
                .map_err(|_| anyhow::anyhow!("SSID too long (max 32 chars)"))?,
            password: password
                .try_into()
                .map_err(|_| anyhow::anyhow!("password too long (max 64 chars)"))?,
            auth_method,
            ..Default::default()
        };

        self.inner
            .set_configuration(&WifiConfiguration::Client(client))
            .context("failed to set client configuration")?;
        self.inner.connect().context("wifi connect failed")?;
        self.inner
            .wait_netif_up()
            .context("timed out waiting for network interface")?;
        Ok(())
    }

    /// Return the acquired IPv4 address as a string, when connected.
    pub fn ip_info(&self) -> Result<String> {
        let info = self
            .inner
            .wifi()
            .sta_netif()
            .get_ip_info()
            .context("failed to read IP info")?;
        Ok(info.ip.to_string())
    }

    pub fn is_connected(&self) -> bool {
        self.inner.is_connected().unwrap_or(false)
    }
}
