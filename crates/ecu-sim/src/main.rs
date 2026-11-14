//! Runs the `collector` against the in-memory mock MEB ECU and prints the live
//! discovery table — the host stand-in for the car until hardware arrives.

use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use collector::{Collector, LaneScheduler, MemStore, SampleStore, Uplink};
use ecu_sim::{pair, replay, MebEcuSim, ReqwestUplink};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let uplink_url = flag(&args, "--uplink");
    let device_key = flag(&args, "--device-key").unwrap_or_else(|| "dev".to_string());

    // `--replay-stage`: synthesize a fictional demo drive and uplink it (no CAN
    // mock, no hardware) so the backend/DB/poster have shareable demo data whose
    // route leaks no real location. See src/replay.rs.
    if args.iter().any(|a| a == "--replay-stage") {
        replay_stage(uplink_url.as_deref(), &device_key, &args);
        return;
    }

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
            r.last_latency_ms
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            r.update_rate_hz,
            val
        );
    }
    if let Some(d) = col.full_pass.last_pass {
        println!(
            "\nfull-pass: last {:?}, longest {:?}",
            d, col.full_pass.longest_pass
        );
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

/// Synthesize the demo drive and uplink it to the backend.
///
/// The drive is generated so that it *ends ~now* (samples back-dated across the
/// drive duration), so a fresh poster window of the last few hours captures it
/// without colliding with any real historical data.
fn replay_stage(uplink_url: Option<&str>, device_key: &str, args: &[String]) {
    let device_id = flag(args, "--device-id").unwrap_or_else(|| "demo-drive".to_string());
    let dt_s: u64 = flag(args, "--step-s")
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);

    // GPS positions come from a GPX track. Default = the embedded synthetic demo
    // route (crates/ecu-sim/assets/demo-route.gpx); `--gpx <path>` reads another.
    let gpx: std::borrow::Cow<str> = match flag(args, "--gpx") {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(s) => {
                println!("replay: GPS route from GPX file {path}");
                std::borrow::Cow::Owned(s)
            }
            Err(e) => {
                eprintln!(
                    "replay: could not read --gpx {path}: {e}; falling back to embedded route"
                );
                std::borrow::Cow::Borrowed(replay::DEMO_GPX)
            }
        },
        None => std::borrow::Cow::Borrowed(replay::DEMO_GPX),
    };

    // Generate once from t=0 to learn the drive duration, then shift so it ends
    // now. (Cheap: regenerating with the real start keeps ts_ms honest.)
    let probe = replay::drive_from_gpx(&device_id, &gpx, 0, dt_s);
    let duration_ms = probe.samples.last().map(|s| s.ts_ms).unwrap_or(0);
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let start_ms = now_ms.saturating_sub(duration_ms);
    let batch = replay::drive_from_gpx(&device_id, &gpx, start_ms, dt_s);

    let n = batch.samples.len();
    let gps = batch
        .samples
        .iter()
        .filter(|s| matches!(s.value, collector::DecodedValue::GpsPosition { .. }))
        .count();
    println!("replay: SYNTHETIC demo drive (fictional — no real location)");
    println!("replay: device_id={device_id}  {n} samples ({gps} GPS points)  step={dt_s}s");
    let secs = |ms: u64| (ms / 1000) as i64;
    println!(
        "replay: drive window (UTC epoch s): {} .. {}  (~{} min)",
        secs(start_ms),
        secs(now_ms),
        (now_ms - start_ms) / 60_000
    );

    match uplink_url {
        Some(url) => {
            let mut up = ReqwestUplink::new(url.to_string(), device_key.to_string());
            // Chunk the uplink so a big drive doesn't exceed body limits.
            let mut pushed = 0usize;
            for chunk in batch.samples.chunks(256) {
                let sub = collector::SampleBatch {
                    device_id: batch.device_id.clone(),
                    samples: chunk.to_vec(),
                };
                match up.push(&sub) {
                    Ok(()) => pushed += sub.samples.len(),
                    Err(e) => {
                        eprintln!("replay: uplink failed after {pushed}: {e:?}");
                        return;
                    }
                }
            }
            println!("replay: uplinked {pushed} samples to {url}");
            println!("replay: now render a poster for the window above, e.g.");
            println!("        scripts/poster.py <start> <end> --out demo-poster.png");
        }
        None => {
            eprintln!("replay: no --uplink <url> given; generated {n} samples but did not send");
            eprintln!("        example: cargo run -p ecu-sim -- --replay-stage --uplink http://localhost:3000");
        }
    }
}
