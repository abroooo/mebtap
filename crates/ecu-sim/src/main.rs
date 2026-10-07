//! Runs the `collector` against the in-memory mock MEB ECU and prints the live
//! discovery table — the host stand-in for the car until hardware arrives.

use std::thread;
use std::time::Duration;

use collector::{Collector, LaneScheduler, MemStore, SampleStore, Uplink, UplinkError};
use ecu_sim::{pair, MebEcuSim};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let uplink_url = flag(&args, "--uplink");
    let device_key = flag(&args, "--device-key").unwrap_or_else(|| "dev".to_string());

    let (bus, link) = pair();
    MebEcuSim::new(link).spawn();

    let sched = LaneScheduler::new(Duration::from_millis(0), Duration::from_millis(0));
    let mut col = Collector::new(bus, MemStore::default(), sched, "sim-esp32");

    let passes = 8;
    println!("ecu-sim: polling mock MEB ECU for {passes} passes...\n");
    for _ in 0..passes {
        col.poll_once();
        thread::sleep(Duration::from_millis(50));
    }

    print_table(&col);

    if let Some(ref url) = uplink_url {
        match ReqwestUplink::new(url.clone(), device_key).push_all(&mut col) {
            Ok(n) => println!("\nuplink: pushed {n} samples to {url}"),
            Err(e) => println!("\nuplink: failed: {e:?}"),
        }
    } else {
        println!(
            "\n(no --uplink url given; {} samples buffered locally)",
            col.store.pending()
        );
    }
}

fn print_table(col: &Collector<ecu_sim::MemCanBus, MemStore>) {
    println!(
        "{:<6} {:<12} {:<5} {:>4} {:>4} {:>8} {:>9}  value",
        "DID", "name", "unit", "ok", "miss", "lat(ms)", "rate(Hz)"
    );
    println!("{}", "-".repeat(72));
    for r in col.discovery_table() {
        let val = match &r.last_value {
            Some(v) => format!("{v:?}"),
            None => "-".to_string(),
        };
        println!(
            "{:#06x} {:<12} {:<5} {:>4} {:>4} {:>8} {:>9.3}  {}",
            r.code,
            r.name,
            r.unit,
            r.responded,
            r.no_response,
            r.last_latency_ms.map(|v| v.to_string()).unwrap_or_else(|| "-".into()),
            r.update_rate_hz,
            val
        );
    }
    if let Some(d) = col.full_pass.last_pass {
        println!("\nfull-pass: last {:?}, longest {:?}", d, col.full_pass.longest_pass);
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

/// Minimal blocking HTTP uplink using the std library only (no reqwest in this
/// scaffold); real reqwest-based client lands in task T5.
struct ReqwestUplink {
    url: String,
    device_key: String,
}

impl ReqwestUplink {
    fn new(url: String, device_key: String) -> Self {
        Self { url, device_key }
    }

    fn push_all(&mut self, col: &mut Collector<ecu_sim::MemCanBus, MemStore>) -> Result<usize, UplinkError> {
        let mut total = 0;
        while let Some(batch) = col.take_batch(256) {
            self.push(&batch)?;
            total += batch.samples.len();
        }
        Ok(total)
    }
}

impl Uplink for ReqwestUplink {
    fn push(&mut self, _batch: &collector::SampleBatch) -> Result<(), UplinkError> {
        // Placeholder until T5 wires a real HTTP client; signals the URL/key are
        // plumbed through so the CLI contract is stable.
        let _ = (&self.url, &self.device_key);
        Err(UplinkError::Unreachable)
    }
}
