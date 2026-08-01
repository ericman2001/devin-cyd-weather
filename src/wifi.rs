//! Wi-Fi station management: scanning and connecting.

use anyhow::{Context, Result};
use esp_idf_hal::modem::Modem;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{
    AuthMethod, BlockingWifi, ClientConfiguration, Configuration as WifiConfiguration, EspWifi,
};

use crate::config::WifiAuth;

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
        log::info!("wifi scan found {} access point(s)", aps.len());
        aps.sort_by_key(|ap| core::cmp::Reverse(ap.signal_strength));
        let mut seen = std::collections::HashSet::new();
        let mut ssids = Vec::new();
        for ap in aps {
            let ssid = ap.ssid.as_str().to_owned();
            if !ssid.is_empty() && seen.insert(ssid.clone()) {
                ssids.push(ssid);
            }
        }
        log::debug!("wifi scan yielded {} unique SSID(s)", ssids.len());
        Ok(ssids)
    }

    /// Connect to the given network and wait until an IP is acquired.
    ///
    /// The `auth` argument selects the security type chosen during
    /// provisioning. When the user picked [`WifiAuth::Open`] or
    /// [`WifiAuth::Auto`] and left the password empty, [`AuthMethod::None`] is
    /// used; otherwise the chosen method is applied verbatim.
    pub fn connect(&mut self, ssid: &str, password: &str, auth: WifiAuth) -> Result<()> {
        let auth_method = if password.is_empty() && matches!(auth, WifiAuth::Open | WifiAuth::Auto)
        {
            AuthMethod::None
        } else {
            auth.to_auth_method()
        };

        log::info!(
            "wifi connect: ssid={ssid:?} auth={} -> {auth_method:?}",
            auth.label()
        );

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

        log::debug!("wifi: setting client configuration");
        self.inner
            .set_configuration(&WifiConfiguration::Client(client))
            .context("failed to set client configuration")?;
        log::debug!("wifi: calling connect()");
        self.inner.connect().context("wifi connect failed")?;
        log::debug!("wifi: waiting for netif up");
        if let Err(e) = self.inner.wait_netif_up() {
            log::warn!("wifi: timed out waiting for netif up: {e:#}");
            return Err(e).context("timed out waiting for network interface");
        }
        match self.ip_info() {
            Ok(ip) => log::info!("wifi connected, acquired IP {ip}"),
            Err(e) => log::warn!("wifi connected but failed to read IP: {e:#}"),
        }
        Ok(())
    }

    /// Stop the Wi-Fi driver, releasing the tens of kilobytes of heap it holds.
    ///
    /// The radar decoder needs a 32 KB contiguous block for its zlib window,
    /// which is not available while the station is up; the caller stops the
    /// radio for the decode and brings it back with [`Wifi::start`].
    pub fn stop(&mut self) -> Result<()> {
        self.inner.stop().context("failed to stop wifi")?;
        log::info!("wifi stopped to free heap");
        Ok(())
    }

    /// Restart the Wi-Fi driver after a [`Wifi::stop`]. The saved
    /// configuration is retained, but the station still has to reconnect.
    pub fn start(&mut self) -> Result<()> {
        self.inner.start().context("failed to restart wifi")?;
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
        let connected = self.inner.is_connected().unwrap_or(false);
        log::debug!("wifi is_connected -> {connected}");
        connected
    }
}
