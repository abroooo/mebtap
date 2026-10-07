//! Host-side MEB ECU simulator. Provides an in-memory `CanBus` wired to a mock
//! ECU that answers UDS service `0x22` with canned responses — including a
//! flow-controlled multi-frame reply for GPS — so the real `collector` code can
//! be exercised end-to-end without any hardware.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use collector::{BusError, CanBus, CanFrame, SampleBatch, Uplink, UplinkError};

const UDS_READ_BY_ID: u8 = 0x22;

/// Blocking HTTP `Uplink` that POSTs batches to `<base_url>/ingest`.
pub struct ReqwestUplink {
    client: reqwest::blocking::Client,
    base_url: String,
    device_key: String,
}

impl ReqwestUplink {
    pub fn new(base_url: impl Into<String>, device_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::blocking::Client::new(),
            base_url: base_url.into(),
            device_key: device_key.into(),
        }
    }
}

impl Uplink for ReqwestUplink {
    fn push(&mut self, batch: &SampleBatch) -> Result<(), UplinkError> {
        let url = format!("{}/ingest", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(url)
            .header("x-device-key", &self.device_key)
            .json(batch)
            .send()
            .map_err(|_| UplinkError::Unreachable)?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(UplinkError::Rejected(format!("HTTP {}", resp.status().as_u16())))
        }
    }
}

/// The collector-facing half of an in-memory CAN link.
pub struct MemCanBus {
    to_ecu: Sender<CanFrame>,
    from_ecu: Receiver<CanFrame>,
    listen_only: bool,
}

/// The ECU-facing half of an in-memory CAN link.
pub struct EcuLink {
    to_collector: Sender<CanFrame>,
    from_collector: Receiver<CanFrame>,
}

/// Create a connected (collector, ecu) link pair.
pub fn pair() -> (MemCanBus, EcuLink) {
    let (to_ecu_tx, to_ecu_rx) = channel();
    let (to_col_tx, to_col_rx) = channel();
    (
        MemCanBus { to_ecu: to_ecu_tx, from_ecu: to_col_rx, listen_only: true },
        EcuLink { to_collector: to_col_tx, from_collector: to_ecu_rx },
    )
}

impl CanBus for MemCanBus {
    fn send(&mut self, frame: &CanFrame) -> Result<(), BusError> {
        if self.listen_only {
            return Err(BusError::ListenOnly);
        }
        self.to_ecu
            .send(frame.clone())
            .map_err(|_| BusError::Driver("ecu link closed".into()))
    }
    fn recv(&mut self, timeout: Duration) -> Option<CanFrame> {
        self.from_ecu.recv_timeout(timeout).ok()
    }
    fn set_listen_only(&mut self, listen_only: bool) {
        self.listen_only = listen_only;
    }
}

/// Mock MEB ECU. Holds the value payload (bytes after `0x62 <DID>`) for each DID
/// it answers, plus a configurable set of non-responders.
pub struct MebEcuSim {
    link: EcuLink,
    responses: HashMap<u16, Vec<u8>>,
    non_responders: HashSet<u16>,
}

impl MebEcuSim {
    pub fn new(link: EcuLink) -> Self {
        Self { link, responses: default_responses(), non_responders: HashSet::new() }
    }

    /// Mark a DID as a non-responder (ECU stays silent → collector times out).
    pub fn set_non_responder(&mut self, code: u16) {
        self.non_responders.insert(code);
    }

    /// Override / add a canned value payload for a DID.
    pub fn set_response(&mut self, code: u16, value: Vec<u8>) {
        self.responses.insert(code, value);
    }

    /// Run the ECU loop until the collector link is dropped.
    pub fn spawn(self) -> JoinHandle<()> {
        thread::spawn(move || self.run())
    }

    fn run(self) {
        loop {
            match self.link.from_collector.recv_timeout(Duration::from_millis(500)) {
                Ok(frame) => self.handle(&frame),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn handle(&self, frame: &CanFrame) {
        // Only answer UDS service 0x22 requests. Ignore stray flow-control etc.
        if frame.data.get(1) != Some(&UDS_READ_BY_ID) || frame.data.len() < 4 {
            return;
        }
        let code = ((frame.data[2] as u16) << 8) | frame.data[3] as u16;
        if self.non_responders.contains(&code) {
            return;
        }
        let Some(value) = self.responses.get(&code) else { return };

        let mut uds = vec![0x62, (code >> 8) as u8, (code & 0xFF) as u8];
        uds.extend_from_slice(value);
        self.send_isotp(frame.id, frame.extended, &uds);
    }

    fn send_isotp(&self, id: u32, extended: bool, uds: &[u8]) {
        let total = uds.len();
        if total <= 7 {
            let mut data = vec![total as u8];
            data.extend_from_slice(uds);
            data.resize(8, 0x55);
            let _ = self.link.to_collector.send(CanFrame::new(id, extended, data));
            return;
        }
        // First frame: 6 payload bytes + 12-bit length.
        let mut ff = vec![0x10 | ((total >> 8) as u8), (total & 0xFF) as u8];
        ff.extend_from_slice(&uds[0..6]);
        if self.link.to_collector.send(CanFrame::new(id, extended, ff)).is_err() {
            return;
        }
        // Wait for the collector's flow-control frame before streaming the rest.
        let _ = self.link.from_collector.recv_timeout(Duration::from_millis(500));
        let mut idx = 6usize;
        let mut seq = 1u8;
        while idx < total {
            let end = (idx + 7).min(total);
            let mut cf = vec![0x20 | (seq & 0x0F)];
            cf.extend_from_slice(&uds[idx..end]);
            cf.resize(8, 0x55);
            if self.link.to_collector.send(CanFrame::new(id, extended, cf)).is_err() {
                return;
            }
            idx = end;
            seq = seq.wrapping_add(1);
        }
    }
}

/// Canned value payloads for the catalogue DIDs. Scalars chosen to decode to
/// plausible values; GPS/12V produce >7 bytes to force multi-frame.
fn default_responses() -> HashMap<u16, Vec<u8>> {
    let mut m = HashMap::new();
    m.insert(0x028C, vec![0x00, 0xFA]); // SoC 100%
    m.insert(0xF40D, vec![0x02, 0x58]); // Speed ~30 km/h
    m.insert(0x7448, vec![0x01]); // Mode: driving
    m.insert(0x1E3B, vec![0x0F, 0xA0]); // PackV 400.0 V
    m.insert(0x1E3D, vec![0x00, 0x64]); // PackI 10.0 A
    m.insert(0x1E0E, vec![0x02, 0x1C]); // TempMax ~13.6 °C
    m.insert(0x1E0F, vec![0x02, 0x00]); // TempMin ~11.2 °C
    m.insert(0x2A0B, vec![0x02, 0x10]); // BatteryTemp ~12.8 °C
    m.insert(0x295A, vec![0x01, 0x86, 0xA0]); // Odometer 10000.0 km
    m.insert(0x2AB2, vec![0x00, 0x80]); // MaxEnergy 128 Wh
    m.insert(0x465B, vec![0x00, 0xC8]); // DCDC current 20.0 A
    m.insert(0x465D, vec![0x00, 0x8C]); // DCDC voltage 14.0 V
    m.insert(0x2430, (0..10u8).collect()); // GPS: 10 bytes → multi-frame
    m.insert(0x2AF7, (0..8u8).collect()); // 12V: 8 bytes → multi-frame
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use collector::{Collector, LaneScheduler, MemStore, SampleStore};
    use meb_proto::DecodedValue;

    fn run_passes(sim_setup: impl FnOnce(&mut MebEcuSim)) -> Collector<MemCanBus, MemStore> {
        let (bus, link) = pair();
        let mut sim = MebEcuSim::new(link);
        sim_setup(&mut sim);
        sim.spawn();
        let sched = LaneScheduler::new(Duration::from_millis(0), Duration::from_millis(0));
        let mut col = Collector::new(bus, MemStore::default(), sched, "sim-dev");
        for _ in 0..4 {
            col.poll_once();
            thread::sleep(Duration::from_millis(5));
        }
        col
    }

    #[test]
    fn end_to_end_responders_decode() {
        let col = run_passes(|_| {});
        let soc = col.stats(0x028C).expect("soc");
        assert!(soc.responded >= 1);
        assert_eq!(soc.last_value, Some(DecodedValue::Float { val: 100.0, unit: "%".into() }));
    }

    #[test]
    fn end_to_end_multiframe_gps_reassembles() {
        let col = run_passes(|_| {});
        let gps = col.stats(0x2430).expect("gps");
        assert!(gps.responded >= 1, "gps should respond");
        assert!(gps.multi_frame);
        match gps.last_value {
            Some(DecodedValue::Bytes(ref b)) => assert_eq!(b.len(), 10),
            ref other => panic!("expected 10 GPS bytes, got {other:?}"),
        }
    }

    #[test]
    fn end_to_end_non_responder_flagged() {
        let col = run_passes(|sim| sim.set_non_responder(0x1E3B));
        let packv = col.stats(0x1E3B).expect("packv polled");
        assert_eq!(packv.responded, 0);
        assert!(packv.no_response >= 1);
    }

    #[test]
    fn samples_accumulate_in_store() {
        let mut col = run_passes(|_| {});
        assert!(col.store.pending() > 0);
        let batch = col.take_batch(1000).expect("batch");
        assert_eq!(batch.device_id, "sim-dev");
        assert!(!batch.samples.is_empty());
    }
}
