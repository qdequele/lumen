-- ADR 014: the route a virtual-model request took, e.g.
-- `acme/chat>acme/eu>mistral-large`. NULL for a foundation model called
-- directly, and for rows written before the column existed.
ALTER TABLE usage_log ADD COLUMN route TEXT;
