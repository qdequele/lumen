-- ADR 015 - durable outbox for billing usage events. A row is inserted in
-- the same transaction as the spend it bills and is never deleted before
-- the control plane acknowledges it.
CREATE TABLE usage_outbox (
    id              TEXT PRIMARY KEY,           -- UUIDv7, the event id
    body            TEXT NOT NULL,              -- serialized event JSON
    created_ms      INTEGER NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_ms INTEGER NOT NULL,
    delivered_ms    INTEGER                     -- NULL = pending
);
CREATE INDEX idx_usage_outbox_due ON usage_outbox (delivered_ms, next_attempt_ms);
