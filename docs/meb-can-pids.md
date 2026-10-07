# MEB CAN / UDS PID cheat sheet (Enyaq / ID.3 / ID.4 / ID.5)

Source: evDash `CarVWID3.cpp` (verified on real MEB cars). Enyaq shares the MEB
platform, so these apply. Treat as a starting target list to validate against
your own SavvyCAN captures — byte scaling can vary by model year.

## Bus / protocol basics

- Bus: 500 kbit/s CAN on OBD pins 6 (CAN-H) / 14 (CAN-L).
- Service: UDS `22` = ReadDataByIdentifier, followed by a 16-bit DID.
  e.g. `22028C` = read DID `028C` (SoC).
- Addressing: set request header per ECU before sending its DIDs.
  - `17FC00xx` = 29-bit extended headers (battery / drivetrain ECUs).
  - `7xx` = 11-bit standard headers (GPS, climate, energy ECUs).
- Multi-frame (ISO-TP) replies need flow-control. Known MEB multi-frame
  response headers that need FC handling: `5C8 / 5E0 / 5F0`.

## ECU headers

| Header | ECU | Addressing |
|---|---|---|
| `17FC007B` | BMS (battery management) | 29-bit |
| `17FC00B9` | DC-DC converter | 29-bit |
| `17FC0076` | Drivetrain / odometer / VIN | 29-bit |
| `767` (00000767) | Online unit / GPS | 11-bit |
| `746` (00000746) | Climate / HVAC | 11-bit |
| `710` (00000710) | Energy / 12V | 11-bit |

## Core signals (the ones you actually want)

### BMS — header `17FC007B`
| DID | Signal | Unit |
|---|---|---|
| `22028C` | **SoC (BMS)** — 0.4 %/step; whole-% on MY2027 | % |
| `22F40D` | **Speed** (from car, not GPS) | km/h |
| `227448` | Operation mode: 0=standby 1=driving 4=AC charge 6=DC charge | enum |
| `221E3B` | **HV battery voltage** | V |
| `221E3D` | **HV battery current** (→ power = V×I) | A |
| `221E33` / `221E34` | cell # highest / lowest voltage | V |
| `221E0E` / `221E0F` | battery max / min temp (+ point) | °C |
| `222A0B` | battery temp (main value) | °C |
| `221E1B` / `221E1C` | dynamic charge / discharge limit | A |
| `221E32` | total accumulated charged & discharged | — |
| `221E40`…`221E8B` | **per-cell voltages** (cells 1..76) | V |
| `221EAE`…`227426` | battery temp points 1..18 | °C |
| `220500` | HV battery serial | — |

### Drivetrain — header `17FC0076`
| DID | Signal | Unit |
|---|---|---|
| `22295A` | **Odometer** | km |
| `220364` | HV auxiliary consumer power | kW |
| `22210E` | gear position (P/R/N/D/B) | enum |
| `22F802` | VIN | — |

### DC-DC — header `17FC00B9`
| DID | Signal | Unit |
|---|---|---|
| `22465B` | DC-DC current (HV→12V) | A |
| `22465D` | DC-DC voltage | V |

### GPS from car — header `767`  ← no external module needed
| DID | Signal |
|---|---|
| `2222B3` | GPS time (use for RTC sync) |
| `222430` | multiframe: lat, long, altitude, fix quality |
| `222431` | tracked / visible satellite count |

### Climate — header `746`
| DID | Signal | Unit |
|---|---|---|
| `222613` / `222609` | inside / outside temp | °C |
| `22F449` | accelerator pedal position | % |
| `22263B` | recirculation state | enum |
| `2242DB` | interior CO2 | ppm |

### Energy / 12V — header `710`
| DID | Signal | Unit |
|---|---|---|
| `222AB2` | **HV battery max energy content** — key for SoH trend | Wh |
| `222AF7` | 12V battery (multiframe) | — |

## SoH note

No clean single SoH DID on MEB. Forums confirm only `22028C` (SoC) is
reliably documented. Derive health over time from:
- `222AB2` max energy content (Wh) trend — primary degradation signal.
- `221E32` accumulated charge/discharge throughput.
- Full-charge usable capacity integrated from `221E3D` current over sessions.

## Reference implementations

- evDash `CarVWID3.cpp` — full command queue + response parsing (C++).
- Car Scanner ELM327 — community MEB profiles (ID.3/4/5/6, Enyaq, Cupra, Audi).
