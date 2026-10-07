//! End-to-end: collector ↔ mock ECU → ReqwestUplink → mock HTTP server.
//! Verifies the batch is POSTed to /ingest with the device-key header and a
//! body that deserializes back into a SampleBatch. No real backend/DB needed.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc::{channel, Sender};
use std::thread;
use std::time::Duration;

use collector::{Collector, LaneScheduler, MemStore, SampleBatch, Uplink};
use ecu_sim::{pair, MebEcuSim, ReqwestUplink};

struct Captured {
    path: String,
    device_key: Option<String>,
    body: String,
}

/// Minimal one-shot HTTP/1.1 server. Serves `expect` requests, 200 OK each,
/// forwarding what it captured over `tx`, then returns.
fn serve(expect: usize, tx: Sender<Captured>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for _ in 0..expect {
            let Ok((mut stream, _)) = listener.accept() else { break };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            // Read until headers complete.
            let header_end = loop {
                let n = stream.read(&mut tmp).unwrap_or(0);
                if n == 0 {
                    break buf.len();
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = find_subslice(&buf, b"\r\n\r\n") {
                    break p + 4;
                }
            };
            let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
            let content_len = head
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                .unwrap_or(0);
            while buf.len() < header_end + content_len {
                let n = stream.read(&mut tmp).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let body = String::from_utf8_lossy(&buf[header_end..]).to_string();
            let path = head.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("").to_string();
            let device_key = head
                .lines()
                .find_map(|l| l.to_ascii_lowercase().strip_prefix("x-device-key:").map(|v| v.trim().to_string()));
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            let _ = tx.send(Captured { path, device_key, body });
        }
    });
    addr
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[test]
fn collector_batch_posts_to_ingest() {
    let (tx, rx) = channel();
    let addr = serve(1, tx);

    // Collector against the mock ECU.
    let (bus, link) = pair();
    MebEcuSim::new(link).spawn();
    let sched = LaneScheduler::new(Duration::from_millis(0), Duration::from_millis(0));
    let mut col = Collector::new(bus, MemStore::default(), sched, "sim-esp32");
    for _ in 0..3 {
        col.poll_once();
        thread::sleep(Duration::from_millis(5));
    }

    // Push one batch.
    let mut up = ReqwestUplink::new(format!("http://{addr}"), "dev");
    let batch = col.take_batch(10_000).expect("some samples");
    up.push(&batch).expect("uplink ok");

    let cap = rx.recv_timeout(Duration::from_secs(5)).expect("server got a request");
    assert_eq!(cap.path, "/ingest");
    assert_eq!(cap.device_key.as_deref(), Some("dev"));
    let parsed: SampleBatch = serde_json::from_str(&cap.body).expect("body is a SampleBatch");
    assert_eq!(parsed.device_id, "sim-esp32");
    assert!(!parsed.samples.is_empty());
}
