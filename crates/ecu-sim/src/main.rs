//! Runs the `collector` against the in-memory mock MEB ECU and prints the live
//! discovery table — the host stand-in for the car until hardware arrives.

use std::thread;
use std::time::Duration;

use collector::{Collector, LaneScheduler, MemStore, SampleStore, Uplink};
use ecu_sim::{pair, MebEcuSim, ReqwestUplink};

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
        let mut up = ReqwestUplink::new(url.clone(), device_key);
        let mut pushed = 0usize;
        let mut err = None;
        while let Some(batch) = col.take_batch(256) {
            match up.push(&batch) {
                Ok(()) => pushed += batch.samples.len(),
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        match err {
            None => println!("\nuplink: pushed {pushed} samples to {url}"),
            Some(e) => println!("\nuplink: pushed {pushed} then failed: {e:?}"),
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
