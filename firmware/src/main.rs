//! ESP32 firmware — thin glue that wires the real TWAI / flash / WiFi drivers
//! into the shared `collector` engine. All polling/decoding/ISO-TP logic lives
//! in the `collector` + `meb-proto` crates (identical to what the host simulator
//! exercises), so this file is only hardware bring-up and trait impls.
//!
//! Status: BUILD-VERIFIED ON BENCH ONLY. Not compiled in CI (needs the ESP-IDF
//! toolchain). A few ESP-IDF API call sites are marked `BENCH:` where the exact
//! signature must be confirmed against the installed esp-idf-hal/svc version on
//! first flash.
//!
//! Safety: UDS service 0x22 only (enforced by `collector`), and the bus starts
//! listen-only; it is switched to normal transmit only while a poll is in flight.

mod flash_store;
mod twai_bus;
mod wifi_uplink;

use std::time::Duration;

use collector::{Collector, LaneScheduler, Uplink};
use esp_idf_hal::peripherals::Peripherals;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

use flash_store::FlashStore;
use twai_bus::TwaiCanBus;
use wifi_uplink::WifiUplink;

// Provisioned via build-time env vars (later: NVS). Placeholders keep the
// scaffold buildable without secrets.
const WIFI_SSID: &str = match option_env!("WIFI_SSID") { Some(s) => s, None => "CHANGEME" };
const WIFI_PASS: &str = match option_env!("WIFI_PASS") { Some(s) => s, None => "CHANGEME" };
const BACKEND_URL: &str = match option_env!("BACKEND_URL") { Some(s) => s, None => "http://192.168.0.10:3000" };
const DEVICE_KEY: &str = match option_env!("DEVICE_KEY") { Some(s) => s, None => "dev" };
const DEVICE_ID: &str = "enyaq-esp32";

const FAST_INTERVAL: Duration = Duration::from_millis(1000);
const SLOW_INTERVAL: Duration = Duration::from_secs(25);
const UPLINK_EVERY_PASSES: u32 = 20;
const UPLINK_BATCH: usize = 256;

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // Local store on internal flash (LittleFS/SPIFFS mounted at /storage).
    // BENCH: mount the VFS partition before first append (see FlashStore::mount).
    let store = FlashStore::mount("/storage/pending.ndjson")?;

    // TWAI on GPIO4 (Rx) / GPIO5 (Tx) @ 500 kbit/s, listen-only until polling.
    let bus = TwaiCanBus::new(
        peripherals.can,
        peripherals.pins.gpio5, // Tx
        peripherals.pins.gpio4, // Rx
    )?;

    let scheduler = LaneScheduler::new(FAST_INTERVAL, SLOW_INTERVAL);
    let mut collector = Collector::new(bus, store, scheduler, DEVICE_ID);

    // WiFi uplink; connection is attempted lazily on each flush.
    let mut uplink = WifiUplink::new(
        peripherals.modem,
        sysloop,
        Some(nvs),
        WIFI_SSID,
        WIFI_PASS,
        BACKEND_URL,
        DEVICE_KEY,
    )?;

    log::info!("collector up; polling MEB over OBD (listen-only until poll)");

    let mut passes: u32 = 0;
    loop {
        let n = collector.poll_once();
        if n > 0 {
            passes = passes.wrapping_add(1);
        }

        if passes % UPLINK_EVERY_PASSES == 0 {
            flush(&mut collector, &mut uplink);
        }

        // Light pacing; LaneScheduler enforces real cadence.
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Drain the local store and push to the backend when reachable. On failure the
/// samples stay buffered (store-and-forward).
fn flush(collector: &mut Collector<TwaiCanBus, FlashStore>, uplink: &mut WifiUplink) {
    while let Some(batch) = collector.take_batch(UPLINK_BATCH) {
        match uplink.push(&batch) {
            Ok(()) => log::info!("uplinked {} samples", batch.samples.len()),
            Err(e) => {
                // Re-buffer and stop; try again next window.
                collector.store.requeue(&batch.samples);
                log::warn!("uplink failed ({e:?}); {} samples buffered", collector.store.pending());
                break;
            }
        }
    }
}
