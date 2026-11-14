//! Shared test harness: spins up a throwaway TimescaleDB in Docker, applies the
//! real migrations, and builds the axum app against it.
//!
//! Requires Docker to be available (the `timescale/timescaledb:latest-pg16`
//! image is pulled on first use). Each `TestDb` is a fresh, isolated container
//! that is torn down when dropped, so tests never share state.
//!
//! Optimisation note: if container-per-test ever gets too slow, switch to one
//! shared container + a fresh database per test (`CREATE DATABASE`); the schema
//! already re-creates the timescaledb extension in migration 0001.

#![allow(dead_code)] // not every test uses every helper

use std::time::Duration;

use axum::{
    body::{Body, Bytes},
    http::{Request, Response},
    Router,
};
use backend::{router, run_migrations, AppState};
use http_body_util::BodyExt;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
    ContainerAsync, GenericImage, ImageExt,
};
use tower::ServiceExt; // oneshot

/// The device key the test app expects on `/ingest`.
pub const TEST_DEVICE_KEY: &str = "test-key";

/// A throwaway TimescaleDB + a connection pool with migrations applied.
pub struct TestDb {
    // Held only to keep the container alive for the lifetime of the test.
    _container: ContainerAsync<GenericImage>,
    pub pool: PgPool,
}

impl TestDb {
    /// Start a fresh TimescaleDB container and apply all migrations.
    pub async fn new() -> Self {
        let container = GenericImage::new("timescale/timescaledb", "latest-pg16")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stdout(
                "database system is ready to accept connections",
            ))
            .with_env_var("POSTGRES_USER", "ev")
            .with_env_var("POSTGRES_PASSWORD", "ev")
            .with_env_var("POSTGRES_DB", "ev_tracker")
            .start()
            .await
            .expect("start timescaledb container (is Docker running?)");

        let host = container.get_host().await.expect("container host");
        let port = container
            .get_host_port_ipv4(5432.tcp())
            .await
            .expect("mapped port");
        let url = format!("postgres://ev:ev@{host}:{port}/ev_tracker");

        let pool = connect_with_retry(&url).await;
        run_migrations(&pool).await.expect("apply migrations");

        Self { _container: container, pool }
    }

    /// App state wired to this database, using the test device key.
    pub fn state(&self) -> AppState {
        AppState::new(self.pool.clone(), TEST_DEVICE_KEY)
    }

    /// A fresh router. Cheap to rebuild per request (state is a pool handle).
    pub fn app(&self) -> Router {
        router(self.state())
    }

    /// Send one request through the router and return (status, body bytes).
    pub async fn request(&self, req: Request<Body>) -> (axum::http::StatusCode, Bytes) {
        let resp: Response<Body> = self
            .app()
            .oneshot(req)
            .await
            .expect("router should be infallible");
        let status = resp.status();
        let body = resp
            .into_body()
            .collect()
            .await
            .expect("read body")
            .to_bytes();
        (status, body)
    }

    /// GET helper. Returns (status, body bytes).
    pub async fn get(&self, uri: &str) -> (axum::http::StatusCode, Bytes) {
        let req = Request::builder()
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        self.request(req).await
    }

    /// POST JSON helper with the valid device key header.
    pub async fn post_ingest(&self, json: &str) -> (axum::http::StatusCode, Bytes) {
        self.post_ingest_with_key(json, TEST_DEVICE_KEY).await
    }

    /// POST JSON helper with an arbitrary device key (for auth tests).
    pub async fn post_ingest_with_key(
        &self,
        json: &str,
        key: &str,
    ) -> (axum::http::StatusCode, Bytes) {
        let req = Request::builder()
            .method("POST")
            .uri("/ingest")
            .header("content-type", "application/json")
            .header("x-device-key", key)
            .body(Body::from(json.to_owned()))
            .unwrap();
        self.request(req).await
    }
}

/// Postgres logs "ready" once during init before it actually accepts outside
/// connections, so poll until a real connection succeeds.
async fn connect_with_retry(url: &str) -> PgPool {
    let opts = PgPoolOptions::new().max_connections(5);
    let mut last_err = None;
    for _ in 0..60 {
        match opts.clone().connect(url).await {
            Ok(pool) => return pool,
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    panic!("database never became reachable: {last_err:?}");
}

/// Parse a JSON body into `T`, panicking with the raw body on failure.
pub fn json<T: serde::de::DeserializeOwned>(body: &Bytes) -> T {
    serde_json::from_slice(body)
        .unwrap_or_else(|e| panic!("bad JSON: {e}\nbody: {}", String::from_utf8_lossy(body)))
}
