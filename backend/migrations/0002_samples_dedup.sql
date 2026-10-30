-- De-duplicate telemetry samples and make ingest idempotent.
--
-- Uplink is at-least-once: on a lost ACK the device re-sends the identical
-- batch (same ts/device_id/did, since ts is stamped once at poll time), so a
-- retry can insert a duplicate row. Distinct readings never collide on this
-- triple (one poll per DID per pass, ms-scale CAN round-trips), so a unique
-- index is safe and lets `INSERT ... ON CONFLICT DO NOTHING` drop the retry.
--
-- A UNIQUE INDEX (not a named constraint) is used so this migration is
-- idempotent via IF NOT EXISTS. On a TimescaleDB hypertable the unique index
-- must include the partition column `ts` — it does.

-- Remove any duplicates already present, keeping one row per triple, or the
-- CREATE UNIQUE INDEX below would fail.
DELETE FROM samples a
USING samples b
WHERE a.ctid < b.ctid
  AND a.ts = b.ts
  AND a.device_id = b.device_id
  AND a.did = b.did;

CREATE UNIQUE INDEX IF NOT EXISTS samples_uniq ON samples (ts, device_id, did);
