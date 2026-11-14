//! Ingest + query API for EV telemetry. axum over TimescaleDB (Postgres).
//!
//! Uses runtime `sqlx::query` (not the compile-time `query!` macro) so the crate
//! builds without a live database.
//!
//! The router and app state are exposed from the library so integration tests
//! (see `tests/`) can drive the handlers against a throwaway database without
//! going through `main`.

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Utc};
use meb_proto::{DecodedValue, SampleBatch};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};

/// Shared state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    /// Expected value of the `x-device-key` header on `/ingest`.
    pub device_key: String,
}

impl AppState {
    pub fn new(pool: PgPool, device_key: impl Into<String>) -> Self {
        Self { pool, device_key: device_key.into() }
    }
}

/// Build the application router. Shared by `main` and the integration tests.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ingest", post(ingest))
        .route("/dids", get(dids))
        .route("/latest", get(latest))
        .route("/series", get(series))
        .with_state(state)
}

/// Apply any pending migrations. `sqlx` tracks applied ones in `_sqlx_migrations`,
/// so this runs each exactly once (shared by `main` and the test harness).
pub async fn run_migrations(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

async fn health() -> &'static str {
    "ok"
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
    let key = headers
        .get("x-device-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if key != st.device_key {
        return Err((StatusCode::UNAUTHORIZED, "bad device key".into()));
    }

    let mut inserted = 0u64;
    for s in &batch.samples {
        let (value_num, value_text) = split_value(&s.value);
        let ts = ms_to_dt(s.ts_ms);
        // ON CONFLICT DO NOTHING: drop uplink-retry duplicates.
        let res = sqlx::query(
            "INSERT INTO samples (device_id, did, ts, value_num, value_text, raw) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (ts, device_id, did) DO NOTHING",
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
        inserted += res.rows_affected();
    }
    Ok(Json(IngestResp { inserted }))
}

async fn dids(State(st): State<AppState>) -> Result<Json<Vec<i64>>, (StatusCode, String)> {
    let rows = sqlx::query("SELECT DISTINCT did FROM samples ORDER BY did")
        .fetch_all(&st.pool)
        .await
        .map_err(internal)?;
    Ok(Json(
        rows.into_iter().map(|r| r.get::<i64, _>("did")).collect(),
    ))
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
    let from = q
        .from
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap());
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

#[derive(Serialize, Deserialize)]
pub struct IngestResp {
    pub inserted: u64,
}

#[derive(Serialize, Deserialize)]
pub struct LatestRow {
    pub did: i64,
    pub ts: DateTime<Utc>,
    pub value_num: Option<f64>,
    pub value_text: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Point {
    pub ts: DateTime<Utc>,
    pub value_num: Option<f64>,
    pub value_text: Option<String>,
}

#[derive(Deserialize)]
pub struct SeriesQuery {
    pub did: i64,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
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
                value: DecodedValue::Float {
                    val: 100.0,
                    unit: "%".into(),
                },
            }],
        };
        let json = serde_json::to_string(&batch).unwrap();
        let back: SampleBatch = serde_json::from_str(&json).unwrap();
        assert_eq!(back.samples.len(), 1);
        assert_eq!(back.device_id, "dev1");
    }

    #[test]
    fn split_value_maps_columns() {
        assert_eq!(
            split_value(&DecodedValue::Float {
                val: 1.5,
                unit: "V".into()
            }),
            (Some(1.5), None)
        );
        assert_eq!(split_value(&DecodedValue::Int(7)), (Some(7.0), None));
        assert_eq!(
            split_value(&DecodedValue::Enum("driving".into())),
            (None, Some("driving".into()))
        );
        assert_eq!(split_value(&DecodedValue::Bytes(vec![1, 2])), (None, None));
    }
}
