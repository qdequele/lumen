-- Account virtual models (ADR 022): one document per Lab account, outside
-- the platform config document. `document` is a JSON array of ADR 014
-- virtual-model objects; `hash` (BLAKE3 hex of `document`) is the account's
-- If-Match version; `generation` is one counter across all rows (max + 1 on
-- each write) so other processes can poll for changes. A row is never
-- deleted by the API: an account whose last model is deleted keeps `[]`.
CREATE TABLE account_virtual_models (
    account_ref TEXT PRIMARY KEY NOT NULL,
    document    TEXT NOT NULL,
    hash        TEXT NOT NULL,
    generation  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);
CREATE INDEX idx_account_virtual_models_generation
    ON account_virtual_models (generation);
