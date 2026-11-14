//! Synthetic *drive replay* generator — a fully host-side demo data source.
//!
//! This produces a plausible, **entirely fictional** telemetry run that flows
//! through the exact same `SampleBatch` → `/ingest` path the real device uses,
//! so the backend, DB, and poster render it like any other drive — **without a
//! car, a flash, or any real GPS**. It exists purely to showcase the possiblities
//! the data gives..
//!
//! Positions come from a **GPX track**: the GPS points are taken verbatim from
//! the GPX so the track lies on real roads, and **only** the per-point speed /
//! SoC / 12 V / HV pack V·I / odometer / sats / mode DIDs are synthesized —
//! from the track's own elevation profile (grade-aware speed, gravitational
//! energy with regen on descents), so the numbers move believably with the
//! terrain. The embedded default is a small, **fictional** synthetic climb
//! shipped with the crate; `--replay-stage --gpx <path>` renders any other
//! track (the README posters were made from public cycling stage GPX tracks).

use meb_proto::{DecodedValue, Sample, SampleBatch};

// DID codes consumed by the poster (see scripts/poster.py + crates/meb-proto).
const DID_SOC: u32 = 0x028C; // BMS SoC            %      value_num
const DID_SPEED: u32 = 0xF40D; // Speed            km/h   value_num
const DID_RAIL: u32 = 0xFFE0; // 12 V rail (ADC)   V      value_num
const DID_ODO: u32 = 0x295A; // Odometer           km     value_num
const DID_MODE: u32 = 0x7448; // Car operation mode enum   value_text
const DID_GPS: u32 = 0x2430; // GPS position       lat,lon value_text
const DID_SATS: u32 = 0x2431; // GPS sats tracked  count  value_num
const DID_HVV: u32 = 0x1E3B; // HV battery voltage V      value_num
const DID_HVI: u32 = 0x1E3D; // HV battery current A      value_num

/// The embedded demo route: a small, **fictional** synthetic alpine climb that
/// ships with the crate (`assets/demo-route.gpx`). Embedding keeps the generator
/// hermetic and the demo byte-reproducible with no third-party data; override it
/// with a real track via `ecu-sim --replay-stage --gpx <path>`.
pub const DEMO_GPX: &str = include_str!("../assets/demo-route.gpx");

// --- vehicle / energy model constants (Škoda Enyaq, order-of-magnitude) ---
const USABLE_WH: f64 = 77_000.0; // usable pack → 1 % SoC == 770 Wh
const MASS_KG: f64 = 2200.0; // kerb + driver
const G: f64 = 9.81;
const DRIVE_ETA: f64 = 0.85; // drivetrain efficiency climbing
const REGEN_ETA: f64 = 0.60; // fraction of descent potential recovered
const SOC_START: f64 = 95.0; // BMS SoC % at the start line
const ODO_START: f64 = 24_000.0; // odometer km at the start
                                 // HV pack open-circuit voltage model: roughly linear in SoC over the operating
                                 // window (~300 V empty … ~405 V full) for an MEB ~352 V nominal pack. The
                                 // emitted HV current is then P/V, so a poster recovering P = V·I round-trips to
                                 // the same instantaneous power that drives the SoC integration below.
const HV_V_BASE: f64 = 300.0;
const HV_V_PER_SOC: f64 = 1.05;

/// A geographic point.
#[derive(Clone, Copy, Debug)]
struct Pt {
    lat: f64,
    lon: f64,
}

/// A route vertex: a position plus its real elevation (metres) from the GPX.
#[derive(Clone, Copy, Debug)]
struct Vtx {
    p: Pt,
    ele: f64,
}

/// Great-circle distance between two points, in km.
fn haversine_km(a: Pt, b: Pt) -> f64 {
    const R: f64 = 6371.0088;
    let (p1, p2) = (a.lat.to_radians(), b.lat.to_radians());
    let dphi = (b.lat - a.lat).to_radians();
    let dlam = (b.lon - a.lon).to_radians();
    let h = (dphi / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlam / 2.0).sin().powi(2);
    2.0 * R * h.sqrt().asin()
}

/// Deterministic tiny LCG so the demo poster is byte-reproducible (no OS RNG).
struct Lcg(u64);
impl Lcg {
    fn new(seed: u64) -> Self {
        Lcg(seed)
    }
    /// Next value in [-1.0, 1.0].
    fn noise(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as f64 / (1u64 << 31) as f64) - 1.0
    }
}

// --------------------------------------------------------------------------- #
// GPX parsing (tolerant, dependency-free)
// --------------------------------------------------------------------------- #
/// Read the first `f64` in `s` that follows `key` (e.g. `lat="`), stopping at
/// the next `"`. Returns `None` if absent/unparseable.
fn attr_f64(s: &str, key: &str) -> Option<f64> {
    let start = s.find(key)? + key.len();
    let rest = &s[start..];
    let end = rest.find('"')?;
    rest[..end].trim().parse().ok()
}

/// Read the `f64` enclosed by `open`/`close` (e.g. `<ele>`/`</ele>`).
fn tag_f64(s: &str, open: &str, close: &str) -> Option<f64> {
    let start = s.find(open)? + open.len();
    let rest = &s[start..];
    let end = rest.find(close)?;
    rest[..end].trim().parse().ok()
}

/// Parse `<trkpt lat=".." lon="..">[<ele>..</ele>]` points out of a GPX string,
/// in file order. Elevation defaults to 0 m when absent. Pure; no I/O.
fn parse_gpx(gpx: &str) -> Vec<Vtx> {
    let mut v = Vec::new();
    // split on the point tag; skip the leading header chunk before the 1st pt.
    for chunk in gpx.split("<trkpt").skip(1) {
        if let (Some(lat), Some(lon)) = (attr_f64(chunk, "lat=\""), attr_f64(chunk, "lon=\"")) {
            let ele = tag_f64(chunk, "<ele>", "</ele>").unwrap_or(0.0);
            v.push(Vtx {
                p: Pt { lat, lon },
                ele,
            });
        }
    }
    v
}

/// Cumulative along-route distance (km) to each vertex.
fn cumulative(route: &[Vtx]) -> Vec<f64> {
    let mut cum = vec![0.0f64];
    for i in 1..route.len() {
        cum.push(cum[i - 1] + haversine_km(route[i - 1].p, route[i].p));
    }
    cum
}

/// Interpolate (position, elevation) at cumulative distance `s` km along the
/// route.
fn sample_at(route: &[Vtx], cum: &[f64], s: f64) -> (Pt, f64) {
    let total = *cum.last().unwrap();
    let s = s.clamp(0.0, total);
    for i in 1..route.len() {
        if s <= cum[i] || i == route.len() - 1 {
            let leg_km = (cum[i] - cum[i - 1]).max(1e-9);
            let f = ((s - cum[i - 1]) / leg_km).clamp(0.0, 1.0);
            let (a, b) = (route[i - 1], route[i]);
            return (
                Pt {
                    lat: a.p.lat + (b.p.lat - a.p.lat) * f,
                    lon: a.p.lon + (b.p.lon - a.p.lon) * f,
                },
                a.ele + (b.ele - a.ele) * f,
            );
        }
    }
    let last = *route.last().unwrap();
    (last.p, last.ele)
}

/// Local road grade in percent at distance `s`, measured over a short forward
/// look-ahead (≈150 m) so it is stable and independent of the current speed.
fn grade_pct(route: &[Vtx], cum: &[f64], s: f64) -> f64 {
    let total = *cum.last().unwrap();
    const DS_M: f64 = 150.0;
    let (_, e0) = sample_at(route, cum, s);
    let (_, e1) = sample_at(route, cum, (s + DS_M / 1000.0).min(total));
    (e1 - e0) / DS_M * 100.0
}

// --------------------------------------------------------------------------- #
// drive synthesis
// --------------------------------------------------------------------------- #
/// Generate the demo drive from the embedded GPX. Thin wrapper over
/// [`drive_from_gpx`] kept for the call sites / tests.
pub fn demo_drive(device_id: &str, start_ms: u64, dt_s: u64) -> SampleBatch {
    drive_from_gpx(device_id, DEMO_GPX, start_ms, dt_s)
}

/// Generate the full synthetic drive as a `SampleBatch` from a GPX track, with
/// the drive *starting* at `start_ms` (Unix ms) and samples spaced every `dt_s`
/// seconds. GPS positions come verbatim from the GPX (interpolated along the
/// real road); every other DID is synthesized from the track's elevation.
///
/// Returns the batch; the caller can inspect the last sample's `ts_ms` to learn
/// the drive's end time (e.g. to offset the whole run so it ends "now").
pub fn drive_from_gpx(device_id: &str, gpx: &str, start_ms: u64, dt_s: u64) -> SampleBatch {
    let route = parse_gpx(gpx);
    assert!(
        route.len() >= 2,
        "GPX has too few track points ({})",
        route.len()
    );
    let cum = cumulative(&route);
    let total_km = *cum.last().unwrap();

    let dt_h = dt_s as f64 / 3600.0;
    let dt_ms = dt_s * 1000;
    let mut rng = Lcg::new(0x5EEDA19ED47);

    let mut soc = SOC_START; // BMS SoC %
    let mut odo = ODO_START; // odometer km
    let mut s = 0.0f64; // distance travelled along the route (km)
    let mut t = start_ms;

    let mut samples: Vec<Sample> = Vec::new();
    let push = |samples: &mut Vec<Sample>, did: u32, ts: u64, value: DecodedValue| {
        samples.push(Sample {
            did,
            ts_ms: ts,
            raw: Vec::new(),
            value,
        });
    };

    // Hard cap on steps: a defensive guard so a modelling bug can never spin
    // into an unbounded allocation (the route is only hundreds of km).
    let max_steps = 1 + (total_km / 0.002) as usize * 2;
    for _ in 0..max_steps {
        let (pos, ele) = sample_at(&route, &cum, s);
        let g = grade_pct(&route, &cum, s);

        // Speed from grade: steep climbs slow to a hairpin crawl, flats run
        // fast, descents reined in. A *car* (not a cyclist) still takes steep
        // 8–13 % alpine ramps at ~30–45 km/h, so the penalty is gentle and
        // floored at 25 km/h while driving (the finish-line ease below is the
        // only thing that takes it below that).
        let mut v = if g >= 0.0 {
            85.0 - g * 4.5
        } else {
            85.0 + g * 2.0
        };
        v += 3.0 * rng.noise();
        v = v.clamp(25.0, 100.0);
        let remaining = total_km - s;
        if remaining < 0.6 {
            v *= (remaining / 0.6).max(0.0); // ease to a stop at the finish line
        }

        let moving = v > 1.0;
        let ds = v * dt_h; // km covered this step
        let dh_m = g / 100.0 * ds * 1000.0; // elevation change over the step (m)

        // Energy: rolling+aero baseline plus gravitational work (recovered at
        // REGEN_ETA on descents, paid at 1/DRIVE_ETA on climbs). e_wh may be
        // negative downhill → SoC ticks back up (real regen, not faked).
        let base_wh = (130.0 + 0.006 * v * v) * ds;
        let grav_wh = if dh_m >= 0.0 {
            MASS_KG * G * dh_m / 3600.0 / DRIVE_ETA
        } else {
            MASS_KG * G * dh_m / 3600.0 * REGEN_ETA
        };
        soc -= (base_wh + grav_wh) / USABLE_WH * 100.0;
        soc = soc.clamp(3.0, 100.0);
        odo += ds;

        // HV pack electrical state for this step. Instantaneous pack power is
        // the step energy over the step time (draw positive, regen negative on
        // descents). Current = power / terminal voltage; parked steps cover ~no
        // ground so power ≈ 0 and current sits near zero. These are the exact
        // numbers the drive model already uses — surfaced as the real HV DIDs so
        // the poster can colour the route by draw vs recuperation.
        let hv_v = (HV_V_BASE + HV_V_PER_SOC * soc + 1.5 * rng.noise()).max(240.0);
        let power_w = (base_wh + grav_wh) / dt_h; // W, draw + / regen -
        let hv_i = power_w / hv_v; // A, draw + / regen -

        let rail = if moving {
            14.2 + 0.15 * rng.noise()
        } else {
            12.7 + 0.1 * rng.noise()
        };
        // Fewer satellites deep in the shadow of the high surrounding peaks.
        let sats = if ele > 2000.0 {
            6.0 + 1.5 * rng.noise().abs()
        } else {
            9.0 + 2.0 * rng.noise().abs()
        };

        push(
            &mut samples,
            DID_GPS,
            t,
            DecodedValue::GpsPosition {
                lat: pos.lat,
                lon: pos.lon,
            },
        );
        push(
            &mut samples,
            DID_SPEED,
            t,
            DecodedValue::Float {
                val: round1(v),
                unit: "km/h".into(),
            },
        );
        push(
            &mut samples,
            DID_SOC,
            t,
            DecodedValue::Float {
                val: round1(soc),
                unit: "%".into(),
            },
        );
        push(
            &mut samples,
            DID_RAIL,
            t,
            DecodedValue::Float {
                val: round2(rail),
                unit: "V".into(),
            },
        );
        push(
            &mut samples,
            DID_ODO,
            t,
            DecodedValue::Float {
                val: round1(odo),
                unit: "km".into(),
            },
        );
        push(
            &mut samples,
            DID_SATS,
            t,
            DecodedValue::Float {
                val: sats.round(),
                unit: "count".into(),
            },
        );
        push(
            &mut samples,
            DID_HVV,
            t,
            DecodedValue::Float {
                val: round1(hv_v),
                unit: "V".into(),
            },
        );
        push(
            &mut samples,
            DID_HVI,
            t,
            DecodedValue::Float {
                val: round1(hv_i),
                unit: "A".into(),
            },
        );
        push(
            &mut samples,
            DID_MODE,
            t,
            DecodedValue::Enum(if moving {
                "driving".into()
            } else {
                "standby".into()
            }),
        );

        if s >= total_km {
            break;
        }
        // Floor the advance (≥2 m/step) so easing the speed to 0 at the finish
        // can never stall forward progress and hang the generator.
        s += ds.max(0.002);
        t += dt_ms;
    }

    SampleBatch {
        device_id: device_id.to_string(),
        samples,
    }
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gps_points(b: &SampleBatch) -> Vec<(f64, f64)> {
        b.samples
            .iter()
            .filter_map(|s| match (s.did, &s.value) {
                (DID_GPS, DecodedValue::GpsPosition { lat, lon }) => Some((*lat, *lon)),
                _ => None,
            })
            .collect()
    }

    fn soc_series(b: &SampleBatch) -> Vec<f64> {
        b.samples
            .iter()
            .filter_map(|s| match (s.did, &s.value) {
                (DID_SOC, DecodedValue::Float { val, .. }) => Some(*val),
                _ => None,
            })
            .collect()
    }

    fn did_series(b: &SampleBatch, did: u32) -> Vec<f64> {
        b.samples
            .iter()
            .filter_map(|s| match (s.did, &s.value) {
                (d, DecodedValue::Float { val, .. }) if d == did => Some(*val),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn emits_hv_power_that_draws_net_and_recuperates_on_descents() {
        let b = demo_drive("demo", 0, 8);
        let v = did_series(&b, DID_HVV);
        let i = did_series(&b, DID_HVI);
        assert_eq!(v.len(), i.len(), "one HV V per HV I sample");
        assert!(!v.is_empty(), "HV DIDs must be emitted");
        // plausible pack voltage window
        assert!(
            v.iter().all(|&x| (240.0..=420.0).contains(&x)),
            "HV V in range"
        );
        // net current is a draw (positive) over a drive that nets SoC down
        let net: f64 = i.iter().sum();
        assert!(net > 0.0, "net HV current should be a draw, got {net}");
        // a mountain stage recovers on descents → some negative (regen) current
        assert!(
            i.iter().any(|&x| x < -1.0),
            "expected real recuperation (negative HV current on some descent)"
        );
    }

    #[test]
    fn embedded_gpx_parses_to_the_full_track() {
        let route = parse_gpx(DEMO_GPX);
        assert!(
            route.len() > 100,
            "expected the full embedded track, got {}",
            route.len()
        );
        // elevation actually populated (not all zero)
        assert!(route.iter().any(|v| v.ele > 1000.0));
    }

    #[test]
    fn produces_a_moving_route_with_many_points() {
        let b = demo_drive("demo", 1_000_000, 8);
        let gps = gps_points(&b);
        assert!(
            gps.len() > 50,
            "route should have plenty of points, got {}",
            gps.len()
        );
        assert!(gps.first() != gps.last());
    }

    #[test]
    fn route_follows_the_gpx_endpoints() {
        let b = demo_drive("demo", 0, 8);
        let gps = gps_points(&b);
        let route = parse_gpx(DEMO_GPX);
        let (first, last) = (*gps.first().unwrap(), *gps.last().unwrap());
        let start = route.first().unwrap().p;
        let finish = route.last().unwrap().p;
        assert!((first.0 - start.lat).abs() < 0.002 && (first.1 - start.lon).abs() < 0.002);
        // ends at the GPX's final track point
        assert!(
            (last.0 - finish.lat).abs() < 0.003,
            "ends at finish lat: {}",
            last.0
        );
        assert!(
            (last.1 - finish.lon).abs() < 0.003,
            "ends at finish lon: {}",
            last.1
        );
    }

    #[test]
    fn embedded_route_distance_is_plausible() {
        let route = parse_gpx(DEMO_GPX);
        let total = *cumulative(&route).last().unwrap();
        assert!(
            (20.0..80.0).contains(&total),
            "embedded demo distance {total} km out of range"
        );
    }

    #[test]
    fn soc_declines_overall_but_regenerates_on_descents() {
        let b = demo_drive("demo", 0, 8);
        let soc = soc_series(&b);
        assert!(
            soc.first().unwrap() > soc.last().unwrap(),
            "soc should net-drop over the drive"
        );
        assert!(
            *soc.iter().min_by(|a, c| a.total_cmp(c)).unwrap() > 3.0,
            "soc must not crater"
        );
        // a mountain stage recovers energy on descents → at least one step rises
        assert!(
            soc.windows(2).any(|w| w[1] > w[0] + 1e-9),
            "expected real regen (SoC increasing on some descent step)"
        );
    }

    #[test]
    fn timestamps_are_monotonic_per_did() {
        let b = demo_drive("demo", 500, 8);
        let mut last_gps = 0u64;
        for s in b.samples.iter().filter(|s| s.did == DID_GPS) {
            assert!(s.ts_ms >= last_gps);
            last_gps = s.ts_ms;
        }
    }

    #[test]
    fn deterministic_for_a_given_start() {
        let a = demo_drive("demo", 123, 8);
        let b = demo_drive("demo", 123, 8);
        assert_eq!(a.samples.len(), b.samples.len());
    }
}
