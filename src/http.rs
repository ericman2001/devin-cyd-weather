//! Minimal HTTP(S) GET helper built on the ESP-IDF HTTP client.
//!
//! TLS is handled via the mbedTLS certificate bundle baked into ESP-IDF (see
//! `sdkconfig.defaults`), so the Open-Meteo HTTPS endpoints validate without us
//! shipping any certificates.

use anyhow::{bail, Context, Result};
use embedded_svc::http::Method;
use esp_idf_svc::http::client::{Configuration, EspHttpConnection};

/// Perform an HTTP(S) GET and return the response body as a `String`.
///
/// `max_len` caps the amount of body we buffer to protect the limited RAM.
pub fn get(url: &str, max_len: usize) -> Result<String> {
    let config = Configuration {
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        timeout: Some(core::time::Duration::from_secs(20)),
        buffer_size: Some(4096),
        buffer_size_tx: Some(1024),
        ..Default::default()
    };

    let mut conn = EspHttpConnection::new(&config).context("failed to create HTTP connection")?;

    conn.initiate_request(Method::Get, url, &[("accept", "application/json")])
        .with_context(|| format!("failed to initiate request to {url}"))?;
    conn.initiate_response()
        .context("failed to read HTTP response headers")?;

    let status = conn.status();
    if !(200..300).contains(&status) {
        bail!("HTTP request to {url} returned status {status}");
    }

    let mut body = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        let n = conn
            .read(&mut buf)
            .map_err(|e| anyhow::anyhow!("failed to read HTTP body: {e:?}"))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&buf[..n]);
        if body.len() > max_len {
            bail!("HTTP response from {url} exceeded {max_len} bytes");
        }
    }

    String::from_utf8(body).context("HTTP response was not valid UTF-8")
}
