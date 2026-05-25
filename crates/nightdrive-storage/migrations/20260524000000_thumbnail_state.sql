-- 20260524000000_thumbnail_state.sql
-- Tracks whether the per-track cover.png was successfully uploaded as a custom YouTube thumbnail.
-- Default 0 means "not yet set" — eligible for thumbnail-retry sweep.
-- Set to 1 by nightdrive-orchestrator when set_thumbnail_best_effort() returns Ok.

ALTER TABLE tracks ADD COLUMN custom_thumbnail_set INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tracks ADD COLUMN thumbnail_last_attempt_at TEXT;

CREATE INDEX IF NOT EXISTS idx_tracks_thumb_retry
  ON tracks(custom_thumbnail_set, state)
  WHERE custom_thumbnail_set = 0;
