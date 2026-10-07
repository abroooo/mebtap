//! Ingest + query API for EV telemetry. axum over TimescaleDB (Postgres).
//!
//! Uses runtime `sqlx::query` (not the compile-time `query!` macro) so the crate
//! builds without a live database.

use std::env;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use meb_proto::{DecodedValue, SampleBatch};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

#[derive(Clone)]
struct AppState {
    pool: PgPool,
    device_key: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db_url = env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://ev:ev@localhost:5432/ev_tracker".to_string());
    let device_key = env::var("DEVICE_KEY").unwrap_or_else(|_| "dev".to_string());
    let bind = env::var("BIND").unwrap_or_else(|_| "0.0.0.0:3000".to_string());

    let pool = PgPoolOptions::new().max_connections(5).connect(&db_url).await?;

    let state = AppState { pool, device_key };
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/ingest", post(ingest))
        .route("/dids", get(dids))
        .route("/latest", get(latest))
        .route("/series", get(series))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind).await?;
    println!("backend listening on {bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Split a decoded value into the numeric / text columns.
fn split_value(v: &DecodedValue) -> (Option<f64>, Option<String>) {
    match v {
        DecodedValue::Float { val, .. } => (Some(*val), None),
        DecodedValue::Int(i) => (Some(*i as f64), None),
        DecodedValue::Enum(s) => (None, Some(s.clone())),
        DecodedValue::Bytes(_) => (None, None),
    }
}

async fn ingest(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(batch): Json<SampleBatch>,
) -> Result<Json<IngestResp>, (StatusCode, String)> {
    let key = headers.get("x-device-key").and_then(|v| v.to_str().ok()).unwrap_or("");
    if key != st.device_key {
        return Err((StatusCode::UNAUTHORIZED, "bad device key".into()));
    }

    let mut inserted = 0u64;
    for s in &batch.samples {
        let (value_num, value_text) = split_value(&s.value);
        let ts = ms_to_dt(s.ts_ms);
        sqlx::query(
            "INSERT INTO samples (device_id, did, ts, value_num, value_text, raw) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&batch.device_id)
        .bind(s.did as i64)
        .bind(ts)
        .bind(value_num)
        .bind(value_text)
        .bind(&s.raw)
        .execute(&st.pool)
        .await
        .map_err(internal)?;
        inserted += 1;
    }
    Ok(Json(IngestResp { inserted }))
}

async fn dids(State(st): State<AppState>) -> Result<Json<Vec<i64>>, (StatusCode, String)> {
    let rows = sqlx::query("SELECT DISTINCT did FROM samples ORDER BY did")
        .fetch_all(&st.pool)
        .await
        .map_err(internal)?;
    Ok(Json(rows.into_iter().map(|r| r.get::<i64, _>("did")).collect()))
}

async fn latest(State(st): State<AppState>) -> Result<Json<Vec<LatestRow>>, (StatusCode, String)> {
    let rows = sqlx::query(
        "SELECT DISTINCT ON (did) did, ts, value_num, value_text \
         FROM samples ORDER BY did, ts DESC",
    )
    .fetch_all(&st.pool)
    .await
    .map_err(internal)?;
    Ok(Json(
        rows.into_iter()
            .map(|r| LatestRow {
                did: r.get("did"),
                ts: r.get("ts"),
                value_num: r.get("value_num"),
                value_text: r.get("value_text"),
            })
            .collect(),
    ))
}

async fn series(
    State(st): State<AppState>,
    Query(q): Query<SeriesQuery>,
) -> Result<Json<Vec<Point>>, (StatusCode, String)> {
    let from = q.from.unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap());
    let to = q.to.unwrap_or_else(Utc::now);
    let rows = sqlx::query(
        "SELECT ts, value_num, value_text FROM samples \
         WHERE did = $1 AND ts >= $2 AND ts <= $3 ORDER BY ts",
    )
    .bind(q.did)
    .bind(from)
    .bind(to)
    .fetch_all(&st.pool)
    .await
    .map_err(internal)?;
    Ok(Json(
        rows.into_iter()
            .map(|r| Point {
                ts: r.get("ts"),
                value_num: r.get("value_num"),
                value_text: r.get("value_text"),
            })
            .collect(),
    ))
}

fn ms_to_dt(ms: u64) -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp_millis(ms as i64).unwrap_or_else(Utc::now)
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

#[derive(Serialize)]
struct IngestResp {
    inserted: u64,
}

#[derive(Serialize)]
struct LatestRow {
    did: i64,
    ts: DateTime<Utc>,
    value_num: Option<f64>,
    value_text: Option<String>,
}

#[derive(Serialize)]
struct Point {
    ts: DateTime<Utc>,
    value_num: Option<f64>,
    value_text: Option<String>,
}

#[derive(Deserialize)]
struct SeriesQuery {
    did: i64,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use meb_proto::Sample;

    #[test]
    fn ingest_payload_roundtrips() {
        let batch = SampleBatch {
            device_id: "dev1".into(),
            samples: vec![Sample {
                did: 0x028C,
                ts_ms: 1_700_000_000_000,
                raw: vec![0x00, 0xFA],
                value: DecodedValue::Float { val: 100.0, unit: "%".into() },
            }],
        };
        let json = serde_json::to_string(&batch).unwrap();
        let back: SampleBatch = serde_json::from_str(&json).unwrap();
        assert_eq!(back.samples.len(), 1);
        assert_eq!(back.device_id, "dev1");
    }

    #[test]
    fn split_value_maps_columns() {
        assert_eq!(split_value(&DecodedValue::Float { val: 1.5, unit: "V".into() }), (Some(1.5), None));
        assert_eq!(split_value(&DecodedValue::Int(7)), (Some(7.0), None));
        assert_eq!(split_value(&DecodedValue::Enum("driving".into())), (None, Some("driving".into())));
        assert_eq!(split_value(&DecodedValue::Bytes(vec![1, 2])), (None, None));
    }
}
