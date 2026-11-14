# firmware

ESP32 (Xtensa) firmware for the EV battery tracker. Thin glue over the shared
`collector` + `meb-proto` crates:

- `twai_bus.rs` — `CanBus` over the ESP32 TWAI controller (GPIO4 Rx / GPIO5 Tx, 500 kbit/s).
- `flash_store.rs` — `SampleStore` as newline-JSON on internal flash (LittleFS/SPIFFS).
- `wifi_uplink.rs` — `Uplink`: connect WiFi, `POST /ingest` to the backend.
- `main.rs` — bring-up + poll/flush loop.

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

**Build-verified on bench only** — deliberately **outside the host workspace and
CI** (needs ESP-IDF; the Xtensa toolchain would collide with the stable x86 build
the CI runs). Sites that
need an API signature double-check against the installed esp-idf-hal/svc version
are marked `BENCH:` in the source. Runtime behaviour is validated once the board
+ MrDIY CAN shield + transceiver are assembled (see `docs/mvp-spec.md` Phase 0).

## Safety

- UDS service `0x22` (ReadDataByIdentifier) only — enforced by `collector`.
- Bus starts listen-only; normal transmit only while a poll is in flight.
