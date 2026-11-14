//! Shared protocol types for MEB CAN/UDS telemetry.

use serde::{Deserialize, Serialize};

// ── ECU headers ──────────────────────────────────────────────────────────────

/// ECU request header used to address a specific ECU before polling DIDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EcuHeader {
    /// Hex header value (e.g. 0x17FC007B for BMS).
    pub value: u32,
    /// Human-readable name.
    pub name: &'static str,
    /// 11-bit (standard) or 29-bit (extended) CAN addressing.
    pub bits: CanBits,
}

/// CAN address bit-width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CanBits {
    Std11,
    Ext29,
}

/// Known ECUs on the MEB platform.
pub const ECUS: &[EcuHeader] = &[
    EcuHeader { value: 0x17FC007B, name: "BMS", bits: CanBits::Ext29 },
    EcuHeader { value: 0x17FC00B9, name: "DCDC", bits: CanBits::Ext29 },
    EcuHeader { value: 0x17FC0076, name: "Drivetrain", bits: CanBits::Ext29 },
    EcuHeader { value: 0x00000767, name: "Online", bits: CanBits::Std11 },
    EcuHeader { value: 0x00000746, name: "Climate", bits: CanBits::Std11 },
    EcuHeader { value: 0x00000710, name: "Energy", bits: CanBits::Std11 },
];

/// Resolve an ECU by header value.
pub fn ecu_by_header(value: u32) -> Option<&'static EcuHeader> {
    ECUS.iter().find(|e| e.value == value)
}

// ── Lane ─────────────────────────────────────────────────────────────────────

/// Polling lane assignment for a DID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Lane {
    Fast, // ~1 Hz
    Slow, // ~20-30 s
}

// ── DID catalogue ────────────────────────────────────────────────────────────

/// A single DID entry in the catalogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Did {
    /// DID code as it appears in the UDS service 22 request (e.g. 0x028C).
    pub code: u16,
    /// ECU header this DID belongs to.
    pub ecu: EcuHeader,
    /// Human-readable signal name.
    pub name: &'static str,
    /// Unit string (e.g. "%", "V", "A", "km/h").
    pub unit: &'static str,
    /// Polling lane.
    pub lane: Lane,
    /// Whether the response is multi-frame (ISO-TP).
    pub multi_frame: bool,
}

/// Full catalogue of core signals from docs/meb-can-pids.md.
pub const DIDS: &[Did] = &[
    // ── BMS (17FC007B) ──────────────────────────────────────────────────────
    Did {
        code: 0x028C,
        ecu: ECUS[0],
        name: "SoC",
        unit: "%",
        lane: Lane::Fast,
        multi_frame: false,
    },
    Did {
        code: 0xF40D,
        ecu: ECUS[0],
        name: "Speed",
        unit: "km/h",
        lane: Lane::Fast,
        multi_frame: false,
    },
    Did {
        code: 0x7448,
        ecu: ECUS[0],
        name: "Mode",
        unit: "",
        lane: Lane::Fast,
        multi_frame: false,
    },
    Did {
        code: 0x1E3B,
        ecu: ECUS[0],
        name: "PackV",
        unit: "V",
        lane: Lane::Fast,
        multi_frame: false,
    },
    Did {
        code: 0x1E3D,
        ecu: ECUS[0],
        name: "PackI",
        unit: "A",
        lane: Lane::Fast,
        multi_frame: false,
    },
    Did {
        code: 0x1E0E,
        ecu: ECUS[0],
        name: "TempMax",
        unit: "°C",
        lane: Lane::Slow,
        multi_frame: false,
    },
    Did {
        code: 0x1E0F,
        ecu: ECUS[0],
        name: "TempMin",
        unit: "°C",
        lane: Lane::Slow,
        multi_frame: false,
    },
    Did {
        code: 0x2A0B,
        ecu: ECUS[0],
        name: "BatteryTemp",
        unit: "°C",
        lane: Lane::Slow,
        multi_frame: false,
    },
    // ── Online/GPS (767) ────────────────────────────────────────────────────
    Did {
        code: 0x2430,
        ecu: ECUS[3],
        name: "GPS",
        unit: "",
        lane: Lane::Slow,
        multi_frame: true,
    },
    // ── Drivetrain (17FC0076) ────────────────────────────────────────────────
    Did {
        code: 0x295A,
        ecu: ECUS[2],
        name: "Odometer",
        unit: "km",
        lane: Lane::Slow,
        multi_frame: false,
    },
    // ── Energy (710) ─────────────────────────────────────────────────────────
    Did {
        code: 0x2AB2,
        ecu: ECUS[5],
        name: "MaxEnergy",
        unit: "Wh",
        lane: Lane::Slow,
        multi_frame: false,
    },
    // ── DCDC (17FC00B9) ──────────────────────────────────────────────────────
    Did {
        code: 0x465B,
        ecu: ECUS[1],
        name: "DCDCCurrent",
        unit: "A",
        lane: Lane::Slow,
        multi_frame: false,
    },
    Did {
        code: 0x465D,
        ecu: ECUS[1],
        name: "DCDCVoltage",
        unit: "V",
        lane: Lane::Slow,
        multi_frame: false,
    },
    // ── Energy 12V multiframe ────────────────────────────────────────────────
    Did {
        code: 0x2AF7,
        ecu: ECUS[5],
        name: "12VBattery",
        unit: "",
        lane: Lane::Slow,
        multi_frame: true,
    },
];

/// Look up a DID entry by code.
pub fn did_by_code(code: u16) -> Option<&'static Did> {
    DIDS.iter().find(|d| d.code == code)
}

// ── DecodedValue ─────────────────────────────────────────────────────────────

/// A decoded sensor value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DecodedValue {
    /// Floating-point value with unit.
    Float { val: f64, unit: String },
    /// Integer value.
    Int(i64),
    /// Enum string representation.
    Enum(String),
    /// GPS position in decimal degrees (WGS84). Negative = south / west.
    GpsPosition { lat: f64, lon: f64 },
    /// Raw bytes when scaling is unknown.
    Bytes(Vec<u8>),
}

/// Decode raw CAN response bytes for a given DID into a `DecodedValue`.
///
/// Scaling rules from docs/meb-can-pids.md:
/// - SoC (0x028C): 0.4 %/step → raw * 0.4
/// - Speed (0xF40D): raw * 0.05 (common MEB scaling)
/// - Mode (0x7448): enum 0=standby 1=driving 4=AC charge 6=DC charge
/// - PackV (0x1E3B): raw * 0.1 V
/// - PackI (0x1E3D): raw * 0.1 A (signed, 2's complement)
/// - Temps (0x1E0E/0x1E0F/0x2A0B): raw * 0.1 - 40 °C
/// - Odometer (0x295A): raw * 0.1 km
/// - MaxEnergy (0x2AB2): raw * 1 Wh
/// - DCDC current (0x465B): raw * 0.1 A
/// - DCDC voltage (0x465D): raw * 0.1 V
///
/// Where scaling is uncertain, returns Bytes.
pub fn decode(did: u16, raw: &[u8]) -> DecodedValue {
    if raw.is_empty() {
        return DecodedValue::Bytes(vec![]);
    }

    // Helper: u16 from big-endian bytes (take first 2)
    let u16_be = |b: &[u8]| -> u16 {
        if b.len() < 2 {
            b[0] as u16
        } else {
            ((b[0] as u16) << 8) | (b[1] as u16)
        }
    };

    match did {
        // SoC: 0.4%/step
        0x028C => {
            let raw_val = u16_be(raw);
            DecodedValue::Float {
                val: raw_val as f64 * 0.4,
                unit: "%".to_string(),
            }
        }
        // Speed: common MEB scaling 0.05 km/h per step
        0xF40D => {
            let raw_val = u16_be(raw);
            DecodedValue::Float {
                val: raw_val as f64 * 0.05,
                unit: "km/h".to_string(),
            }
        }
        // Mode: enum
        0x7448 => {
            let raw_val = raw[0];
            let mode = match raw_val {
                0 => "standby",
                1 => "driving",
                4 => "ac_charge",
                6 => "dc_charge",
                _ => "unknown",
            };
            DecodedValue::Enum(mode.to_string())
        }
        // PackV: 0.1 V per step
        0x1E3B => {
            let raw_val = u16_be(raw);
            DecodedValue::Float {
                val: raw_val as f64 * 0.1,
                unit: "V".to_string(),
            }
        }
        // PackI: 0.1 A per step (signed)
        0x1E3D => {
            let raw_val = u16_be(raw) as i16;
            DecodedValue::Float {
                val: raw_val as f64 * 0.1,
                unit: "A".to_string(),
            }
        }
        // TempMax: 0.1 °C per step - 40
        0x1E0E => {
            let raw_val = u16_be(raw);
            DecodedValue::Float {
                val: raw_val as f64 * 0.1 - 40.0,
                unit: "°C".to_string(),
            }
        }
        // TempMin: 0.1 °C per step - 40
        0x1E0F => {
            let raw_val = u16_be(raw);
            DecodedValue::Float {
                val: raw_val as f64 * 0.1 - 40.0,
                unit: "°C".to_string(),
            }
        }
        // BatteryTemp: 0.1 °C per step - 40
        0x2A0B => {
            let raw_val = u16_be(raw);
            DecodedValue::Float {
                val: raw_val as f64 * 0.1 - 40.0,
                unit: "°C".to_string(),
            }
        }
        // Odometer: 0.1 km per step (multi-byte, read full big-endian slice)
        0x295A => {
            let raw_val = raw.iter().fold(0u64, |acc, &b| (acc << 8) | b as u64);
            DecodedValue::Float {
                val: raw_val as f64 * 0.1,
                unit: "km".to_string(),
            }
        }
        // MaxEnergy: 1 Wh per step
        0x2AB2 => {
            let raw_val = u16_be(raw);
            DecodedValue::Int(raw_val as i64)
        }
        // DCDC current: 0.1 A
        0x465B => {
            let raw_val = u16_be(raw);
            DecodedValue::Float {
                val: raw_val as f64 * 0.1,
                unit: "A".to_string(),
            }
        }
        // DCDC voltage: 0.1 V
        0x465D => {
            let raw_val = u16_be(raw);
            DecodedValue::Float {
                val: raw_val as f64 * 0.1,
                unit: "V".to_string(),
            }
        }
        // GPS and 12VBattery are multi-frame — return bytes
        0x2430 | 0x2AF7 => DecodedValue::Bytes(raw.to_vec()),
        // Unknown DIDs return raw bytes
        _ => DecodedValue::Bytes(raw.to_vec()),
    }
}

// ── Sample + Batch (uplink wire format) ──────────────────────────────────────

/// A single telemetry sample.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    /// DID code (e.g. 0x028C).
    pub did: u32,
    /// Timestamp in milliseconds (monotonic or wall-clock, as set by device).
    pub ts_ms: u64,
    /// Raw response bytes from the ECU.
    pub raw: Vec<u8>,
    /// Decoded value.
    pub value: DecodedValue,
}

/// A batch of samples from one device — the POST /ingest wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SampleBatch {
    /// Device identifier (e.g. MAC address or serial).
    pub device_id: String,
    /// Samples in this batch.
    pub samples: Vec<Sample>,
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_soc() {
        // SoC 0x028C: raw=250 → 250 * 0.4 = 100.0 %
        let raw = [0x00, 0xFA]; // 250 big-endian
        let val = decode(0x028C, &raw);
        assert_eq!(val, DecodedValue::Float { val: 100.0, unit: "%".to_string() });

        // raw=0 → 0.0 %
        let raw = [0x00, 0x00];
        let val = decode(0x028C, &raw);
        assert_eq!(val, DecodedValue::Float { val: 0.0, unit: "%".to_string() });

        // raw=1 → 0.4 %
        let raw = [0x00, 0x01];
        let val = decode(0x028C, &raw);
        assert_eq!(val, DecodedValue::Float { val: 0.4, unit: "%".to_string() });
    }

    #[test]
    fn test_decode_pack_voltage() {
        // PackV 0x1E3B: raw=40000 → 40000 * 0.1 = 4000.0 V
        let raw = [0x9C, 0x40]; // 40000 big-endian
        let val = decode(0x1E3B, &raw);
        assert_eq!(val, DecodedValue::Float { val: 4000.0, unit: "V".to_string() });
    }

    #[test]
    fn test_decode_pack_current() {
        // PackI 0x1E3D: raw=1000 → 1000 * 0.1 = 100.0 A
        let raw = [0x03, 0xE8]; // 1000 big-endian
        let val = decode(0x1E3D, &raw);
        assert_eq!(val, DecodedValue::Float { val: 100.0, unit: "A".to_string() });
    }

    #[test]
    fn test_decode_mode() {
        // Mode 0x7448: 0=standby
        let raw = [0x00];
        let val = decode(0x7448, &raw);
        assert_eq!(val, DecodedValue::Enum("standby".to_string()));

        // 1=driving
        let raw = [0x01];
        let val = decode(0x7448, &raw);
        assert_eq!(val, DecodedValue::Enum("driving".to_string()));

        // 4=ac_charge
        let raw = [0x04];
        let val = decode(0x7448, &raw);
        assert_eq!(val, DecodedValue::Enum("ac_charge".to_string()));

        // 6=dc_charge
        let raw = [0x06];
        let val = decode(0x7448, &raw);
        assert_eq!(val, DecodedValue::Enum("dc_charge".to_string()));
    }

    #[test]
    fn test_decode_temperature() {
        // TempMax 0x1E0E: raw=440 → 440 * 0.1 - 40 = 4.0 °C
        let raw = [0x01, 0xB8]; // 440 big-endian
        let val = decode(0x1E0E, &raw);
        assert_eq!(val, DecodedValue::Float { val: 4.0, unit: "°C".to_string() });
    }

    #[test]
    fn test_decode_omdeter() {
        // Odometer 0x295A: raw=100000 → 100000 * 0.1 = 10000.0 km
        let raw = [0x01, 0x86, 0xA0]; // 100000 big-endian
        let val = decode(0x295A, &raw);
        assert_eq!(val, DecodedValue::Float { val: 10000.0, unit: "km".to_string() });
    }

    #[test]
    fn test_decode_max_energy() {
        // MaxEnergy 0x2AB2: raw=60000 → 60000 Wh
        let raw = [0xEA, 0x60]; // 60000 big-endian
        let val = decode(0x2AB2, &raw);
        assert_eq!(val, DecodedValue::Int(60000));
    }

    #[test]
    fn test_decode_multiframe_returns_bytes() {
        // GPS 0x2430 is multi-frame → bytes
        let raw = [0x01, 0x02, 0x03];
        let val = decode(0x2430, &raw);
        assert_eq!(val, DecodedValue::Bytes(vec![0x01, 0x02, 0x03]));

        // 12VBattery 0x2AF7 is multi-frame → bytes
        let val = decode(0x2AF7, &raw);
        assert_eq!(val, DecodedValue::Bytes(vec![0x01, 0x02, 0x03]));
    }

    #[test]
    fn test_decode_unknown_returns_bytes() {
        let raw = [0xAA, 0xBB];
        let val = decode(0xFFFF, &raw);
        assert_eq!(val, DecodedValue::Bytes(vec![0xAA, 0xBB]));
    }

    #[test]
    fn test_sample_batch_serde_roundtrip() {
        let batch = SampleBatch {
            device_id: "test-device".to_string(),
            samples: vec![
                Sample {
                    did: 0x028C,
                    ts_ms: 1_000_000,
                    raw: vec![0x00, 0xFA],
                    value: DecodedValue::Float { val: 100.0, unit: "%".to_string() },
                },
                Sample {
                    did: 0x1E3B,
                    ts_ms: 1_000_001,
                    raw: vec![0x9C, 0x40],
                    value: DecodedValue::Float { val: 4000.0, unit: "V".to_string() },
                },
            ],
        };

        let json = serde_json::to_string(&batch).unwrap();
        let parsed: SampleBatch = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.device_id, batch.device_id);
        assert_eq!(parsed.samples.len(), batch.samples.len());
        assert_eq!(parsed.samples[0].did, batch.samples[0].did);
        assert_eq!(parsed.samples[0].ts_ms, batch.samples[0].ts_ms);
        assert_eq!(parsed.samples[0].raw, batch.samples[0].raw);
        assert_eq!(parsed.samples[0].value, batch.samples[0].value);
        assert_eq!(parsed.samples[1].did, batch.samples[1].did);
        assert_eq!(parsed.samples[1].value, batch.samples[1].value);
    }

    #[test]
    fn test_did_catalogue_contains_core_signals() {
        // Verify the catalogue has all the core DIDs
        let codes: std::collections::HashSet<u16> =
            DIDS.iter().map(|d| d.code).collect();
        let expected = [
            0x028C, 0xF40D, 0x7448, 0x1E3B, 0x1E3D, 0x1E0E, 0x1E0F,
            0x2A0B, 0x2430, 0x295A, 0x2AB2, 0x465B, 0x465D, 0x2AF7,
        ];
        for code in &expected {
            assert!(codes.contains(code), "Missing DID 0x{:04X}", code);
        }
    }

    #[test]
    fn test_multiframe_flags() {
        // GPS (0x2430) should be multi-frame
        let gps = did_by_code(0x2430).unwrap();
        assert!(gps.multi_frame);

        // 12VBattery (0x2AF7) should be multi-frame
        let bat12 = did_by_code(0x2AF7).unwrap();
        assert!(bat12.multi_frame);

        // SoC should NOT be multi-frame
        let soc = did_by_code(0x028C).unwrap();
        assert!(!soc.multi_frame);
    }

    #[test]
    fn test_ecu_headers() {
        // Verify ECU count and addressing
        assert_eq!(ECUS.len(), 6);

        // BMS is 29-bit
        let bms = ecu_by_header(0x17FC007B).unwrap();
        assert_eq!(bms.name, "BMS");
        assert_eq!(bms.bits, CanBits::Ext29);

        // Online is 11-bit
        let online = ecu_by_header(0x00000767).unwrap();
        assert_eq!(online.name, "Online");
        assert_eq!(online.bits, CanBits::Std11);

        // Unknown header returns None
        assert!(ecu_by_header(0xDEADBEEF).is_none());
    }
}
