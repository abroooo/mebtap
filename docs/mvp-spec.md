# MVP Spec — EV Battery Tracker (Discovery Phase)

## Purpose

Before building the full stack, determine **what data the Skoda Enyaq exposes
over its OBD-II port and how fast each value updates**. This drives every later
decision (poll cadence, data model, which signals to analyze).

The MVP is a **discovery tool**, not the product. Output of the MVP = a
validated signal catalogue + timing data.

## Goals

1. Bring up CAN on the OBD-II port and communicate with the car.
2. Enumerate which MEB UDS PIDs actually respond on this vehicle.
3. Measure, per responding PID:
   - decoded value + unit (validated against `docs/meb-can-pids.md`),
   - **effective update rate** (how often the value actually changes),
   - response latency / whether multi-frame.
4. Measure **full-pass timing** (time to poll the complete signal set once) on
   the real car.
5. Expose results live on a web page served by the ESP32 (captive portal / AP).

## Non-Goals (explicitly out of scope for MVP)

- No backend, no database, no REST API.
- No long-term storage / SD logging beyond what discovery needs.
- No cloud transfer, no in-car live display app.
- No analysis (averages, SoH trend, driver profiling).
- No final firmware architecture — throwaway/iterative code is fine.
- No Rust requirement for the MVP firmware (see Firmware Language).

## Hardware

| Item | Note |
|---|---|
| MrDIY ESP32 CAN Bus Shield v1.3b | real CAN transceiver, OBD2 connector, buck 7–15.5V, R/RW toggle |
| ESP32 DevKit V1 (30-pin) | **exact variant required** by the shield |
| OBD2 plug | included, assembly (solder) required |
| USB cable | flashing + serial debug |
| Multimeter | verify pin 16 power-off behavior |
| microSD (optional) | only if raw capture needs offloading |

Pins (per shield, **v1.3+**): CAN Rx = GPIO4, Tx = GPIO5 (v1.2 and earlier swap
these). OBD: pin6 CAN-H, pin14 CAN-L, pin16 power, pin4 GND. Shield revision +
MrDIY ESP32RET firmware caveat: see `docs/meb-can-pids.md` → "Shield / firmware".

## Bus / Protocol Facts (established)

- 500 kbit/s CAN.
- OBD port is **gateway-firewalled**: powertrain frames are NOT free-broadcast.
  Data is obtained by **UDS polling** (service `22` ReadDataByIdentifier + DID),
  not passive sniffing.
- Mixed addressing: `17FCxxxx` = 29-bit headers, `7xx` = 11-bit headers.
  Collector must handle both.
- Multi-frame (ISO-TP + flow control) required for GPS, cell voltages, 12V.
  Known MEB multi-frame response headers: `5C8 / 5E0 / 5F0`.
- OBD port power dies shortly after car off → doubles as ignition proxy;
  no deep-sleep logic needed. **Verify the power-off delay with a multimeter.**

## Signal Set Under Test

Reference: `docs/meb-can-pids.md`. Lane model to validate:

**Fast lane (target ~1 Hz):**
- SoC `22028C`, Speed `22F40D`, Pack V `221E3B`, Pack I `221E3D`
- GPS `222430` (multiframe; drop to 0.5 Hz if 1 Hz gets tight)

**Slow lane (~20–30 s) / state:**
- Mode `227448` (state enum: standby/driving/AC/DC — also the lane switcher)
- All temps: `221E0E`/`221E0F`/`222A0B` + 18 points `221EAE`…`227426`
- 76 cell voltages `221E40`…`221E8B`
- Odometer `22295A`, Max energy content `222AB2` (SoH trend)
- DC-DC `22465B`/`22465D`, accumulated throughput `221E32`, limits, aux power

Final cadences are an **output** of the MVP (measured), not an assumption.

## Method / Phases

### Phase 0 — No-code sniff
- Flash ESP32RET to the board; connect SavvyCAN on a laptop.
- Capture in listen-only (R mode) first. Confirm bus bring-up, baud, ACK.
- Flip to RW; issue UDS `22` requests against the reference PID list.
- Confirm which PIDs respond and rough byte layout matches the reference.

### Phase 1 — Discovery firmware
- Custom ESP32 firmware (native TWAI controller):
  - CAN init @ 500 kbit/s, ISO-TP (single + multi-frame + flow control).
  - Cycle ECU headers (`17FC007B`, `17FC0076`, `17FC00B9`, `767`, `746`, `710`).
  - Poll each DID; decode per reference; timestamp every sample.
  - Per-DID stats: response/no-response, latency, multi-frame flag,
    value-change counter → effective update rate.
  - Full-pass timer.
- Captive portal / AP web page: live table of DID → value, unit, update rate,
  latency. Raw-frame dump view for unknown/unmatched responses.

## Safety Constraints (firmware MUST obey)

- **Read-only.** Only UDS service `22` (ReadDataByIdentifier). Never send
  session-control, routine-control, actuator, or write services.
- Default to R (listen-only) mode until actively polling.
- Sane poll rate — do not flood the gateway.
- Rely on port power-off = car-off; do not add logic that keeps the gateway
  awake while parked.

## Acceptance Criteria (MVP is "done" when)

1. Board reads live data from the car over the OBD port.
2. Confirmed list of responding vs non-responding PIDs for this Enyaq.
3. Per-PID: value, unit, measured effective update rate, multi-frame flag.
4. Measured full-pass time for the complete signal set on the real car.
5. Measured OBD pin-16 power-off delay after ignition off.
6. All of the above viewable on the ESP32-served web page.

## Outputs (feed the next design stage)

- Validated signal catalogue (which signals exist + real update rates).
- Real-world poll-bandwidth budget (full-pass timing).
- Confirmed lane assignment + achievable fast-lane cadence.
- Confidence on GPS-from-car (avoids external GPS module).

## Firmware Language

- Full stack target is **Rust** (backend, analysis, frontend).
- MVP firmware language is **open**: Arduino/ESP-IDF (C/C++) is the low-friction
  path and lets us reuse evDash logic directly; Rust on ESP32 (`esp-hal` /
  `esp-idf-hal` + `embassy`) is viable but TWAI/ISO-TP support is rougher.
  Decision deferred — MVP is throwaway, so optimize for fastest path to the
  signal catalogue.

## Reference

- evDash `CarVWID3.cpp` — MEB request sequences, ISO-TP handling, byte scaling.
  Used as **specification/inspiration**, not a dependency.
- `docs/meb-can-pids.md` — extracted PID/header map.
