-- Issue #66: soft-delete for virtual keys. Deleting a key is a tombstone,
-- never a row removal: `usage_log.key_id` references the key id, so a hard
-- delete would orphan usage history and break per-key attribution/audit.
-- NULL = active; a unix-seconds timestamp = deleted at that moment. Deleted
-- keys never authenticate, never load into the in-memory table, are hidden
-- from the default admin list, and reject further PATCH/DELETE/rotate.
ALTER TABLE virtual_keys ADD COLUMN deleted_at INTEGER;
