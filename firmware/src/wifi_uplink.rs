//! `Uplink` impl: connect WiFi on demand and POST a `SampleBatch` to the backend
//! `/ingest` endpoint over the ESP-IDF HTTP client.
//!
//! BENCH: confirm esp-idf-svc wifi/http constructor signatures against the
//! installed version; the request shape (method, headers, body) is final.

use collector::{SampleBatch, Uplink, UplinkError};
use embedded_svc::http::client::Client as HttpClient;
use embedded_svc::http::Method;
use embedded_svc::io::Write as _; // BENCH: may be `embedded_io::Write` depending on version
use esp_idf_hal::modem::Modem;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{
    AuthMethod, BlockingWifi, ClientConfiguration, Configuration as WifiConfig, EspWifi,
};

pub struct WifiUplink<'d> {
    wifi: BlockingWifi<EspWifi<'d>>,
    ingest_url: String,
    device_key: String,
}

impl<'d> WifiUplink<'d> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        modem: Modem,
        sysloop: EspSystemEventLoop,
        nvs: Option<EspDefaultNvsPartition>,
        ssid: &str,
        pass: &str,
        base_url: &str,
        device_key: &str,
    ) -> anyhow::Result<Self> {
        let esp_wifi = EspWifi::new(modem, sysloop.clone(), nvs)?;
        let mut wifi = BlockingWifi::wrap(esp_wifi, sysloop)?;
        let cfg = WifiConfig::Client(ClientConfiguration {
            ssid: ssid.try_into().map_err(|_| anyhow::anyhow!("ssid too long"))?,
            password: pass.try_into().map_err(|_| anyhow::anyhow!("pass too long"))?,
            auth_method: AuthMethod::WPA2Personal,
            ..Default::default()
        });
        wifi.set_configuration(&cfg)?;
        Ok(Self {
            wifi,
            ingest_url: format!("{}/ingest", base_url.trim_end_matches('/')),
            device_key: device_key.to_string(),
        })
    }

    fn ensure_connected(&mut self) -> Result<(), UplinkError> {
        if self.wifi.is_connected().unwrap_or(false) {
            return Ok(());
        }
        self.wifi.start().map_err(|_| UplinkError::Unreachable)?;
        self.wifi.connect().map_err(|_| UplinkError::Unreachable)?;
        self.wifi.wait_netif_up().map_err(|_| UplinkError::Unreachable)?;
        Ok(())
    }
}

impl<'d> Uplink for WifiUplink<'d> {
    fn push(&mut self, batch: &SampleBatch) -> Result<(), UplinkError> {
        self.ensure_connected()?;

        let body = serde_json::to_vec(batch)
            .map_err(|e| UplinkError::Rejected(format!("serialize: {e}")))?;
        let len = body.len().to_string();

        let conn = EspHttpConnection::new(&HttpConfig {
            use_global_ca_store: true,
            crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
            ..Default::default()
        })
        .map_err(|_| UplinkError::Unreachable)?;
        let mut client = HttpClient::wrap(conn);

        let headers = [
            ("content-type", "application/json"),
            ("content-length", len.as_str()),
            ("x-device-key", self.device_key.as_str()),
        ];
        let mut req = client
            .request(Method::Post, &self.ingest_url, &headers)
            .map_err(|_| UplinkError::Unreachable)?;
        req.write_all(&body).map_err(|_| UplinkError::Unreachable)?;
        req.flush().map_err(|_| UplinkError::Unreachable)?;

        let resp = req.submit().map_err(|_| UplinkError::Unreachable)?;
        let status = resp.status();
        if (200..300).contains(&status) {
            Ok(())
        } else {
            Err(UplinkError::Rejected(format!("HTTP {status}")))
        }
    }
}
