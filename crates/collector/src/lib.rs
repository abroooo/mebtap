//! Collector — transport traits, UDS/ISO-TP client, lane scheduler, discovery
//! stats, and the polling engine. Pure logic (std only) so it compiles for both
//! the host (tests + simulator) and the esp32 target.
//!
//! Safety: the only UDS service exposed is `0x22` (ReadDataByIdentifier). There
//! is deliberately no API to emit session-control, routine, or write services.
//! The bus starts listen-only and is only switched to normal transmit while a
//! poll is actively in flight.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use meb_proto::{decode, Did, Lane, DIDS};
pub use meb_proto::{self, DecodedValue, Sample, SampleBatch};

// ── Transport traits ─────────────────────────────────────────────────────────

/// A raw CAN frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanFrame {
    /// Arbitration id (11-bit value, or 29-bit when `extended`).
    pub id: u32,
    /// True for 29-bit extended identifiers.
    pub extended: bool,
    /// Payload bytes (0..=8).
    pub data: Vec<u8>,
}

impl CanFrame {
    pub fn new(id: u32, extended: bool, data: Vec<u8>) -> Self {
        Self { id, extended, data }
    }
}

/// Error raised by a transport send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BusError {
    /// Attempted to transmit while the bus is in listen-only mode.
    ListenOnly,
    /// Underlying driver failure.
    Driver(String),
}

/// A CAN transport. Implemented in-memory by `ecu-sim` and by TWAI on firmware.
pub trait CanBus {
    /// Transmit a frame. Must fail with `BusError::ListenOnly` if listen-only.
    fn send(&mut self, frame: &CanFrame) -> Result<(), BusError>;
    /// Receive one frame, waiting up to `timeout`. `None` on timeout.
    fn recv(&mut self, timeout: Duration) -> Option<CanFrame>;
    /// Switch between listen-only (read, no ACK/TX) and normal transmit.
    fn set_listen_only(&mut self, listen_only: bool);
}

/// Persistent-ish buffer of samples pending uplink.
pub trait SampleStore {
    fn append(&mut self, samples: &[Sample]);
    /// Remove and return up to `max` oldest samples for a batch.
    fn drain_batch(&mut self, max: usize) -> Vec<Sample>;
    /// Number of samples currently buffered.
    fn pending(&self) -> usize;
}

/// Error raised while pushing a batch to the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UplinkError {
    /// No connectivity right now; caller should keep the samples buffered.
    Unreachable,
    /// Server rejected the batch.
    Rejected(String),
}

/// Forwards batches of samples to the backend when connectivity allows.
pub trait Uplink {
    fn push(&mut self, batch: &SampleBatch) -> Result<(), UplinkError>;
}

/// In-memory `SampleStore` (used on host and as a simple firmware fallback).
#[derive(Debug, Default)]
pub struct MemStore {
    buf: std::collections::VecDeque<Sample>,
}

impl SampleStore for MemStore {
    fn append(&mut self, samples: &[Sample]) {
        self.buf.extend(samples.iter().cloned());
    }
    fn drain_batch(&mut self, max: usize) -> Vec<Sample> {
        let n = max.min(self.buf.len());
        self.buf.drain(0..n).collect()
    }
    fn pending(&self) -> usize {
        self.buf.len()
    }
}

// ── UDS / ISO-TP ─────────────────────────────────────────────────────────────

const UDS_READ_BY_ID: u8 = 0x22;
const UDS_POSITIVE: u8 = 0x62; // 0x22 + 0x40
const UDS_NEGATIVE: u8 = 0x7F;

/// Error from a single UDS read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UdsError {
    /// ECU did not answer within the timeout.
    NoResponse,
    /// ECU returned a negative response (0x7F) with the given NRC.
    Negative(u8),
    /// Response was malformed / did not match the requested DID.
    Malformed(String),
    /// Transport layer failure.
    Bus(BusError),
}

/// A UDS ReadDataByIdentifier (service 0x22) client over a `CanBus`, with
/// ISO-TP single- and multi-frame (flow-controlled) reassembly.
pub struct Uds22Client<B: CanBus> {
    bus: B,
    /// How long to wait for each frame of a response.
    pub frame_timeout: Duration,
}

impl<B: CanBus> Uds22Client<B> {
    pub fn new(bus: B) -> Self {
        Self { bus, frame_timeout: Duration::from_millis(200) }
    }

    pub fn bus_mut(&mut self) -> &mut B {
        &mut self.bus
    }

    /// Read one DID. Returns the value payload (bytes after `0x62 <DID hi> <DID lo>`).
    pub fn read(&mut self, did: &Did) -> Result<Vec<u8>, UdsError> {
        let ext = matches!(did.ecu.bits, meb_proto::CanBits::Ext29);
        let req = CanFrame::new(
            did.ecu.value,
            ext,
            vec![
                0x03, // single frame, 3 payload bytes
                UDS_READ_BY_ID,
                (did.code >> 8) as u8,
                (did.code & 0xFF) as u8,
                0x55,
                0x55,
                0x55,
                0x55,
            ],
        );

        // Only transmit while actively polling.
        self.bus.set_listen_only(false);
        let send_res = self.bus.send(&req).map_err(UdsError::Bus);
        let result = send_res.and_then(|()| self.recv_response(did));
        self.bus.set_listen_only(true);
        result
    }

    fn recv_response(&mut self, did: &Did) -> Result<Vec<u8>, UdsError> {
        let first = self.bus.recv(self.frame_timeout).ok_or(UdsError::NoResponse)?;
        if first.data.is_empty() {
            return Err(UdsError::Malformed("empty frame".into()));
        }
        let pci_type = first.data[0] >> 4;
        let uds = match pci_type {
            0x0 => {
                // Single frame: low nibble = length.
                let len = (first.data[0] & 0x0F) as usize;
                if 1 + len > first.data.len() {
                    return Err(UdsError::Malformed("SF length exceeds frame".into()));
                }
                first.data[1..1 + len].to_vec()
            }
            0x1 => {
                // First frame: 12-bit total length, 6 payload bytes follow.
                if first.data.len() < 8 {
                    return Err(UdsError::Malformed("short first frame".into()));
                }
                let total = (((first.data[0] & 0x0F) as usize) << 8) | first.data[1] as usize;
                let mut buf = first.data[2..8].to_vec();
                // Send flow control: clear-to-send, no block size / separation.
                let ext = matches!(did.ecu.bits, meb_proto::CanBits::Ext29);
                let fc = CanFrame::new(did.ecu.value, ext, vec![0x30, 0x00, 0x00, 0, 0, 0, 0, 0]);
                self.bus.send(&fc).map_err(UdsError::Bus)?;
                let mut expected_seq = 1u8;
                while buf.len() < total {
                    let cf = self.bus.recv(self.frame_timeout).ok_or(UdsError::NoResponse)?;
                    if cf.data.is_empty() || (cf.data[0] >> 4) != 0x2 {
                        return Err(UdsError::Malformed("expected consecutive frame".into()));
                    }
                    let seq = cf.data[0] & 0x0F;
                    if seq != (expected_seq & 0x0F) {
                        return Err(UdsError::Malformed("consecutive frame out of order".into()));
                    }
                    expected_seq = expected_seq.wrapping_add(1);
                    buf.extend_from_slice(&cf.data[1..]);
                }
                buf.truncate(total);
                buf
            }
            other => {
                return Err(UdsError::Malformed(format!("unexpected PCI type {other:#x}")));
            }
        };

        if uds.first() == Some(&UDS_NEGATIVE) {
            let nrc = uds.get(2).copied().unwrap_or(0);
            return Err(UdsError::Negative(nrc));
        }
        if uds.len() < 3 || uds[0] != UDS_POSITIVE {
            return Err(UdsError::Malformed("not a positive 0x62 response".into()));
        }
        let got_did = ((uds[1] as u16) << 8) | uds[2] as u16;
        if got_did != did.code {
            return Err(UdsError::Malformed(format!(
                "DID mismatch: want {:#06x} got {:#06x}",
                did.code, got_did
            )));
        }
        Ok(uds[3..].to_vec())
    }
}

// ── Operation mode (lane switcher) ───────────────────────────────────────────

/// MEB operation mode from DID 0x7448.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Standby,
    Driving,
    AcCharge,
    DcCharge,
    Unknown,
}

impl Mode {
    pub fn from_raw(b: u8) -> Mode {
        match b {
            0 => Mode::Standby,
            1 => Mode::Driving,
            4 => Mode::AcCharge,
            6 => Mode::DcCharge,
            _ => Mode::Unknown,
        }
    }
    /// Fast lane only matters when the car is actually doing something.
    fn fast_lane_active(self) -> bool {
        !matches!(self, Mode::Standby)
    }
}

// ── Lane scheduler ───────────────────────────────────────────────────────────

/// Decides which DIDs are due to poll, per lane cadence and operation mode.
pub struct LaneScheduler {
    pub fast_interval: Duration,
    pub slow_interval: Duration,
    last_fast: Option<Instant>,
    last_slow: Option<Instant>,
    mode: Mode,
}

impl LaneScheduler {
    pub fn new(fast_interval: Duration, slow_interval: Duration) -> Self {
        Self {
            fast_interval,
            slow_interval,
            last_fast: None,
            last_slow: None,
            mode: Mode::Unknown,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    fn due_lane(last: &mut Option<Instant>, interval: Duration, now: Instant) -> bool {
        match *last {
            Some(t) if now.duration_since(t) < interval => false,
            _ => {
                *last = Some(now);
                true
            }
        }
    }

    /// DIDs due to be polled at `now`. Fast lane is skipped while in standby;
    /// the mode DID (0x7448) is always polled with the fast lane so we notice
    /// a transition out of standby.
    pub fn due(&mut self, now: Instant) -> Vec<&'static Did> {
        let mut out = Vec::new();
        let fast_due = Self::due_lane(&mut self.last_fast, self.fast_interval, now);
        let slow_due = Self::due_lane(&mut self.last_slow, self.slow_interval, now);
        for d in DIDS {
            let include = match d.lane {
                Lane::Fast => {
                    fast_due && (self.mode.fast_lane_active() || d.code == 0x7448)
                }
                Lane::Slow => slow_due,
            };
            if include {
                out.push(d);
            }
        }
        out
    }
}

// ── Discovery stats ──────────────────────────────────────────────────────────

/// Per-DID discovery statistics — the preserved MVP output.
#[derive(Debug, Clone)]
pub struct DidStats {
    pub code: u16,
    pub name: &'static str,
    pub unit: &'static str,
    pub multi_frame: bool,
    pub responded: u32,
    pub no_response: u32,
    pub last_latency: Option<Duration>,
    pub last_value: Option<DecodedValue>,
    pub change_count: u32,
    first_seen: Option<Instant>,
    last_seen: Option<Instant>,
}

impl DidStats {
    fn new(d: &Did) -> Self {
        Self {
            code: d.code,
            name: d.name,
            unit: d.unit,
            multi_frame: d.multi_frame,
            responded: 0,
            no_response: 0,
            last_latency: None,
            last_value: None,
            change_count: 0,
            first_seen: None,
            last_seen: None,
        }
    }

    /// Effective update rate in Hz, derived from observed value changes over the
    /// window between first and last response.
    pub fn update_rate_hz(&self) -> f64 {
        match (self.first_seen, self.last_seen) {
            (Some(a), Some(b)) => {
                let secs = b.duration_since(a).as_secs_f64();
                if secs > 0.0 {
                    self.change_count as f64 / secs
                } else {
                    0.0
                }
            }
            _ => 0.0,
        }
    }
}

/// One row of the live discovery table.
#[derive(Debug, Clone)]
pub struct DiscoveryRow {
    pub code: u16,
    pub name: &'static str,
    pub unit: &'static str,
    pub multi_frame: bool,
    pub responded: u32,
    pub no_response: u32,
    pub last_latency_ms: Option<u128>,
    pub update_rate_hz: f64,
    pub last_value: Option<DecodedValue>,
}

/// Measures wall-clock time for one complete poll pass.
#[derive(Debug, Default, Clone)]
pub struct FullPassTimer {
    pub last_pass: Option<Duration>,
    pub longest_pass: Option<Duration>,
}

// ── Collector ────────────────────────────────────────────────────────────────

/// Ties the UDS client, lane scheduler, store, and stats together.
pub struct Collector<B: CanBus, S: SampleStore> {
    pub uds: Uds22Client<B>,
    pub store: S,
    pub scheduler: LaneScheduler,
    pub device_id: String,
    stats: HashMap<u16, DidStats>,
    pub full_pass: FullPassTimer,
}

impl<B: CanBus, S: SampleStore> Collector<B, S> {
    pub fn new(bus: B, store: S, scheduler: LaneScheduler, device_id: impl Into<String>) -> Self {
        // Default to listen-only until a poll actively transmits.
        let mut uds = Uds22Client::new(bus);
        uds.bus_mut().set_listen_only(true);
        Self {
            uds,
            store,
            scheduler,
            device_id: device_id.into(),
            stats: HashMap::new(),
            full_pass: FullPassTimer::default(),
        }
    }

    /// Run one scheduling pass: poll all DIDs due now, decode, store, update
    /// stats, and measure full-pass time.
    pub fn poll_once(&mut self) -> usize {
        let now = Instant::now();
        let due = self.scheduler.due(now);
        if due.is_empty() {
            return 0;
        }
        let pass_start = Instant::now();
        let mut samples = Vec::new();
        for d in due {
            let t0 = Instant::now();
            let res = self.uds.read(d);
            let latency = t0.elapsed();
            let entry = self.stats.entry(d.code).or_insert_with(|| DidStats::new(d));
            match res {
                Ok(value_bytes) => {
                    let value = decode(d.code, &value_bytes);
                    entry.responded += 1;
                    entry.last_latency = Some(latency);
                    let now2 = Instant::now();
                    if entry.first_seen.is_none() {
                        entry.first_seen = Some(now2);
                    }
                    entry.last_seen = Some(now2);
                    if entry.last_value.as_ref() != Some(&value) {
                        entry.change_count += 1;
                    }
                    entry.last_value = Some(value.clone());

                    if d.code == 0x7448 {
                        if let DecodedValue::Enum(ref s) = value {
                            self.scheduler.set_mode(mode_from_label(s));
                        }
                    }

                    samples.push(Sample {
                        did: d.code as u32,
                        ts_ms: now_ms(),
                        raw: value_bytes,
                        value,
                    });
                }
                Err(UdsError::NoResponse) => entry.no_response += 1,
                Err(_) => entry.no_response += 1,
            }
        }
        let count = samples.len();
        if !samples.is_empty() {
            self.store.append(&samples);
        }
        let dur = pass_start.elapsed();
        self.full_pass.last_pass = Some(dur);
        self.full_pass.longest_pass = Some(match self.full_pass.longest_pass {
            Some(m) if m >= dur => m,
            _ => dur,
        });
        count
    }

    /// Drain buffered samples into a batch for uplink.
    pub fn take_batch(&mut self, max: usize) -> Option<SampleBatch> {
        let samples = self.store.drain_batch(max);
        if samples.is_empty() {
            None
        } else {
            Some(SampleBatch { device_id: self.device_id.clone(), samples })
        }
    }

    pub fn stats(&self, code: u16) -> Option<&DidStats> {
        self.stats.get(&code)
    }

    /// Snapshot of the live discovery table.
    pub fn discovery_table(&self) -> Vec<DiscoveryRow> {
        let mut rows: Vec<DiscoveryRow> = self
            .stats
            .values()
            .map(|s| DiscoveryRow {
                code: s.code,
                name: s.name,
                unit: s.unit,
                multi_frame: s.multi_frame,
                responded: s.responded,
                no_response: s.no_response,
                last_latency_ms: s.last_latency.map(|d| d.as_millis()),
                update_rate_hz: s.update_rate_hz(),
                last_value: s.last_value.clone(),
            })
            .collect();
        rows.sort_by_key(|r| r.code);
        rows
    }
}

fn mode_from_label(s: &str) -> Mode {
    match s {
        "standby" => Mode::Standby,
        "driving" => Mode::Driving,
        "ac_charge" => Mode::AcCharge,
        "dc_charge" => Mode::DcCharge,
        _ => Mode::Unknown,
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use meb_proto::did_by_code;
    use std::collections::VecDeque;

    /// Mock bus: canned responses keyed by requested DID. A DID absent from the
    /// map is a non-responder (recv yields nothing).
    struct MockBus {
        responses: HashMap<u16, Vec<CanFrame>>,
        inbox: VecDeque<CanFrame>,
        listen_only: bool,
        id: u32,
    }

    impl MockBus {
        fn new() -> Self {
            Self {
                responses: HashMap::new(),
                inbox: VecDeque::new(),
                listen_only: true,
                id: 0x7E8,
            }
        }
    }

    impl CanBus for MockBus {
        fn send(&mut self, frame: &CanFrame) -> Result<(), BusError> {
            if self.listen_only {
                return Err(BusError::ListenOnly);
            }
            // Flow-control frame: deliver any queued consecutive frames (already
            // queued at request time), so nothing to do here.
            if frame.data.first().map(|b| b >> 4) == Some(0x3) {
                return Ok(());
            }
            // Service-22 request: enqueue the canned response for its DID.
            if frame.data.get(1) == Some(&UDS_READ_BY_ID) {
                let code = ((frame.data[2] as u16) << 8) | frame.data[3] as u16;
                if let Some(frames) = self.responses.get(&code) {
                    for f in frames {
                        self.inbox.push_back(f.clone());
                    }
                }
            }
            Ok(())
        }
        fn recv(&mut self, _timeout: Duration) -> Option<CanFrame> {
            self.inbox.pop_front()
        }
        fn set_listen_only(&mut self, listen_only: bool) {
            self.listen_only = listen_only;
        }
    }

    fn sf(id: u32, uds: &[u8]) -> CanFrame {
        let mut data = vec![uds.len() as u8];
        data.extend_from_slice(uds);
        data.resize(8, 0x55);
        CanFrame::new(id, false, data)
    }

    #[test]
    fn single_frame_read_decodes_soc() {
        let mut bus = MockBus::new();
        // 0x62 02 8C <value=0xFA> → SoC 250*0.4 = 100%
        bus.responses
            .insert(0x028C, vec![sf(bus.id, &[0x62, 0x02, 0x8C, 0x00, 0xFA])]);
        let mut client = Uds22Client::new(bus);
        let did = did_by_code(0x028C).unwrap();
        let payload = client.read(did).unwrap();
        assert_eq!(payload, vec![0x00, 0xFA]);
        assert_eq!(decode(0x028C, &payload), DecodedValue::Float { val: 100.0, unit: "%".into() });
    }

    #[test]
    fn non_responder_yields_no_response() {
        let bus = MockBus::new();
        let mut client = Uds22Client::new(bus);
        let did = did_by_code(0x028C).unwrap();
        assert_eq!(client.read(did), Err(UdsError::NoResponse));
    }

    #[test]
    fn multi_frame_reassembles() {
        let mut bus = MockBus::new();
        let id = bus.id;
        // GPS 0x2430: total UDS payload = 0x62 24 30 + 10 value bytes = 13 bytes.
        let value: Vec<u8> = (0..10u8).collect();
        let mut uds = vec![0x62, 0x24, 0x30];
        uds.extend_from_slice(&value);
        let total = uds.len(); // 13
        // First frame: PCI 0x1, length 13, 6 payload bytes.
        let mut ff = vec![0x10 | ((total >> 8) as u8), (total & 0xFF) as u8];
        ff.extend_from_slice(&uds[0..6]);
        // Consecutive frames carry the rest, 7 bytes each.
        let mut cf1 = vec![0x21];
        cf1.extend_from_slice(&uds[6..13]);
        cf1.resize(8, 0x55);
        bus.responses
            .insert(0x2430, vec![CanFrame::new(id, false, ff), CanFrame::new(id, false, cf1)]);
        let mut client = Uds22Client::new(bus);
        let did = did_by_code(0x2430).unwrap();
        let payload = client.read(did).unwrap();
        assert_eq!(payload, value);
    }

    #[test]
    fn listen_only_blocks_transmit() {
        let mut bus = MockBus::new();
        bus.set_listen_only(true);
        // read() flips listen-only off before sending, so a successful send path
        // works; assert the raw guard independently.
        assert_eq!(bus.send(&CanFrame::new(0x7E0, false, vec![0x03])), Err(BusError::ListenOnly));
    }

    fn mode_sf(id: u32, raw: u8) -> CanFrame {
        sf(id, &[0x62, 0x74, 0x48, raw])
    }

    #[test]
    fn collector_polls_stores_and_tracks_stats() {
        let mut bus = MockBus::new();
        let id = bus.id;
        // Driving mode so the fast lane is active.
        bus.responses.insert(0x7448, vec![mode_sf(id, 1)]);
        bus.responses
            .insert(0x028C, vec![sf(id, &[0x62, 0x02, 0x8C, 0x00, 0xFA])]);
        // PackV 0x1E3B present, PackI 0x1E3D a non-responder.
        bus.responses
            .insert(0x1E3B, vec![sf(id, &[0x62, 0x1E, 0x3B, 0x0F, 0xA0])]);

        let sched = LaneScheduler::new(Duration::from_millis(0), Duration::from_secs(3600));
        let mut col = Collector::new(bus, MemStore::default(), sched, "test-dev");
        // First pass sets mode via 0x7448; subsequent fast polls now active.
        col.poll_once();
        col.poll_once();

        assert!(col.store.pending() > 0);
        let soc = col.stats(0x028C).expect("soc stats");
        assert!(soc.responded >= 1);
        assert!(matches!(soc.last_value, Some(DecodedValue::Float { .. })));
        let packi = col.stats(0x1E3D);
        assert!(packi.map(|s| s.no_response >= 1).unwrap_or(true));

        assert!(col.full_pass.last_pass.is_some());
        let batch = col.take_batch(100).expect("batch");
        assert_eq!(batch.device_id, "test-dev");
        assert!(!batch.samples.is_empty());
    }
}
