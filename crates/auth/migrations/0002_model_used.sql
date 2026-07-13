-- M6: record which model actually served a request (a fallback may differ from
-- the requested `model`), mirroring the `x-ferrogate-model-used` response
-- header. Defaults to the empty string for pre-M6 rows; the writer always sets
-- it (to the served model, == `model` when no fallback fired).
ALTER TABLE usage_log ADD COLUMN model_used TEXT NOT NULL DEFAULT '';
