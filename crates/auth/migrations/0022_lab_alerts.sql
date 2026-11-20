-- ADR 015 amendment 2026-11-20 - Lab alerts. Both tables are written in the
-- same transaction as the outbox row of the alert they record, so an alert
-- is reported once across restarts and replicas.

-- The budget edge-trigger state of a key or group after its last Lab alert.
-- `budget_max_micro` names the budget period: a row recorded under another
-- cap is ignored at boot.
CREATE TABLE lab_budget_alert_state (
    scope            TEXT NOT NULL,              -- key | group
    subject_id       TEXT NOT NULL,
    budget_max_micro INTEGER NOT NULL,
    crossed          INTEGER NOT NULL,           -- thresholds already reported
    exhausted        INTEGER NOT NULL,           -- budget.exhausted reported
    updated_ms       INTEGER NOT NULL,
    PRIMARY KEY (scope, subject_id)
);

-- One row per `key.expiring` event sent: a key, the expiry it was sent for
-- and the milestone (7, 1, 0 days left).
CREATE TABLE lab_key_expiry_notices (
    key_id     TEXT NOT NULL,
    expires_at INTEGER NOT NULL,                 -- unix seconds
    days_left  INTEGER NOT NULL,
    created_ms INTEGER NOT NULL,
    PRIMARY KEY (key_id, expires_at, days_left)
);
