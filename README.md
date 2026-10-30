# EV Battery Tracker

Telemetry logger for the **Škoda Enyaq** (VW **MEB** platform). An ESP32 on the
car's OBD-II port polls the vehicle over CAN/UDS, buffers samples on flash, and
forwards them to a Rust backend for storage and analysis — State of Charge, SoH
trend, pack voltage/current, cell voltages, temperatures, and GPS straight from
the car.

> **Status:** host stack complete and tested. The CAN transceiver + board aren't
> assembled yet, so the *entire* collect → decode → store → uplink pipeline is
> developed and unit-tested on a laptop against a simulated car. Real drivers
> drop in when the hardware arrives — **no logic changes**.

---

## Why this design

**Firmware is thin glue. All logic lives in host-testable crates behind traits.**

The ESP32 only supplies concrete implementations (TWAI, flash, WiFi). Everything
that matters — the UDS/ISO-TP client, the lane scheduler, the discovery stats,
the sample schema — is plain `std` Rust that compiles and runs on a laptop. A
host simulator (`ecu-sim`) runs the *exact* collector code the ESP32 runs, against
an in-memory mock MEB ECU. So we build and validate the real system before the
transceiver is even in hand.

```
      ┌──────────────┐        CAN / UDS 0x22         ┌──────────────┐
      │   Škoda      │  ◄───────────────────────────►│    ESP32     │
      │   Enyaq      │     (listen-only until poll)   │  (thin glue) │
      └──────────────┘                                └──────┬───────┘
                                                             │ batch POST (TLS)
         swap in for the real car on the host               ▼
      ┌──────────────┐        same collector code     ┌──────────────┐
      │   ecu-sim    │  ◄───────────────────────────► │   backend    │
      │ (mock ECU)   │                                 │ axum + TSDB  │
      └──────────────┘                                 └──────────────┘
```

## Workspace layout

```
crates/
  meb-proto/   DID catalogue, per-signal decoders, Sample schema (serde).
  collector/   Transport traits + UDS/ISO-TP client + lane scheduler + stats.
  ecu-sim/     In-memory mock MEB ECU; runs collector end-to-end on the host.
backend/       axum ingest + TimescaleDB query API (sqlx).
firmware/      ESP32 (esp-idf-hal, std) glue. SEPARATE crate — own esp toolchain.
docs/          architecture.md, mvp-spec.md, meb-can-pids.md (authoritative).
```

`firmware/` is **excluded from the host workspace** so the Xtensa (esp) toolchain
and the x86 stable toolchain never collide. It reuses `meb-proto` + `collector`
by path dependency: same logic, different concrete drivers.

### The crates

| Crate | What it does |
|---|---|
| **`meb-proto`** | Single source of truth for the wire format. DID catalogue (SoC, Speed, PackV/I, GPS, temps, cell voltages, odometer, max-energy/SoH, DC-DC, 12 V…), per-signal scaling, and the `Sample`/`SampleBatch` serde schema shared by firmware, sim, and backend. |
| **`collector`** | Pure logic, no ESP/host deps. Defines the seams as traits — `CanBus`, `SampleStore`, `Uplink` — plus a UDS service-`0x22` client with ISO-TP single/multi-frame + flow control, a fast/slow `LaneScheduler`, and `DidStats` (response rate, latency, multi-frame flag, effective update rate, full-pass timer). |
| **`ecu-sim`** | `MemCanBus` (paired in-memory channels) + `MebEcuSim` answering canned UDS responses, including a flow-controlled multi-frame GPS reply and configurable non-responders. Binary prints the live discovery table and can POST to the backend. |
| **`backend`** | `axum` ingest + TimescaleDB hypertables via `sqlx`. Device-key auth. Query API for DIDs / latest / time series. Uses runtime queries so `cargo check` works without a live DB. |
| **`firmware`** | ESP32 drivers: `TwaiCanBus` (TWAI, 500 kbit/s), `FlashStore` (LittleFS), `WifiUplink` (TLS batch POST). `main` wires them into `collector`. The only non-host-tested code. |

## Quick start

```sh
# Host simulator — collector ↔ mock MEB ECU, prints the discovery table.
cargo run -p ecu-sim

# Full host stack, end to end: sim → backend → query.
docker-compose up -d                                          # TimescaleDB, host :5433
cat backend/migrations/*.sql | docker-compose exec -T timescaledb \
  psql -U ev -d ev_tracker                                    # schema (no local psql)
DATABASE_URL=postgres://ev:ev@localhost:5433/ev_tracker \
  DEVICE_KEY=dev cargo run -p backend &                       # API on :3000
cargo run -p ecu-sim -- --uplink http://localhost:3000 --device-key dev
curl 'http://localhost:3000/latest'
```

```sh
cargo test --workspace     # meb-proto + collector + ecu-sim + backend
```

### Backend API

| Method | Path | Notes |
|---|---|---|
| `GET`  | `/health` | liveness |
| `POST` | `/ingest` | body = `SampleBatch` JSON; header `x-device-key` required |
| `GET`  | `/dids` | distinct DIDs seen |
| `GET`  | `/latest` | latest value per DID |
| `GET`  | `/series?did=&from=&to=` | time range (RFC3339) |

Env: `DATABASE_URL` (compose publishes TimescaleDB on host **:5433** →
`postgres://ev:ev@localhost:5433/ev_tracker`; code default is `:5432`),
`DEVICE_KEY` (default `dev`), `BIND` (default `0.0.0.0:3000`).

### Firmware (bench build only)

Needs the esp toolchain + ESP-IDF — **not built in CI**. See
[`firmware/README.md`](firmware/README.md).

```sh
. "$HOME/export-esp.sh"
cd firmware
WIFI_SSID=... WIFI_PASS=... BACKEND_URL=http://<host>:3000 DEVICE_KEY=dev \
  cargo build --release
```

## How the car actually talks

- **500 kbit/s CAN.** The OBD port is **gateway-firewalled** — powertrain frames
  are *not* free-broadcast. Data comes from **UDS polling** (service `0x22`
  ReadDataByIdentifier + DID), not passive sniffing.
- **Mixed addressing:** `17FCxxxx` = 29-bit headers (BMS/DCDC/Drivetrain),
  `7xx` = 11-bit (Online/Climate/Energy). The collector handles both.
- **Multi-frame (ISO-TP + flow control)** for GPS, cell voltages, 12 V.
- **Two lanes:** fast (~1 Hz: SoC, speed, pack V/I, GPS) and slow (~20–30 s:
  temps, 76 cell voltages, odometer, SoH, DC-DC), switched by the drive-mode DID.
- OBD-port power dies shortly after ignition-off → doubles as an ignition proxy,
  no deep-sleep logic needed.

## Safety

The firmware is **read-only by construction**:

- Only UDS service **`0x22`** (ReadDataByIdentifier) is exposed. There is
  deliberately *no* API for session-control, routine, actuator, or write
  services — enforced in `collector`, not just by convention.
- The bus starts **listen-only** and only transmits while a poll is in flight.
- Sane poll cadence; never keeps the gateway awake while parked.

## Docs

- [`docs/architecture.md`](docs/architecture.md) — authoritative system design.
- [`docs/mvp-spec.md`](docs/mvp-spec.md) — discovery-phase goals + acceptance.
- [`docs/meb-can-pids.md`](docs/meb-can-pids.md) — PID/header map + CAN shield notes.

## Roadmap

1. Bench-build `firmware/`; resolve any `BENCH:`-marked esp-idf API sites.
2. Stand up TimescaleDB + run the real sim → backend → `/latest` loop.
3. Hardware arrives → Phase 0 SavvyCAN sniff, flash firmware, validate DIDs on
   the real Enyaq, and feed measured update rates back into the lane assignments.
