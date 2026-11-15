# firmware

ESP32 (Xtensa) firmware for the EV battery tracker. Thin glue over the shared
`collector` + `meb-proto` crates:

- `twai_bus.rs`: `CanBus` over the ESP32 TWAI controller (GPIO4 Rx / GPIO5 Tx, 500 kbit/s).
- `flash_store.rs`: `SampleStore` as newline-JSON on internal flash (LittleFS/SPIFFS).
- `wifi_uplink.rs`: `Uplink`: connect WiFi, `POST /ingest` to the backend.
- `main.rs`: bring-up + poll/flush loop.

All polling/UDS/ISO-TP/stats logic is in `../crates/collector` and is identical
to what the host `ecu-sim` exercises. This crate is **not** part of the host
cargo workspace (it has its own `[workspace]` + `rust-toolchain.toml = esp`).

## Build (bench)

Requires the esp toolchain (`espup` already installed) and ESP-IDF; the first
build downloads the toolchain + IDF and needs network.

```sh
. "$HOME/export-esp.sh"          # puts the esp Rust toolchain on PATH
cd firmware
WIFI_SSID=... WIFI_PASS=... BACKEND_URL=http://<host>:3000 DEVICE_KEY=dev \
  cargo build --release
# flash + monitor (runner configured in .cargo/config.toml):
cargo run --release
```

Build-time env vars (`WIFI_SSID`, `WIFI_PASS`, `BACKEND_URL`, `DEVICE_KEY`) have
safe placeholder defaults so the crate builds without secrets; set them for a
real flash.

## Status

**On-car validated; bench-build only (by design, not in CI).** The firmware is
deliberately kept **outside the host workspace and CI** as it needs ESP-IDF, and
the Xtensa toolchain would collide with the stable x86 build CI runs. So it's
build-verified on the bench rather than in GitHub Actions. Sites that need an API
signature double-checked against the installed esp-idf-hal/svc version are marked
`BENCH:` in the source.

Hardware is assembled and running: the ESP32 + MrDIY CAN shield + transceiver poll
the real Škoda Enyaq over 500 kbit/s CAN. Parked bring-up confirms live UDS: the
core DIDs answer and GPS/time decode straight from the car (see `docs/mvp-spec.md`
Phase 0). Parked-mode hardening (deep-sleep power management + SD-backed
store-and-forward uplink) is in final cleanup.

## Safety

- UDS service `0x22` (ReadDataByIdentifier) only - enforced by `collector`.
- Bus starts listen-only; normal transmit only while a poll is in flight.
