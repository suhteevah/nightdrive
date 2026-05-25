//! nightdrive-storage — sqlx + SQLite: tracks table, uploads, livestream_rotation_log.
//!
//! Scope is deliberately small: every method maps to one of the named operations in
//! `docs/ROADMAP.md` § N1.3. No general-purpose query builder, no transaction wrapper,
//! no caching layer. The pipeline's writers go through [`Tracks::insert`] /
//! [`Tracks::update_state`]; the publisher goes through [`Uploads`]; the livestream
//! supervisor goes through [`LivestreamRotation::next_track`].
//!
//! ## Connection lifecycle
//!
//! [`Db::connect_and_migrate`] opens a [`sqlx::SqlitePool`] against an on-disk file
//! (creating the file if missing) and runs every embedded migration. The pool is
//! `Clone` and cheap to share across tokio tasks.
//!
//! ## Error mapping
//!
//! Every sqlx failure surfaces as [`NightdriveError::Storage`] with the original
//! error stringified. Domain-level errors (e.g. "track not found") get their own
//! `Storage(...)` messages so callers can match on prefix.

use nightdrive_core::{CompositionSpec, NightdriveError, NightdriveResult, TrackId, TrackState};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use std::path::Path;
use std::str::FromStr;
use tracing::{debug, info, instrument};

// =============================================================================
// Pool wrapper
// =============================================================================

/// Owned handle to the nightdrive SQLite database. Cheaply cloneable —
/// it wraps a [`sqlx::SqlitePool`] which itself is `Arc`-shaped.
#[derive(Debug, Clone)]
pub struct Db {
    pool: SqlitePool,
}

impl Db {
    /// Open the SQLite database at `path` (creating the file if missing),
    /// then run every embedded migration. Returns a [`Db`] handle ready
    /// for the [`Tracks`] / [`Uploads`] / [`LivestreamRotation`] APIs.
    #[instrument(skip_all, fields(path = %path.as_ref().display()))]
    pub async fn connect_and_migrate(path: impl AsRef<Path>) -> NightdriveResult<Self> {
        let path = path.as_ref();
        let url = format!("sqlite://{}?mode=rwc", path.display());
        // `?mode=rwc` = read+write+create. Without this, sqlx refuses to create a
        // missing file even though SQLite itself happily would.
        let opts = SqliteConnectOptions::from_str(&url)
            .map_err(|e| NightdriveError::Storage(format!("invalid sqlite url {url}: {e}")))?
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);

        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await
            .map_err(|e| NightdriveError::Storage(format!("open sqlite: {e}")))?;

        info!("running migrations");
        sqlx::migrate!()
            .run(&pool)
            .await
            .map_err(|e| NightdriveError::Storage(format!("migrate: {e}")))?;

        Ok(Self { pool })
    }

    /// Direct pool access for callers that need to compose queries that aren't
    /// covered by the typed APIs. Use sparingly — every new query should
    /// preferably become a typed method here.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}

// =============================================================================
// TrackRow — what the tracks table actually stores
// =============================================================================

#[derive(Debug, Clone)]
pub struct TrackRow {
    pub id: TrackId,
    pub title: String,
    pub bpm: i64,
    pub key: String,
    pub seed: i64,
    pub spec_json: String,
    pub state: TrackState,
    pub audio_path: Option<String>,
    pub cover_path: Option<String>,
    pub visualizer_path: Option<String>,
    pub duration_secs: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
    pub custom_thumbnail_set: bool,
    pub thumbnail_last_attempt_at: Option<String>,
}

fn parse_state(raw: &str) -> NightdriveResult<TrackState> {
    Ok(match raw {
        "pending" => TrackState::Pending,
        "spec_generated" => TrackState::SpecGenerated,
        "audio_rendered" => TrackState::AudioRendered,
        "cover_rendered" => TrackState::CoverRendered,
        "audio_mastered" => TrackState::AudioMastered,
        "video_encoded" => TrackState::VideoEncoded,
        "published" => TrackState::Published,
        "failed" => TrackState::Failed,
        other => {
            return Err(NightdriveError::Storage(format!(
                "unknown track state in db: {other:?}"
            )));
        }
    })
}

fn row_to_track(row: &sqlx::sqlite::SqliteRow) -> NightdriveResult<TrackRow> {
    let id_str: String = row.try_get("id").map_err(map_sqlx)?;
    let state_str: String = row.try_get("state").map_err(map_sqlx)?;
    Ok(TrackRow {
        id: TrackId(id_str),
        title: row.try_get("title").map_err(map_sqlx)?,
        bpm: row.try_get("bpm").map_err(map_sqlx)?,
        key: row.try_get("key").map_err(map_sqlx)?,
        seed: row.try_get("seed").map_err(map_sqlx)?,
        spec_json: row.try_get("spec_json").map_err(map_sqlx)?,
        state: parse_state(&state_str)?,
        audio_path: row.try_get("audio_path").map_err(map_sqlx)?,
        cover_path: row.try_get("cover_path").map_err(map_sqlx)?,
        visualizer_path: row.try_get("visualizer_path").map_err(map_sqlx)?,
        duration_secs: row.try_get("duration_secs").map_err(map_sqlx)?,
        created_at: row.try_get("created_at").map_err(map_sqlx)?,
        updated_at: row.try_get("updated_at").map_err(map_sqlx)?,
        custom_thumbnail_set: row
            .try_get::<i64, _>("custom_thumbnail_set")
            .map_err(map_sqlx)? != 0,
        thumbnail_last_attempt_at: row
            .try_get("thumbnail_last_attempt_at")
            .map_err(map_sqlx)?,
    })
}

fn map_sqlx(e: sqlx::Error) -> NightdriveError {
    NightdriveError::Storage(e.to_string())
}

// =============================================================================
// Tracks
// =============================================================================

pub struct Tracks;

impl Tracks {
    /// Insert a new track row in the `pending` state. The `spec_json` column
    /// stores the serialized [`CompositionSpec`] verbatim; the indexed columns
    /// (title, bpm, key, duration_secs) are denormalized copies of the spec
    /// fields for fast filtering without a JSON-parse round trip per row.
    #[instrument(skip_all, fields(track_id = %spec.track_id, seed))]
    pub async fn insert(db: &Db, spec: &CompositionSpec, seed: i64) -> NightdriveResult<()> {
        let spec_json = serde_json::to_string(spec)
            .map_err(|e| NightdriveError::Storage(format!("serialize spec: {e}")))?;

        // INSERT OR IGNORE so re-runs of pipeline_one_album (which call
        // insert idempotently per track) don't blow up on the UNIQUE constraint
        // for tracks.id. Existing rows keep their state + spec — updates flow
        // through `update_state` instead.
        sqlx::query(
            "INSERT OR IGNORE INTO tracks (id, title, bpm, key, seed, spec_json, state, duration_secs) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(spec.track_id.as_str())
        .bind(&spec.title)
        .bind(spec.bpm as i64)
        .bind(&spec.musical_key)
        .bind(seed)
        .bind(&spec_json)
        .bind(TrackState::Pending.as_str())
        .bind(spec.duration_seconds as i64)
        .execute(&db.pool)
        .await
        .map_err(map_sqlx)?;

        debug!("track inserted");
        Ok(())
    }

    /// Transition a track to a new state. Bumps `updated_at` to `now`. Errors
    /// if the track doesn't exist (sqlx reports rowcount=0).
    #[instrument(skip(db), fields(track_id = %id, state = %state.as_str()))]
    pub async fn update_state(
        db: &Db,
        id: &TrackId,
        state: TrackState,
    ) -> NightdriveResult<()> {
        let result = sqlx::query(
            "UPDATE tracks SET state = ?, updated_at = datetime('now') WHERE id = ?",
        )
        .bind(state.as_str())
        .bind(id.as_str())
        .execute(&db.pool)
        .await
        .map_err(map_sqlx)?;

        if result.rows_affected() == 0 {
            return Err(NightdriveError::Storage(format!(
                "track not found: {id}"
            )));
        }
        debug!("track state updated");
        Ok(())
    }

    /// List tracks, optionally filtered by state, ordered by `created_at ASC`.
    #[instrument(skip(db), fields(filter = ?filter))]
    pub async fn list(
        db: &Db,
        filter: Option<TrackState>,
    ) -> NightdriveResult<Vec<TrackRow>> {
        let rows = match filter {
            Some(state) => {
                sqlx::query(
                    "SELECT id, title, bpm, key, seed, spec_json, state, audio_path, \
                     cover_path, visualizer_path, duration_secs, created_at, updated_at, \
                     custom_thumbnail_set, thumbnail_last_attempt_at \
                     FROM tracks WHERE state = ? ORDER BY created_at ASC",
                )
                .bind(state.as_str())
                .fetch_all(&db.pool)
                .await
            }
            None => {
                sqlx::query(
                    "SELECT id, title, bpm, key, seed, spec_json, state, audio_path, \
                     cover_path, visualizer_path, duration_secs, created_at, updated_at, \
                     custom_thumbnail_set, thumbnail_last_attempt_at \
                     FROM tracks ORDER BY created_at ASC",
                )
                .fetch_all(&db.pool)
                .await
            }
        }
        .map_err(map_sqlx)?;

        rows.iter().map(row_to_track).collect()
    }

    /// Fetch a single track by id, or `None` if missing.
    #[instrument(skip(db), fields(track_id = %id))]
    pub async fn get(db: &Db, id: &TrackId) -> NightdriveResult<Option<TrackRow>> {
        let maybe_row = sqlx::query(
            "SELECT id, title, bpm, key, seed, spec_json, state, audio_path, \
             cover_path, visualizer_path, duration_secs, created_at, updated_at, \
             custom_thumbnail_set, thumbnail_last_attempt_at \
             FROM tracks WHERE id = ?",
        )
        .bind(id.as_str())
        .fetch_optional(&db.pool)
        .await
        .map_err(map_sqlx)?;

        maybe_row.as_ref().map(row_to_track).transpose()
    }

    /// List published tracks that have a YouTube video ID but whose thumbnail
    /// has not yet been successfully set. Used by the thumbnail-retry sweep
    /// (`nightdrive-cli thumbnails retry-failed`) and the album post-publish pass.
    ///
    /// The `youtube_video_id` is resolved from the most recent completed upload
    /// for the track. `limit` caps the returned set for incremental retry passes
    /// (use 100 to drain the backlog in one shot).
    #[instrument(skip(db), fields(limit))]
    pub async fn list_published_with_missing_thumbnail(
        db: &Db,
        limit: i64,
    ) -> NightdriveResult<Vec<(TrackRow, String)>> {
        // Join to uploads to pick up the video_id. A track can have >1 upload
        // row (retries); take the most recent completed one.
        let rows = sqlx::query(
            "SELECT t.id, t.title, t.bpm, t.key, t.seed, t.spec_json, t.state, \
                    t.audio_path, t.cover_path, t.visualizer_path, t.duration_secs, \
                    t.created_at, t.updated_at, t.custom_thumbnail_set, \
                    t.thumbnail_last_attempt_at, u.youtube_video_id \
             FROM tracks t \
             JOIN uploads u ON u.track_id = t.id AND u.status = 'complete' \
             WHERE t.state = 'published' \
               AND t.custom_thumbnail_set = 0 \
               AND u.youtube_video_id IS NOT NULL \
             ORDER BY u.completed_at DESC \
             LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&db.pool)
        .await
        .map_err(map_sqlx)?;

        rows.iter()
            .map(|row| {
                let track = row_to_track(row)?;
                let video_id: String =
                    row.try_get("youtube_video_id").map_err(map_sqlx)?;
                Ok((track, video_id))
            })
            .collect()
    }

    /// Mark a track's custom thumbnail as successfully set. Clears the
    /// retry-eligible flag and stamps the attempt timestamp.
    #[instrument(skip(db), fields(track_id = %track_id))]
    pub async fn mark_thumbnail_set(db: &Db, track_id: &TrackId) -> NightdriveResult<()> {
        sqlx::query(
            "UPDATE tracks \
             SET custom_thumbnail_set = 1, thumbnail_last_attempt_at = datetime('now'), \
                 updated_at = datetime('now') \
             WHERE id = ?",
        )
        .bind(track_id.as_str())
        .execute(&db.pool)
        .await
        .map_err(map_sqlx)
        .map(|_| ())
    }

    /// Record a failed or rate-limited thumbnail attempt without marking it
    /// as set. The track remains in the retry-eligible pool; only the
    /// last-attempt timestamp advances (so callers can implement a cooldown).
    #[instrument(skip(db), fields(track_id = %track_id))]
    pub async fn mark_thumbnail_attempted(db: &Db, track_id: &TrackId) -> NightdriveResult<()> {
        sqlx::query(
            "UPDATE tracks \
             SET thumbnail_last_attempt_at = datetime('now'), updated_at = datetime('now') \
             WHERE id = ?",
        )
        .bind(track_id.as_str())
        .execute(&db.pool)
        .await
        .map_err(map_sqlx)
        .map(|_| ())
    }
}

// =============================================================================
// Uploads
// =============================================================================

#[derive(Debug, Clone)]
pub struct UploadRow {
    pub id: i64,
    pub track_id: TrackId,
    pub youtube_video_id: Option<String>,
    pub upload_url: Option<String>,
    pub bytes_uploaded: i64,
    pub status: String,
    pub error: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
}

pub struct Uploads;

impl Uploads {
    /// Insert a fresh upload row in the `queued` state. Returns the auto-incremented
    /// `id` so the caller can pin the resumable session URL later via
    /// [`Uploads::set_youtube_id`] / [`Uploads::set_upload_url`].
    #[instrument(skip(db), fields(track_id = %track_id))]
    pub async fn insert(db: &Db, track_id: &TrackId) -> NightdriveResult<i64> {
        let row = sqlx::query(
            "INSERT INTO uploads (track_id, status) VALUES (?, 'queued') RETURNING id",
        )
        .bind(track_id.as_str())
        .fetch_one(&db.pool)
        .await
        .map_err(map_sqlx)?;

        let id: i64 = row.try_get("id").map_err(map_sqlx)?;
        debug!(upload_id = id, "upload row inserted");
        Ok(id)
    }

    /// Stamp the youtube_video_id once the resumable upload completes. Also
    /// flips `status` to `complete` and sets `completed_at` to now.
    #[instrument(skip(db), fields(upload_id, video_id))]
    pub async fn set_youtube_id(
        db: &Db,
        upload_id: i64,
        video_id: &str,
    ) -> NightdriveResult<()> {
        let result = sqlx::query(
            "UPDATE uploads SET youtube_video_id = ?, status = 'complete', \
             completed_at = datetime('now') WHERE id = ?",
        )
        .bind(video_id)
        .bind(upload_id)
        .execute(&db.pool)
        .await
        .map_err(map_sqlx)?;

        if result.rows_affected() == 0 {
            return Err(NightdriveError::Storage(format!(
                "upload not found: {upload_id}"
            )));
        }
        Ok(())
    }

    /// List the most recent `limit` uploads, ordered by `started_at DESC` (most
    /// recent first). The cli's `uploads list` subcommand uses this for the
    /// operator-facing view of recent activity.
    #[instrument(skip(db), fields(limit))]
    pub async fn list_recent(db: &Db, limit: u32) -> NightdriveResult<Vec<UploadRow>> {
        let rows = sqlx::query(
            "SELECT id, track_id, youtube_video_id, upload_url, bytes_uploaded, \
             status, error, started_at, completed_at FROM uploads \
             ORDER BY started_at DESC LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&db.pool)
        .await
        .map_err(map_sqlx)?;

        rows.into_iter()
            .map(|row| {
                let track_id_str: String = row.try_get("track_id").map_err(map_sqlx)?;
                Ok(UploadRow {
                    id: row.try_get("id").map_err(map_sqlx)?,
                    track_id: TrackId(track_id_str),
                    youtube_video_id: row.try_get("youtube_video_id").map_err(map_sqlx)?,
                    upload_url: row.try_get("upload_url").map_err(map_sqlx)?,
                    bytes_uploaded: row.try_get("bytes_uploaded").map_err(map_sqlx)?,
                    status: row.try_get("status").map_err(map_sqlx)?,
                    error: row.try_get("error").map_err(map_sqlx)?,
                    started_at: row.try_get("started_at").map_err(map_sqlx)?,
                    completed_at: row.try_get("completed_at").map_err(map_sqlx)?,
                })
            })
            .collect()
    }

    /// Fetch an upload row by id, or `None` if missing.
    pub async fn get(db: &Db, upload_id: i64) -> NightdriveResult<Option<UploadRow>> {
        let maybe = sqlx::query(
            "SELECT id, track_id, youtube_video_id, upload_url, bytes_uploaded, \
             status, error, started_at, completed_at FROM uploads WHERE id = ?",
        )
        .bind(upload_id)
        .fetch_optional(&db.pool)
        .await
        .map_err(map_sqlx)?;

        match maybe {
            None => Ok(None),
            Some(row) => {
                let track_id_str: String = row.try_get("track_id").map_err(map_sqlx)?;
                Ok(Some(UploadRow {
                    id: row.try_get("id").map_err(map_sqlx)?,
                    track_id: TrackId(track_id_str),
                    youtube_video_id: row.try_get("youtube_video_id").map_err(map_sqlx)?,
                    upload_url: row.try_get("upload_url").map_err(map_sqlx)?,
                    bytes_uploaded: row.try_get("bytes_uploaded").map_err(map_sqlx)?,
                    status: row.try_get("status").map_err(map_sqlx)?,
                    error: row.try_get("error").map_err(map_sqlx)?,
                    started_at: row.try_get("started_at").map_err(map_sqlx)?,
                    completed_at: row.try_get("completed_at").map_err(map_sqlx)?,
                }))
            }
        }
    }
}

// =============================================================================
// LivestreamRotation
// =============================================================================

pub struct LivestreamRotation;

impl LivestreamRotation {
    /// Pick the next track for the 24/7 livestream rotation. Strategy:
    /// among `published` tracks, prefer the one whose most-recent
    /// `livestream_rotation_log` entry is oldest (or absent entirely —
    /// brand-new tracks jump the queue). Returns `None` if no published
    /// track exists yet.
    ///
    /// Caller is expected to immediately log a new
    /// `livestream_rotation_log` row when they start playing — see
    /// [`LivestreamRotation::log_start`]. Without that follow-up, the
    /// same track would be chosen again on the next call.
    #[instrument(skip(db))]
    pub async fn next_track(db: &Db) -> NightdriveResult<Option<TrackRow>> {
        let maybe_row = sqlx::query(
            "SELECT t.id, t.title, t.bpm, t.key, t.seed, t.spec_json, t.state, \
                    t.audio_path, t.cover_path, t.visualizer_path, t.duration_secs, \
                    t.created_at, t.updated_at, t.custom_thumbnail_set, \
                    t.thumbnail_last_attempt_at \
             FROM tracks t \
             LEFT JOIN ( \
                 SELECT track_id, MAX(started_at) AS last_started \
                 FROM livestream_rotation_log GROUP BY track_id \
             ) r ON r.track_id = t.id \
             WHERE t.state = 'published' \
             ORDER BY r.last_started IS NOT NULL, r.last_started ASC, t.created_at ASC \
             LIMIT 1",
        )
        .fetch_optional(&db.pool)
        .await
        .map_err(map_sqlx)?;

        maybe_row.as_ref().map(row_to_track).transpose()
    }

    /// Log a fresh "we started playing this track" row. The supervisor calls
    /// this immediately after [`LivestreamRotation::next_track`] picks the
    /// track and the player starts streaming.
    pub async fn log_start(db: &Db, track_id: &TrackId) -> NightdriveResult<i64> {
        let row = sqlx::query(
            "INSERT INTO livestream_rotation_log (track_id) VALUES (?) RETURNING id",
        )
        .bind(track_id.as_str())
        .fetch_one(&db.pool)
        .await
        .map_err(map_sqlx)?;
        row.try_get("id").map_err(map_sqlx)
    }
}
