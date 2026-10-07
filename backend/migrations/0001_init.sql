CREATE EXTENSION IF NOT EXISTS timescaledb;

-- Decoded telemetry samples.
CREATE TABLE IF NOT EXISTS samples (
    device_id  TEXT        NOT NULL,
    did        BIGINT      NOT NULL,
    ts         TIMESTAMPTZ NOT NULL,
    value_num  DOUBLE PRECISION,
    value_text TEXT,
    raw        BYTEA       NOT NULL
);

SELECT create_hypertable('samples', 'ts', if_not_exists => TRUE);

CREATE INDEX IF NOT EXISTS samples_did_ts_idx ON samples (did, ts DESC);

-- Raw frames for responses we could not match/decode.
CREATE TABLE IF NOT EXISTS raw_frames (
    device_id TEXT        NOT NULL,
    ts        TIMESTAMPTZ NOT NULL,
    can_id    BIGINT      NOT NULL,
    data      BYTEA       NOT NULL
);

SELECT create_hypertable('raw_frames', 'ts', if_not_exists => TRUE);
