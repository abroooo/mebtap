//! Endpoint integration tests against a throwaway TimescaleDB.
//!
//! Run with: `cargo test -p backend --test endpoints`
//! Requires Docker (see tests/common/mod.rs). These are ignored by a plain
//! unit-test run only if Docker is unavailable (they will otherwise fail at
//! container start with a clear message).

mod common;

use axum::http::StatusCode;
use backend::{IngestResp, LatestRow, Point};
use common::{json, TestDb, TEST_DEVICE_KEY};
use meb_proto::{DecodedValue, Sample, SampleBatch};

const SOC_DID: u32 = 0x028C; // 652
const SOC_DID_I64: i64 = SOC_DID as i64;

/// Build a one-device batch of SoC float samples as `(ts_ms, value)` pairs.
fn soc_batch(device_id: &str, points: &[(u64, f64)]) -> String {
    let batch = SampleBatch {
        device_id: device_id.to_string(),
        samples: points
            .iter()
            .map(|(ts_ms, val)| Sample {
                did: SOC_DID,
                ts_ms: *ts_ms,
                raw: vec![0x00, 0xFA],
                value: DecodedValue::Float { val: *val, unit: "%".into() },
            })
            .collect(),
    };
    serde_json::to_string(&batch).unwrap()
}

#[tokio::test]
async fn health_ok() {
    let db = TestDb::new().await;
    let (status, body) = db.get("/health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], b"ok");
}

#[tokio::test]
async fn ingest_rejects_bad_key() {
    let db = TestDb::new().await;
    let body = soc_batch("dev1", &[(1_700_000_000_000, 55.0)]);
    let (status, _) = db.post_ingest_with_key(&body, "wrong-key").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // And nothing was written.
    let (status, body) = db.get("/dids").await;
    assert_eq!(status, StatusCode::OK);
    let dids: Vec<i64> = json(&body);
    assert!(dids.is_empty(), "no rows should exist, got {dids:?}");
}

#[tokio::test]
async fn ingest_then_read_back() {
    let db = TestDb::new().await;
    let body = soc_batch(
        "dev1",
        &[(1_700_000_000_000, 40.0), (1_700_000_060_000, 55.0)],
    );

    let (status, resp) = db.post_ingest(&body).await;
    assert_eq!(status, StatusCode::OK);
    let resp: IngestResp = json(&resp);
    assert_eq!(resp.inserted, 2);

    // /dids
    let (_, body) = db.get("/dids").await;
    let dids: Vec<i64> = json(&body);
    assert_eq!(dids, vec![SOC_DID_I64]);

    // /latest -> most recent value (55.0 at the later ts)
    let (_, body) = db.get("/latest").await;
    let latest: Vec<LatestRow> = json(&body);
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].did, SOC_DID_I64);
    assert_eq!(latest[0].value_num, Some(55.0));

    // /series -> both points in chronological order
    let (_, body) = db.get(&format!("/series?did={SOC_DID_I64}")).await;
    let points: Vec<Point> = json(&body);
    assert_eq!(points.len(), 2);
    assert_eq!(points[0].value_num, Some(40.0));
    assert_eq!(points[1].value_num, Some(55.0));
}

#[tokio::test]
async fn ingest_is_idempotent() {
    let db = TestDb::new().await;
    let body = soc_batch("dev1", &[(1_700_000_000_000, 40.0)]);

    let (_, resp) = db.post_ingest(&body).await;
    assert_eq!(json::<IngestResp>(&resp).inserted, 1);

    // Re-sending the identical batch (uplink retry) must insert nothing.
    let (_, resp) = db.post_ingest(&body).await;
    assert_eq!(
        json::<IngestResp>(&resp).inserted,
        0,
        "duplicate (ts,device_id,did) must be dropped by ON CONFLICT"
    );

    let (_, body) = db.get(&format!("/series?did={SOC_DID_I64}")).await;
    assert_eq!(json::<Vec<Point>>(&body).len(), 1);
}

#[tokio::test]
async fn series_respects_time_range() {
    let db = TestDb::new().await;
    // t0 = 2023-11-14T22:13:20Z, t1 = +60s.
    let t0 = 1_700_000_000_000u64;
    let t1 = t0 + 60_000;
    let body = soc_batch("dev1", &[(t0, 40.0), (t1, 55.0)]);
    db.post_ingest(&body).await;

    // Window that starts after t0 should only include t1's point.
    let from = "2023-11-14T22:13:50Z"; // t0 + 30s
    let (status, body) = db
        .get(&format!("/series?did={SOC_DID_I64}&from={from}"))
        .await;
    assert_eq!(status, StatusCode::OK);
    let points: Vec<Point> = json(&body);
    assert_eq!(points.len(), 1, "only the later sample is in range");
    assert_eq!(points[0].value_num, Some(55.0));
}

/// The device key constant is wired through; guard against accidental drift.
#[test]
fn device_key_constant_is_nonempty() {
    assert!(!TEST_DEVICE_KEY.is_empty());
}
