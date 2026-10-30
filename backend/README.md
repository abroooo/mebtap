# backend

axum ingest + TimescaleDB query API for EV telemetry.

## Run locally

```sh
# 1. start TimescaleDB (from repo root) — published on host :5433
docker-compose up -d

# 2. run the API — pending migrations are applied automatically on startup
DATABASE_URL=postgres://ev:ev@localhost:5433/ev_tracker DEVICE_KEY=dev cargo run -p backend
```

Migrations in `backend/migrations/` run on startup via `sqlx::migrate!` (tracked
in `_sqlx_migrations`, each applied once). Add new ones as `NNNN_name.sql`; never
edit an already-applied file (checksums are verified) — add a follow-up instead.

Env: `DATABASE_URL` (compose publishes on host **:5433** →
`postgres://ev:ev@localhost:5433/ev_tracker`; code default is `:5432`),
`DEVICE_KEY` (default `dev`), `BIND` (default `0.0.0.0:3000`).

## Endpoints

| Method | Path | Notes |
|---|---|---|
| GET  | `/health` | liveness |
| POST | `/ingest` | body = `meb_proto::SampleBatch` JSON; header `x-device-key` required |
| GET  | `/dids` | distinct DIDs seen |
| GET  | `/latest` | latest value per DID |
| GET  | `/series?did=&from=&to=` | time range (RFC3339 timestamps) |

## End-to-end with the simulator

```sh
docker-compose up -d
DATABASE_URL=postgres://ev:ev@localhost:5433/ev_tracker DEVICE_KEY=dev cargo run -p backend &
cargo run -p ecu-sim -- --uplink http://localhost:3000 --device-key dev
curl 'http://localhost:3000/latest'
```

`cargo check` builds without a live DB (runtime `sqlx::query`, no compile-time macros).
