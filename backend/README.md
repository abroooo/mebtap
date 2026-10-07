# backend

axum ingest + TimescaleDB query API for EV telemetry.

## Run locally

```sh
# 1. start TimescaleDB (from repo root)
docker-compose up -d

# 2. apply schema
psql postgres://ev:ev@localhost:5432/ev_tracker -f backend/migrations/0001_init.sql

# 3. run the API
DEVICE_KEY=dev cargo run -p backend
```

Env: `DATABASE_URL` (default `postgres://ev:ev@localhost:5432/ev_tracker`),
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
docker-compose up -d && psql ... -f backend/migrations/0001_init.sql
DEVICE_KEY=dev cargo run -p backend &
cargo run -p ecu-sim -- --uplink http://localhost:3000 --device-key dev
curl 'http://localhost:3000/latest'
```

`cargo check` builds without a live DB (runtime `sqlx::query`, no compile-time macros).
