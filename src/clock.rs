//! Wall-clock time via SNTP.
//!
//! The board has no RTC, so the forecast radar (whose tiles are addressed by
//! forecast minute relative to the model run) needs the network time to work
//! out which frames are "now" and later. SNTP also lets the radar screen label
//! each frame with a real clock time.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use esp_idf_svc::sntp::{EspSntp, SyncStatus};

/// Any timestamp below this (2023-01-01) means the clock has not been set.
const EPOCH_SANITY: i64 = 1_672_531_200;

/// Owns the SNTP client; dropping it stops the periodic resynchronisation.
pub struct Clock {
    sntp: EspSntp<'static>,
}

impl Clock {
    /// Start SNTP against the default pool servers.
    pub fn start() -> Result<Self> {
        let sntp = EspSntp::new_default().context("failed to start SNTP")?;
        Ok(Self { sntp })
    }

    /// Block until the first synchronisation lands, or `timeout` elapses.
    /// Returns whether the clock is usable.
    pub fn wait_for_sync(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if self.sntp.get_sync_status() == SyncStatus::Completed {
                log::info!("clock synchronised: {:?}", now_unix());
                return true;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        log::warn!("clock did not synchronise within {timeout:?}");
        false
    }
}

/// The current Unix timestamp, or `None` while the clock is unset.
pub fn now_unix() -> Option<i64> {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    (secs > EPOCH_SANITY).then_some(secs)
}
