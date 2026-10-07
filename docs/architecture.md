# Architecture — EV Battery Tracker (real stack)

Supersedes the throwaway MVP firmware in `mvp-spec.md`. The discovery goals of
the MVP (validate PIDs, measure update rates, full-pass timing) are folded into
the `collector`'s stats layer so we keep them while building the real system.

## Principle

**Firmware is thin glue. All logic lives in host-testable crates behind traits.**
The ESP32 only provides concrete impls (TWAI, flash, WiFi). This lets us develop
and unit-test the entire collect→decode→store→uplink pipeline on a laptop with a
simulated car **before the CAN transceiver arrives**, then drop the real drivers
in with no logic changes.

## Decisions (locked 2026-10-07)

| Decision | Choice | Why |
|---|---|---|
| Firmware language | Rust, `esp-idf-hal` (**std**) | TLS + filesystem + WiFi + TWAI all mature via ESP-IDF; `no_std`/embassy buys nothing at 1 Hz polling. |
| Dev without hardware | Host simulator + transport abstraction | `ecu-sim` runs the real `collector` code against a mock MEB ECU. |
| FW ↔ backend | Store-and-forward: device flash → REST batch POST when WiFi reachable | Robust to no-connectivity-in-car. |
| Device storage | LittleFS on internal flash now (~1–3 MB), SD later behind same trait | ~290 KB/hr fast lane fits a drive; SD only for long WiFi gaps. |
| Backend | Rust `axum` + TimescaleDB (Postgres) via `sqlx` | Rust end-goal; time-series purpose-built; SQL-friendly. |

## Workspace layout

```
/ (host cargo workspace, stable toolchain)
  Cargo.toml                 # members: crates/*, backend  (firmware is NOT a member)
  crates/
    meb-proto/               # DID catalogue, decoders, Sample schema (serde). std, minimal deps.
    collector/               # traits + polling engine + stats. Host-unit-testable.
    ecu-sim/                  # in-memory CanBus + mock MEB ECU; host end-to-end runner.
  backend/                   # axum ingest + TimescaleDB + query API.
  firmware/                  # SEPARATE crate + rust-toolchain.toml(esp) + .cargo/config.
                             # depends on ../crates/meb-proto, ../crates/collector by path.
  docker-compose.yml         # TimescaleDB for local dev.
  docs/
```

`firmware/` is excluded from the host workspace so the esp (Xtensa) toolchain
and the x86 stable toolchain never collide. It reuses `meb-proto` + `collector`
by path dependency — same logic, different concrete impls.

## Crates

### `meb-proto`
- DID catalogue from `docs/meb-can-pids.md`: `Did`, ECU header, name, unit, lane.
- `decode(did, &[u8]) -> DecodedValue` per-signal scaling (SoC 0.4%/step, etc.).
- `Sample { did, ts, raw: Vec<u8>, value: DecodedValue }` + serde for the uplink
  wire format and the backend ingest body. Single source of truth for the schema.

### `collector`
Pure logic, no ESP/host deps. Defines the architecture seams as traits:
- `CanBus`: `send(frame)`, `recv(timeout) -> Option<CanFrame>`. 11-bit + 29-bit IDs.
- `Uds22Client` over `CanBus`: ISO-TP single + multi-frame + flow control.
  **Only service `22` (ReadDataByIdentifier).** No write/session/routine services.
- `SampleStore`: `append(&[Sample])`, `drain_batch(max) -> Vec<Sample>`, `pending()`.
- `Uplink`: `push(&[Sample]) -> Result`.
- `LaneScheduler`: fast lane (~1 Hz) / slow lane (~20–30 s), mode-driven switch
  via DID `227448`.
- `DidStats`: response/no-response, latency, multi-frame flag, value-change
  counter → effective update rate; `FullPassTimer`. (= the MVP discovery output.)
- Safety: defaults to listen-only (R) until a poll is actively issued.

### `ecu-sim` (host dev tool)
- `MemCanBus`: paired in-memory channels implementing `CanBus` both directions.
- `MebEcuSim`: answers UDS `22` for catalogue DIDs with canned responses,
  including a scripted multi-frame GPS reply and configurable non-responders,
  so we can validate ISO-TP + the discovery stats without a car.
- Binary: runs `collector` against the sim, prints the DID→value/rate/latency
  discovery table, optionally POSTs to the backend (exercises `Uplink`).

### `backend`
- `axum`: `POST /ingest` (batch of `meb-proto::Sample`), auth token (device key).
- TimescaleDB hypertable for samples; raw-frame table for unknown responses.
- Query: `GET /dids`, `GET /latest`, `GET /series?did=..&from=..&to=..`.
- `sqlx` with offline metadata so `cargo check` works without a live DB.
- `docker-compose.yml` brings up TimescaleDB for local dev.

### `firmware` (ESP32, build-verified manually)
- `TwaiCanBus`: ESP-IDF TWAI driver, GPIO4 Rx / GPIO5 Tx, 500 kbit/s,
  listen-only by default, RW only while polling.
- `FlashStore`: LittleFS `SampleStore` impl (SD swap-in later).
- `WifiUplink`: connects when SSID reachable, batch `POST /ingest` via TLS.
- `main`: wire the three impls into `collector`. ~the only non-host-tested code.

## Test strategy without hardware
- `meb-proto`: unit tests for every decoder against known byte layouts.
- `collector`: unit tests with a mock `CanBus` (incl. multi-frame + non-responder).
- `ecu-sim`: end-to-end host run collector↔sim; integration test asserting the
  discovery table + that samples reach an in-memory/`backend` `Uplink`.
- `firmware`: `cargo build` with the esp toolchain only; runtime verified on the
  bench once the transceiver + board are assembled.
