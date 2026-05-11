-- nightdrive · initial schema
-- run via sqlx-cli or the nightdrive-storage::migrate() entrypoint.

CREATE TABLE IF NOT EXISTS tracks (
    id              TEXT PRIMARY KEY,          -- ulid / uuid v7
    title           TEXT NOT NULL,
    bpm             INTEGER NOT NULL,
    key             TEXT NOT NULL,
    seed            INTEGER NOT NULL,
    spec_json       TEXT NOT NULL,             -- raw CompositionSpec
    state           TEXT NOT NULL,             -- nightdrive_core::TrackState: pending|spec_generated|audio_rendered|cover_rendered|audio_mastered|video_encoded|published|failed
    audio_path      TEXT,
    cover_path      TEXT,
    visualizer_path TEXT,                       -- final mp4
    duration_secs   INTEGER,
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_tracks_state      ON tracks(state);
CREATE INDEX IF NOT EXISTS idx_tracks_created_at ON tracks(created_at);

CREATE TABLE IF NOT EXISTS uploads (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id            TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    youtube_video_id    TEXT,
    upload_url          TEXT,                   -- resumable session
    bytes_uploaded      INTEGER NOT NULL DEFAULT 0,
    status              TEXT NOT NULL,          -- queued|uploading|complete|failed
    error               TEXT,
    started_at          TEXT NOT NULL DEFAULT (datetime('now')),
    completed_at        TEXT
);

CREATE INDEX IF NOT EXISTS idx_uploads_track  ON uploads(track_id);
CREATE INDEX IF NOT EXISTS idx_uploads_status ON uploads(status);

CREATE TABLE IF NOT EXISTS livestream_rotation_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id    TEXT NOT NULL REFERENCES tracks(id),
    started_at  TEXT NOT NULL DEFAULT (datetime('now')),
    ended_at    TEXT,
    listeners   INTEGER                        -- snapshot from yt analytics, optional
);

CREATE INDEX IF NOT EXISTS idx_rotation_track ON livestream_rotation_log(track_id);
