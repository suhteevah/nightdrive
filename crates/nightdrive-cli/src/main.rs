//! nightdrive-cli — manual triggers, db operations, YouTube auth, status queries.
//!
//! Subcommands:
//!   db migrate                         — run pending sqlx migrations
//!   youtube auth                       — OAuth Desktop flow to obtain a refresh token
//!   tracks list                        — print recent tracks and their pipeline state
//!   uploads list                       — print upload history
//!   stream status                      — check whether the 24/7 livestream service is running
//!   stems generate                     — run Demucs on a track or album to produce stems
//!   export album                       — bundle FLAC + cover + (optional) stems for Spotify/DistroKid
//!   thumbnails retry-failed            — retry custom thumbnail upload for published tracks where it failed
//!
//! All subcommands that touch the database resolve their SQLite path via
//! `AppConfig` (NIGHTDRIVE_CONFIG env / fallback list). Override the config
//! file with `--config <path>` for one-off runs.

use anyhow::{Context, anyhow};
use clap::{Parser, Subcommand};
use nightdrive_core::{CompositionSpec, TrackPaths, config::AppConfig};
use nightdrive_youtube::{YoutubeUploader, YoutubeClient, YoutubeCredentials};
use nightdrive_stems::{DemucsCli, StemSeparator, StemsConfig};
use nightdrive_storage::{Db, Tracks, Uploads};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "nightdrive-cli", version, about = "nightdrive manual control interface")]
struct Cli {
    /// Path to nightdrive.toml. Overrides NIGHTDRIVE_CONFIG.
    #[arg(long, env = "NIGHTDRIVE_CONFIG")]
    config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Database operations.
    Db {
        #[command(subcommand)]
        action: DbAction,
    },
    /// YouTube account operations.
    Youtube {
        #[command(subcommand)]
        action: YoutubeAction,
    },
    /// Track operations.
    Tracks {
        #[command(subcommand)]
        action: TracksAction,
    },
    /// Upload history.
    Uploads {
        #[command(subcommand)]
        action: UploadsAction,
    },
    /// Livestream operations.
    Stream {
        #[command(subcommand)]
        action: StreamAction,
    },
    /// Stem-separation operations (Demucs htdemucs_ft).
    Stems {
        #[command(subcommand)]
        action: StemsAction,
    },
    /// Export bundles for distribution platforms (Spotify, DistroKid, etc).
    Export {
        #[command(subcommand)]
        action: ExportAction,
    },
    /// Thumbnail maintenance.
    Thumbnails {
        #[command(subcommand)]
        cmd: ThumbnailsCmd,
    },
    /// Album backlog + drop control.
    Album {
        #[command(subcommand)]
        cmd: AlbumCmd,
    },
}

#[derive(Subcommand)]
enum DbAction {
    /// Run pending sqlx migrations against the configured SQLite database.
    Migrate,
}

#[derive(Subcommand)]
enum YoutubeAction {
    /// Open browser for OAuth Desktop flow and print the refresh token.
    Auth,
}

#[derive(Subcommand)]
enum TracksAction {
    /// List recent tracks and their current pipeline state.
    List {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
}

#[derive(Subcommand)]
enum UploadsAction {
    /// List upload history.
    List {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
}

#[derive(Subcommand)]
enum StreamAction {
    /// Print whether the 24/7 livestream service is running.
    Status,
}

#[derive(Subcommand)]
enum StemsAction {
    /// Run Demucs on every track in an album, writing 4-stem WAVs into the
    /// per-track artifact dir under `stems/`.
    Generate {
        /// Album slug (matches `docs/albums/<slug>.json`). Required.
        #[arg(long)]
        album: String,
        /// Optional 1-based track number to limit the run to a single track.
        #[arg(long)]
        track: Option<u32>,
    },
}

#[derive(Debug, Subcommand)]
enum ThumbnailsCmd {
    /// Retry custom thumbnail upload for published tracks where it failed.
    RetryFailed {
        /// Max tracks to attempt this pass (respect per-day YT cap).
        #[arg(long, default_value_t = 80)]
        max: i64,
        /// Don't actually call YT; just print what would be tried.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
enum AlbumCmd {
    /// Backlog management (list, add, approve, nack, remove).
    Backlog {
        #[command(subcommand)]
        cmd: BacklogCmd,
    },
    /// Ask openclaw main to propose N new themes -> backlog.proposed[].
    Propose {
        #[arg(long, default_value_t = 3)]
        count: u32,
    },
    /// Pop next approved slug, run composer + render + upload + schedule. Idempotent.
    DropNext {
        #[arg(long)]
        dry_run: bool,
        /// Compose the head slug's album JSON (if missing) and exit — no pop,
        /// no render, no upload. Run as a pre-eviction step so the cloud-LLM
        /// composer reaches openclaw `main` while inference-embed is still up.
        #[arg(long)]
        compose_only: bool,
    },
    /// Create/sync the album's YouTube playlist: ensure one playlist exists,
    /// add the album's uploaded videos, and put the playlist link in each
    /// video's description (idempotent). Run after uploads.
    PlaylistSync {
        #[arg(long)]
        slug: String,
    },
    /// Upload an album's tracks staggered under the per-day video.insert cap,
    /// syncing the playlist each batch and self-scheduling a +25h continuation
    /// until the album completes. Resumable + idempotent (skips already-uploaded
    /// tracks). This is what `drop-next` calls.
    PublishStaggered {
        #[arg(long)]
        slug: String,
        /// RFC3339 sync-drop timestamp applied to every track's scheduled publish.
        #[arg(long)]
        publish_at: Option<String>,
        /// Max uploads per run/day (GCP project video.insert cap).
        #[arg(long, default_value_t = 6)]
        per_day: u32,
    },
}

#[derive(Debug, Subcommand)]
enum BacklogCmd {
    /// Print proposed / approved / history sections.
    List,
    /// Add a slug to the backlog. Default: proposed[] with 24h promote_at.
    /// Pass --approved to skip the soak and go straight to approved[].
    Add {
        slug: String,
        #[arg(long)]
        theme: String,
        #[arg(long)]
        approved: bool,
        #[arg(long, value_delimiter = ',')]
        danger_zone_keys: Vec<String>,
    },
    /// Move a proposed slug to approved immediately (manual approval).
    Approve { slug: String },
    /// Delete a proposed slug (manual rejection — soak override).
    Nack { slug: String },
    /// Delete a slug from both proposed and approved (does NOT touch history).
    Remove { slug: String },
}

#[derive(Subcommand)]
enum ExportAction {
    /// Build an `exports/<slug>/` bundle: per-track FLAC + cover + (optional)
    /// stems, ready for upload to Spotify / DistroKid / TuneCore.
    Album {
        /// Album slug (matches `docs/albums/<slug>.json`). Required.
        #[arg(long)]
        slug: String,
        /// Output directory. Defaults to `exports/<slug>/`.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Include 4-stem WAVs alongside the master FLAC. Requires that
        /// `nightdrive-cli stems generate --album <slug>` has already run
        /// (or the per-track `stems/` dirs exist from another path).
        #[arg(long, default_value_t = false)]
        include_stems: bool,
    },
}

// =============================================================================
// album propose
// =============================================================================

#[tracing::instrument(skip(cfg))]
async fn album_propose(cfg: &nightdrive_core::config::AppConfig, count: u32) -> anyhow::Result<()> {
    if count == 0 {
        anyhow::bail!("--count must be >= 1");
    }
    let backlog_path = cfg.paths.backlog_json();
    let albums_dir = cfg.paths.albums_dir();
    let existing_slugs = collect_existing_slugs(&albums_dir, &backlog_path);

    let prompt = build_propose_prompt(count, &existing_slugs);
    let gw = nightdrive_openclaw_main::GatewayConfig::from_env()?;
    let reply = nightdrive_openclaw_main::ask_main(&gw, &prompt).await?;
    let proposals: Vec<ProposedFromLlm> = parse_proposals(&reply)?;

    if proposals.is_empty() {
        println!("openclaw main returned 0 proposals");
        return Ok(());
    }

    let now = chrono::Utc::now();
    let mut accepted: Vec<String> = Vec::new();
    nightdrive_core::backlog::mutate(&backlog_path, |bl| {
        for p in &proposals {
            if existing_slugs.contains(&p.slug) {
                tracing::warn!(slug = %p.slug, "propose: openclaw returned existing slug, skipping");
                continue;
            }
            bl.proposed.push(nightdrive_core::backlog::Proposed {
                slug: p.slug.clone(),
                theme: p.theme.clone(),
                proposed_at: now,
                promote_at: now + chrono::Duration::hours(24),
                proposed_by: "openclaw-main".into(),
                danger_zone_keys: p.danger_zone_keys.clone(),
            });
            accepted.push(p.slug.clone());
        }
        Ok(())
    })?;

    println!("proposed: {:?}", accepted);
    println!("(24h soak; NACK any via 'nightdrive-cli album backlog nack <slug>')");
    if !accepted.is_empty() {
        let _ = nightdrive_core::telegram::notify(&format!(
            "nightdrive: {} new themes proposed — 24h soak. NACK any via 'nightdrive-cli album backlog nack <slug>' on cnc. Slugs: {}",
            accepted.len(),
            accepted.join(", ")
        ));
    }
    Ok(())
}

#[derive(serde::Deserialize, Debug)]
struct ProposedFromLlm {
    slug: String,
    theme: String,
    #[serde(default)]
    danger_zone_keys: Vec<String>,
}

fn collect_existing_slugs(
    albums_dir: &std::path::Path,
    backlog_path: &std::path::Path,
) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    if let Ok(rd) = std::fs::read_dir(albums_dir) {
        for e in rd.flatten() {
            if let Some(name) = e.path().file_stem().and_then(|s| s.to_str()) {
                set.insert(name.to_string());
            }
        }
    }
    if let Ok(bl) = nightdrive_core::backlog::load(backlog_path) {
        for a in &bl.approved { set.insert(a.slug.clone()); }
        for p in &bl.proposed { set.insert(p.slug.clone()); }
        for h in &bl.history { set.insert(h.slug.clone()); }
    }
    set
}

fn build_propose_prompt(count: u32, existing_slugs: &std::collections::HashSet<String>) -> String {
    let mut sorted: Vec<&str> = existing_slugs.iter().map(|s| s.as_str()).collect();
    sorted.sort_unstable();
    format!(
        "You are nightdrive's theme curator. Propose {count} new synthwave album themes for a YouTube channel that already has these slugs: {sorted:?}\n\n\
         Each new theme MUST be visually + sonically distinct from existing ones (no near-duplicates).\n\
         Each theme should be evocative + concrete enough that an SDXL cover prompt and a 12-track musical arc can be derived.\n\n\
         Output ONLY a JSON array (no prose, no fence). Each element:\n\
         {{\n  \"slug\": \"<kebab-case>-vol-1\",\n  \"theme\": \"<1-2 sentence vivid description>\",\n  \"danger_zone_keys\": [\"<key>\", ...]   // theme keys from docs/album-danger-zone.json this theme should danger-zone-check against (may be empty)\n}}\n\n\
         Available danger_zone keys: tron, blade_runner, tokyo_cyberpunk, miami_vice, berlin_wall, atompunk, sovetskiy, sunset, neo_tokyo. Use only those keys.\n\
         Return exactly {count} elements as a JSON array (top-level [ ... ])."
    )
}

fn parse_proposals(reply: &str) -> anyhow::Result<Vec<ProposedFromLlm>> {
    let cleaned = reply.trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    serde_json::from_str(cleaned)
        .map_err(|e| anyhow::anyhow!("could not parse openclaw reply as proposals JSON: {e}\nraw reply: {}", &cleaned.chars().take(400).collect::<String>()))
}

// =============================================================================
// thumbnails retry-failed
// =============================================================================

#[tracing::instrument(fields(max, dry_run))]
async fn thumbnails_retry_failed(max: i64, dry_run: bool) -> anyhow::Result<()> {
    let cfg = AppConfig::load().context("load nightdrive.toml")?;
    let db = nightdrive_storage::Db::connect_and_migrate(&cfg.paths.sqlite_db)
        .await
        .context("connect_and_migrate")?;
    let candidates = nightdrive_storage::Tracks::list_published_with_missing_thumbnail(&db, max)
        .await
        .context("list_published_with_missing_thumbnail")?;
    tracing::info!(count = candidates.len(), max, dry_run, "thumbnail-retry: candidates loaded");

    if candidates.is_empty() {
        println!("no failed thumbnails to retry");
        return Ok(());
    }

    if dry_run {
        for (track, video_id) in &candidates {
            println!("DRY-RUN would retry {} (video={})", track.id, video_id);
        }
        return Ok(());
    }

    let creds = nightdrive_youtube::YoutubeCredentials::from_env()?;
    let yt = nightdrive_youtube::YoutubeClient::new(creds)?;
    let mut ok_count: u32 = 0;
    let mut rate_limited = false;

    for (track, video_id) in &candidates {
        let thumb_path = local_thumbnail_path(&cfg, track.id.as_str());
        if !thumb_path.exists() {
            tracing::warn!(
                track_id = %track.id,
                path = %thumb_path.display(),
                "thumbnail-retry: local thumb file missing, skipping"
            );
            continue;
        }

        match yt.set_thumbnail(video_id, &thumb_path).await {
            Ok(_) => {
                nightdrive_storage::Tracks::mark_thumbnail_set(&db, &track.id)
                    .await
                    .with_context(|| format!("mark_thumbnail_set {}", track.id))?;
                ok_count += 1;
                tracing::info!(track_id = %track.id, video_id = %video_id, "thumbnail-retry: set");
                println!("OK {} (video={})", track.id, video_id);
            }
            Err(e) if is_rate_limited_yt(&e) => {
                let _ = nightdrive_storage::Tracks::mark_thumbnail_attempted(&db, &track.id).await;
                tracing::warn!(track_id = %track.id, "thumbnail-retry: 429 — stopping pass");
                rate_limited = true;
                break;
            }
            Err(e) => {
                let _ = nightdrive_storage::Tracks::mark_thumbnail_attempted(&db, &track.id).await;
                tracing::warn!(track_id = %track.id, err = %e, "thumbnail-retry: non-429 error, continuing");
                println!("FAIL {} (video={}): {}", track.id, video_id, e);
            }
        }
    }

    let suffix = if rate_limited { " (rate-limited mid-pass)" } else { "" };
    println!("retried {} thumbnails{}", ok_count, suffix);
    Ok(())
}

/// Prefer `thumbnail.jpg` (final encode thumb) if present, fall back to `cover.png`.
fn local_thumbnail_path(cfg: &AppConfig, track_id: &str) -> std::path::PathBuf {
    // TrackPaths::new constructs  <work_dir>/tracks/<track_id>/
    // which matches the on-disk artifact layout used everywhere else.
    use nightdrive_core::TrackId;
    let tid = TrackId(track_id.to_string());
    let paths = TrackPaths::new(&cfg.paths.work_dir, &tid);
    let jpg = paths.thumbnail_jpg();
    if jpg.exists() { jpg } else { paths.cover_png() }
}

fn is_rate_limited_yt(e: &nightdrive_core::NightdriveError) -> bool {
    let s = format!("{e}");
    s.contains("429") || s.contains("rateLimitExceeded") || s.contains("uploadLimitExceeded")
}

// =============================================================================

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();

    if let Some(p) = &cli.config {
        // SAFETY: single-threaded at this point (right after parse, before
        // tokio spawns any workers). `set_var` is `unsafe` in std as of
        // edition 2024 because env mutations aren't thread-safe; here we're
        // pre-runtime so no other thread can observe.
        unsafe { std::env::set_var("NIGHTDRIVE_CONFIG", p) };
    }

    match cli.command {
        Command::Db { action: DbAction::Migrate } => db_migrate().await,
        Command::Youtube { action: YoutubeAction::Auth } => youtube_auth().await,
        Command::Tracks { action: TracksAction::List { limit } } => tracks_list(limit).await,
        Command::Uploads { action: UploadsAction::List { limit } } => uploads_list(limit).await,
        Command::Stream { action: StreamAction::Status } => stream_status().await,
        Command::Stems { action: StemsAction::Generate { album, track } } => {
            stems_generate(&album, track).await
        }
        Command::Export { action: ExportAction::Album { slug, out, include_stems } } => {
            export_album(&slug, out, include_stems).await
        }
        Command::Thumbnails { cmd } => match cmd {
            ThumbnailsCmd::RetryFailed { max, dry_run } => {
                thumbnails_retry_failed(max, dry_run).await
            }
        },
        Command::Album { cmd } => {
            let cfg = AppConfig::load().context("load nightdrive.toml")?;
            match cmd {
                AlbumCmd::Backlog { cmd } => album_backlog(&cfg, cmd).await,
                AlbumCmd::Propose { count } => album_propose(&cfg, count).await,
                AlbumCmd::DropNext { dry_run, compose_only } => album_drop_next(&cfg, dry_run, compose_only).await,
                AlbumCmd::PlaylistSync { slug } => album_playlist_sync(&cfg, &slug).await,
                AlbumCmd::PublishStaggered { slug, publish_at, per_day } => {
                    let pa = match publish_at {
                        Some(s) => Some(
                            chrono::DateTime::parse_from_rfc3339(&s)
                                .context("parse --publish-at (RFC3339)")?
                                .with_timezone(&chrono::Utc),
                        ),
                        None => None,
                    };
                    album_publish_staggered(&cfg, &slug, pa, per_day).await
                }
            }
        }
    }
}

// =============================================================================
// db migrate
// =============================================================================

async fn db_migrate() -> anyhow::Result<()> {
    let cfg = AppConfig::load().context("load nightdrive.toml")?;
    // Make sure the parent directory exists — operators new to nightdrive
    // commonly haven't created /var/lib/nightdrive yet and the migration's
    // ?mode=rwc only covers file creation, not parent dirs.
    if let Some(parent) = cfg.paths.sqlite_db.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create sqlite parent dir {}", parent.display()))?;
        }
    }
    println!("migrating {}", cfg.paths.sqlite_db.display());
    let _db = Db::connect_and_migrate(&cfg.paths.sqlite_db)
        .await
        .context("connect_and_migrate")?;
    println!("OK");
    Ok(())
}

// =============================================================================
// youtube auth
// =============================================================================

async fn youtube_auth() -> anyhow::Result<()> {
    let client_id = std::env::var("NIGHTDRIVE_YT_CLIENT_ID").map_err(|_| {
        anyhow!(
            "NIGHTDRIVE_YT_CLIENT_ID not set. Put it in .env after creating the \
             OAuth Desktop client in Google Cloud Console (see .env.example)."
        )
    })?;
    let client_secret = std::env::var("NIGHTDRIVE_YT_CLIENT_SECRET").map_err(|_| {
        anyhow!("NIGHTDRIVE_YT_CLIENT_SECRET not set. Put it in .env (see .env.example).")
    })?;

    let refresh =
        nightdrive_youtube::bootstrap_refresh_token(&client_id, &client_secret).await?;

    println!("\n\nRefresh token (paste into .env as NIGHTDRIVE_YT_REFRESH_TOKEN):\n");
    println!("    {refresh}\n");
    Ok(())
}

// =============================================================================
// tracks list
// =============================================================================

async fn tracks_list(limit: u32) -> anyhow::Result<()> {
    let cfg = AppConfig::load().context("load nightdrive.toml")?;
    let db = Db::connect_and_migrate(&cfg.paths.sqlite_db)
        .await
        .context("connect_and_migrate")?;
    let mut rows = Tracks::list(&db, None).await.context("list tracks")?;
    // Tracks::list returns ASC by created_at; take the last `limit` for "most recent."
    if rows.len() > limit as usize {
        rows = rows.split_off(rows.len() - limit as usize);
    }
    rows.reverse();

    if rows.is_empty() {
        println!("(no tracks yet)");
        return Ok(());
    }

    // Tab-separated for easy piping into awk / cut / less.
    println!("ID\tBPM\tKEY\tSTATE\tCREATED\tTITLE");
    for r in rows {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            r.id,
            r.bpm,
            r.key,
            r.state.as_str(),
            r.created_at,
            r.title,
        );
    }
    Ok(())
}

// =============================================================================
// uploads list
// =============================================================================

async fn uploads_list(limit: u32) -> anyhow::Result<()> {
    let cfg = AppConfig::load().context("load nightdrive.toml")?;
    let db = Db::connect_and_migrate(&cfg.paths.sqlite_db)
        .await
        .context("connect_and_migrate")?;
    let rows = Uploads::list_recent(&db, limit)
        .await
        .context("list uploads")?;

    if rows.is_empty() {
        println!("(no uploads yet)");
        return Ok(());
    }

    println!("UPLOAD_ID\tTRACK_ID\tYOUTUBE_ID\tSTATUS\tSTARTED\tCOMPLETED");
    for r in rows {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            r.id,
            r.track_id,
            r.youtube_video_id.as_deref().unwrap_or("-"),
            r.status,
            r.started_at,
            r.completed_at.as_deref().unwrap_or("-"),
        );
    }
    Ok(())
}

// =============================================================================
// stream status
// =============================================================================

// =============================================================================
// stems generate
// =============================================================================
//
// Album JSON shape (subset we read here). The album-composer subagent writes
// `docs/albums/<slug>.json` with this surface; the orchestrator's `run-album`
// consumes it too. We deliberately deserialize a minimal subset so a missing
// optional field in an older album JSON doesn't break stems/export.

#[derive(Debug, Deserialize)]
struct AlbumJson {
    album_slug: String,
    title: String,
    tracks: Vec<AlbumTrackJson>,
}

#[derive(Debug, Deserialize)]
struct AlbumTrackJson {
    track_number: u32,
    title: String,
}

async fn stems_generate(slug: &str, track_filter: Option<u32>) -> anyhow::Result<()> {
    let cfg = AppConfig::load().context("load nightdrive.toml")?;
    let album = load_album_json(slug).await?;

    // Build title→track_root map by scanning every spec.json under tracks_dir.
    // This is O(N tracks on disk) but happens once per album export — fine.
    let title_index = build_title_index(&cfg.paths.tracks_dir).await?;
    println!(
        "indexed {} on-disk tracks under {}",
        title_index.len(),
        cfg.paths.tracks_dir.display()
    );

    // Default Demucs config. Operators can override via env vars on the
    // spawned subprocess (DEMUCS_DEVICE, DEMUCS_MODEL).
    let stems_cfg = StemsConfig {
        device: std::env::var("DEMUCS_DEVICE").unwrap_or_else(|_| "cuda".to_string()),
        model: std::env::var("DEMUCS_MODEL").unwrap_or_else(|_| "htdemucs_ft".to_string()),
        ..StemsConfig::default()
    };
    let demucs = DemucsCli::new(stems_cfg);

    let mut ran = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for track in &album.tracks {
        if let Some(filter) = track_filter {
            if track.track_number != filter {
                continue;
            }
        }
        let Some(track_root) = title_index.get(&track.title) else {
            println!(
                "WARN track {} '{}' has no on-disk artifacts (no spec.json with matching title); skipping",
                track.track_number, track.title,
            );
            skipped += 1;
            continue;
        };
        let track_id = track_root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        let paths = TrackPaths {
            root: track_root.clone(),
        };
        if !paths.master_flac().exists() {
            println!(
                "WARN track {} '{}' ({}) has no master.flac yet; skipping",
                track.track_number, track.title, track_id,
            );
            skipped += 1;
            continue;
        }
        let stems_root = paths.root.join("stems").join("drums.wav");
        if stems_root.exists() {
            println!(
                "OK track {} '{}' ({}) already has stems; skipping",
                track.track_number, track.title, track_id,
            );
            skipped += 1;
            continue;
        }

        println!(
            "running demucs on track {} '{}' ({})...",
            track.track_number, track.title, track_id,
        );
        match demucs.separate(&paths).await {
            Ok(_) => {
                println!("OK track {} stems written", track.track_number);
                ran += 1;
            }
            Err(e) => {
                println!("FAIL track {} '{}': {}", track.track_number, track.title, e);
                failed += 1;
            }
        }
    }

    println!(
        "stems generate done · album={} · ran={} skipped={} failed={}",
        album.album_slug, ran, skipped, failed,
    );
    if failed > 0 {
        return Err(anyhow!("{failed} track(s) failed stem separation"));
    }
    Ok(())
}

// =============================================================================
// export album
// =============================================================================

async fn export_album(
    slug: &str,
    out_override: Option<PathBuf>,
    include_stems: bool,
) -> anyhow::Result<()> {
    let cfg = AppConfig::load().context("load nightdrive.toml")?;
    let album = load_album_json(slug).await?;

    let out_root = out_override
        .unwrap_or_else(|| PathBuf::from("exports").join(slug));
    tokio::fs::create_dir_all(&out_root)
        .await
        .with_context(|| format!("create out dir {}", out_root.display()))?;
    tokio::fs::create_dir_all(out_root.join("covers"))
        .await
        .context("create covers/ dir")?;
    if include_stems {
        tokio::fs::create_dir_all(out_root.join("stems"))
            .await
            .context("create stems/ dir")?;
    }

    let title_index = build_title_index(&cfg.paths.tracks_dir).await?;

    let mut copied = 0usize;
    let mut warned = 0usize;

    for track in &album.tracks {
        let Some(track_root) = title_index.get(&track.title) else {
            println!(
                "WARN track {} '{}' missing from disk (no spec.json title match); skipping",
                track.track_number, track.title,
            );
            warned += 1;
            continue;
        };
        let paths = TrackPaths {
            root: track_root.clone(),
        };

        let safe_title = sanitize_filename(&track.title);
        let prefix = format!("{:02} - {}", track.track_number, safe_title);

        // master.flac → exports/<slug>/<NN - Title>.flac
        if !paths.master_flac().exists() {
            println!(
                "WARN track {} '{}' has no master.flac at {}; skipping",
                track.track_number, track.title, paths.master_flac().display(),
            );
            warned += 1;
            continue;
        }
        let flac_dst = out_root.join(format!("{prefix}.flac"));
        copy_file(&paths.master_flac(), &flac_dst).await?;

        // cover.png → exports/<slug>/covers/<NN - Title>.png
        if paths.cover_png().exists() {
            let cover_dst = out_root.join("covers").join(format!("{prefix}.png"));
            copy_file(&paths.cover_png(), &cover_dst).await?;
        }

        if include_stems {
            let stems_src = paths.root.join("stems");
            if !stems_src.exists() {
                println!(
                    "WARN track {} '{}' include_stems requested but {} missing — run `stems generate --album {}` first",
                    track.track_number, track.title, stems_src.display(), slug,
                );
                warned += 1;
            } else {
                let stems_dst = out_root.join("stems").join(&prefix);
                tokio::fs::create_dir_all(&stems_dst)
                    .await
                    .with_context(|| format!("mkdir {}", stems_dst.display()))?;
                for stem in ["drums.wav", "bass.wav", "vocals.wav", "other.wav"] {
                    let src = stems_src.join(stem);
                    if src.exists() {
                        copy_file(&src, &stems_dst.join(stem)).await?;
                    }
                }
            }
        }

        copied += 1;
        println!("OK track {} '{}' copied", track.track_number, track.title);
    }

    // Drop a README into the export dir documenting what's there.
    let readme = format!(
        "{}\n{}\n\nBundle layout:\n\
         - <NN> - <Title>.flac         24-bit FLAC masters (Spotify / DistroKid / TuneCore upload)\n\
         - covers/<NN> - <Title>.png   1024² album cover per track\n{}\
         \nGenerated by nightdrive-cli export album --slug {} on {}.\n",
        album.title,
        "=".repeat(album.title.chars().count()),
        if include_stems {
            "- stems/<NN - Title>/{drums,bass,vocals,other}.wav  4-stem split via Demucs htdemucs_ft\n"
        } else { "" },
        slug,
        chrono::Utc::now().format("%Y-%m-%d %H:%M UTC"),
    );
    tokio::fs::write(out_root.join("README.txt"), readme)
        .await
        .context("write README.txt")?;

    println!(
        "export done · album={} · out={} · copied={} warnings={}",
        album.album_slug,
        out_root.display(),
        copied,
        warned,
    );
    Ok(())
}

// =============================================================================
// Shared helpers for the stems/export subcommands
// =============================================================================

async fn load_album_json(slug: &str) -> anyhow::Result<AlbumJson> {
    let path = PathBuf::from("docs").join("albums").join(format!("{slug}.json"));
    let text = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("read album json at {}", path.display()))?;
    let album: AlbumJson = serde_json::from_str(&text)
        .with_context(|| format!("parse album json at {}", path.display()))?;
    if album.album_slug != slug {
        return Err(anyhow!(
            "album_slug {:?} in {} does not match requested slug {:?}",
            album.album_slug, path.display(), slug,
        ));
    }
    Ok(album)
}

async fn build_title_index(
    tracks_dir: &Path,
) -> anyhow::Result<std::collections::HashMap<String, PathBuf>> {
    let mut idx = std::collections::HashMap::new();
    if !tracks_dir.exists() {
        return Ok(idx);
    }
    let mut entries = tokio::fs::read_dir(tracks_dir)
        .await
        .with_context(|| format!("read_dir {}", tracks_dir.display()))?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let spec_path = path.join("spec.json");
        if !spec_path.exists() {
            continue;
        }
        let Ok(text) = tokio::fs::read_to_string(&spec_path).await else { continue };
        let Ok(spec) = serde_json::from_str::<CompositionSpec>(&text) else { continue };
        // Latest-wins: a re-rendered track overwrites the prior index entry
        // since spec.json is rewritten on each run. Doesn't matter for our
        // purpose because both should share the same artifact directory.
        idx.insert(spec.title, path);
    }
    Ok(idx)
}

async fn copy_file(src: &Path, dst: &Path) -> anyhow::Result<()> {
    if let Some(parent) = dst.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    tokio::fs::copy(src, dst)
        .await
        .with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
    Ok(())
}

/// Strip filesystem-unfriendly characters from a track title so we can use it
/// in the exports/<slug>/ tree. Keeps spaces (DistroKid is fine with them) and
/// replaces `/`, `\`, `:`, `*`, `?`, `"`, `<`, `>`, `|` with `-`.
fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            c => c,
        })
        .collect::<String>()
        .trim()
        .trim_end_matches('.')
        .to_string()
}

// =============================================================================
// album backlog
// =============================================================================

#[tracing::instrument(skip(cfg))]
async fn album_backlog(cfg: &AppConfig, cmd: BacklogCmd) -> anyhow::Result<()> {
    let path = cfg.paths.backlog_json();
    match cmd {
        BacklogCmd::List => {
            let bl = nightdrive_core::backlog::load(&path).unwrap_or_else(|_| {
                nightdrive_core::backlog::Backlog {
                    version: 1,
                    youtube_strikes: 0,
                    proposed: vec![],
                    approved: vec![],
                    history: vec![],
                }
            });
            println!("=== proposed ({}) ===", bl.proposed.len());
            for p in &bl.proposed {
                println!("  {} (promotes at {}) -- {}", p.slug, p.promote_at, p.theme);
            }
            println!("=== approved ({}) ===", bl.approved.len());
            for a in &bl.approved {
                println!("  {} -- {}", a.slug, a.theme);
            }
            println!("=== history ({}) ===", bl.history.len());
            for h in bl.history.iter().rev().take(10) {
                println!("  {} (dropped {})", h.slug, h.dropped_at);
            }
            Ok(())
        }
        BacklogCmd::Add { slug, theme, approved, danger_zone_keys } => {
            // Validate no duplicate across all three sections before mutating.
            {
                let bl = nightdrive_core::backlog::load(&path).unwrap_or_else(|_| {
                    nightdrive_core::backlog::Backlog {
                        version: 1,
                        youtube_strikes: 0,
                        proposed: vec![],
                        approved: vec![],
                        history: vec![],
                    }
                });
                if bl.approved.iter().any(|a| a.slug == slug) {
                    anyhow::bail!("slug already exists in approved[]: {}", slug);
                }
                if bl.proposed.iter().any(|p| p.slug == slug) {
                    anyhow::bail!("slug already exists in proposed[]: {}", slug);
                }
                if bl.history.iter().any(|h| h.slug == slug) {
                    anyhow::bail!("slug already exists in history[]: {}", slug);
                }
            }
            nightdrive_core::backlog::mutate(&path, |bl| {
                if approved {
                    bl.approved.push(nightdrive_core::backlog::Approved {
                        slug: slug.clone(),
                        theme: theme.clone(),
                        approved_at: chrono::Utc::now(),
                        danger_zone_keys: danger_zone_keys.clone(),
                    });
                } else {
                    let now = chrono::Utc::now();
                    bl.proposed.push(nightdrive_core::backlog::Proposed {
                        slug: slug.clone(),
                        theme: theme.clone(),
                        proposed_at: now,
                        promote_at: now + chrono::Duration::hours(24),
                        proposed_by: "manual".into(),
                        danger_zone_keys: danger_zone_keys.clone(),
                    });
                }
                Ok(())
            })?;
            println!("added: {}", slug);
            Ok(())
        }
        BacklogCmd::Approve { slug } => {
            let mut found = false;
            nightdrive_core::backlog::mutate(&path, |bl| {
                if let Some(idx) = bl.proposed.iter().position(|p| p.slug == slug) {
                    let p = bl.proposed.remove(idx);
                    bl.approved.push(nightdrive_core::backlog::Approved {
                        slug: p.slug,
                        theme: p.theme,
                        approved_at: chrono::Utc::now(),
                        danger_zone_keys: p.danger_zone_keys,
                    });
                    found = true;
                }
                Ok(())
            })?;
            if found {
                println!("approved: {}", slug);
            } else {
                println!("not found in proposed: {}", slug);
            }
            Ok(())
        }
        BacklogCmd::Nack { slug } => {
            let mut found = false;
            nightdrive_core::backlog::mutate(&path, |bl| {
                let before = bl.proposed.len();
                bl.proposed.retain(|p| p.slug != slug);
                found = bl.proposed.len() != before;
                Ok(())
            })?;
            if found {
                println!("nack'd: {}", slug);
            } else {
                println!("not found in proposed: {}", slug);
            }
            Ok(())
        }
        BacklogCmd::Remove { slug } => {
            let mut found_proposed = 0usize;
            let mut found_approved = 0usize;
            nightdrive_core::backlog::mutate(&path, |bl| {
                let bp = bl.proposed.len();
                bl.proposed.retain(|p| p.slug != slug);
                found_proposed = bp - bl.proposed.len();
                let ba = bl.approved.len();
                bl.approved.retain(|a| a.slug != slug);
                found_approved = ba - bl.approved.len();
                Ok(())
            })?;
            println!(
                "removed: {} (proposed: {}, approved: {})",
                slug, found_proposed, found_approved
            );
            Ok(())
        }
    }
}

// =============================================================================
// album drop-next
// =============================================================================

#[tracing::instrument(skip(cfg))]
async fn album_drop_next(cfg: &nightdrive_core::config::AppConfig, dry_run: bool, compose_only: bool) -> anyhow::Result<()> {
    let backlog_path = cfg.paths.backlog_json();
    let albums_dir = cfg.paths.albums_dir();
    let danger_zone_path = cfg.paths.danger_zone_json();
    let now = chrono::Utc::now();

    // 1. Channel-health gate.
    {
        let bl = nightdrive_core::backlog::load(&backlog_path)?;
        if bl.youtube_strikes > 0 {
            let msg = format!(
                "nightdrive: drop-next refused (youtube_strikes={}). Reset via backlog edit.",
                bl.youtube_strikes
            );
            tracing::warn!("{}", msg);
            println!("{}", msg);
            let _ = nightdrive_core::telegram::notify(&msg);
            return Ok(());
        }
    }

    // 2. Promote expired proposals.
    let bl = nightdrive_core::backlog::mutate(&backlog_path, |bl| {
        let promoted = nightdrive_core::backlog::promote_expired(bl, now);
        if !promoted.is_empty() {
            tracing::info!(?promoted, "drop-next: auto-promoted expired proposals");
        }
        Ok(())
    })?;

    // 3 + 4. Refuse if no approved.
    if bl.approved.is_empty() {
        println!("backlog empty — run `nightdrive-cli album propose` or add slugs manually");
        return Ok(());
    }

    // Peek head (no mutation yet — dry-run must not consume).
    let (head_slug, head_theme, head_dz_keys) = {
        let head = nightdrive_core::backlog::peek_approved(&bl).expect("non-empty checked above");
        (head_slug_clone(&head.slug), head.theme.clone(), head.danger_zone_keys.clone())
    };

    // Compose-only: ensure the head slug's album JSON exists, then exit — no pop,
    // no render, no upload, no history. This runs as a pre-eviction ExecStartPre
    // so the cloud-LLM composer reaches openclaw `main` while inference-embed
    // (which `main` uses for memory) is still up. The subsequent full drop-next
    // then finds the JSON present (see the `album_json_path.exists()` skip below)
    // and runs only the GPU-bound render + upload after eviction. Without this,
    // eviction stops embed first and `main` hangs → 180s podman-exec timeout
    // (the cause of the 2026-05-25/28 silent autonomous-drop failures).
    if compose_only {
        let album_json_path = albums_dir.join(format!("{head_slug}.json"));
        if album_json_path.exists() {
            println!("compose-only: {head_slug} album JSON already present — nothing to do");
            return Ok(());
        }
        let gw = nightdrive_openclaw_main::GatewayConfig::from_env()?;
        let req = nightdrive_album_composer::ComposeRequest {
            slug: head_slug.clone(),
            theme: head_theme.clone(),
            track_count: 12,
            danger_zone_keys: head_dz_keys.clone(),
            albums_dir: albums_dir.clone(),
            danger_zone_path: danger_zone_path.clone(),
            max_retries: 3,
        };
        let spec = nightdrive_album_composer::compose(&gw, &req)
            .await
            .map_err(|e| anyhow::anyhow!("compose-only failed for {head_slug}: {e}"))?;
        std::fs::write(&album_json_path, serde_json::to_string_pretty(&spec)?)?;
        println!("compose-only: composed + wrote {}", album_json_path.display());
        return Ok(());
    }

    // 5. Compute publish_at = (now + 3 days) at 00:00 UTC.
    let publish_at = (now + chrono::Duration::days(3))
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight UTC always valid")
        .and_utc();

    // 6. Dry-run exit BEFORE consuming.
    if dry_run {
        println!(
            "DRY-RUN drop-next: slug={} publish_at={} theme={:?}",
            head_slug,
            publish_at.to_rfc3339(),
            head_theme
        );
        return Ok(());
    }

    // 7. POP head — commit.
    nightdrive_core::backlog::mutate(&backlog_path, |bl| {
        // Defensive: verify slug still at head before popping (concurrent mutate guard).
        if let Some(h) = bl.approved.first() {
            if h.slug == head_slug {
                bl.approved.remove(0);
            }
        }
        Ok(())
    })?;
    tracing::info!(slug = %head_slug, "drop-next: popped head, beginning compose+render");
    let _ = nightdrive_core::telegram::notify(&format!(
        "nightdrive: dropping {}. ETA ~3h render + 2-day upload window. Sync-drop {}.",
        head_slug, publish_at.to_rfc3339()
    ));

    // 8. Compose if album JSON doesn't yet exist.
    let album_json_path = albums_dir.join(format!("{}.json", head_slug));
    if !album_json_path.exists() {
        let gw = nightdrive_openclaw_main::GatewayConfig::from_env()?;
        let req = nightdrive_album_composer::ComposeRequest {
            slug: head_slug.clone(),
            theme: head_theme.clone(),
            track_count: 12,
            danger_zone_keys: head_dz_keys.clone(),
            albums_dir: albums_dir.clone(),
            danger_zone_path: danger_zone_path.clone(),
            max_retries: 3,
        };
        match nightdrive_album_composer::compose(&gw, &req).await {
            Ok(spec) => {
                std::fs::write(&album_json_path, serde_json::to_string_pretty(&spec)?)?;
                tracing::info!(slug = %head_slug, "drop-next: composed + wrote album JSON");
            }
            Err(e) => {
                tracing::warn!(
                    slug = %head_slug,
                    error = %e,
                    "drop-next: composer failed, restoring slug to head of approved"
                );
                nightdrive_core::backlog::mutate(&backlog_path, |bl| {
                    bl.approved.insert(
                        0,
                        nightdrive_core::backlog::Approved {
                            slug: head_slug.clone(),
                            theme: head_theme.clone(),
                            approved_at: now,
                            danger_zone_keys: head_dz_keys.clone(),
                        },
                    );
                    Ok(())
                })?;
                return Err(anyhow::anyhow!("composer failed for {}: {}", head_slug, e));
            }
        }
    } else {
        tracing::info!(slug = %head_slug, "drop-next: album JSON already exists, skipping composer");
    }

    // 9. Upload staggered under the GCP per-day video.insert cap. This does the
    //    first batch now (≤ max_uploads_per_day tracks), syncs the album playlist
    //    + descriptions, and self-schedules +25h continuations until the album
    //    completes. Replaces the old single run-album call that 429-ed after 6.
    tracing::info!(
        slug = %head_slug,
        publish_at = %publish_at.to_rfc3339(),
        per_day = cfg.youtube.max_uploads_per_day,
        "drop-next: invoking staggered publish"
    );
    if let Err(e) =
        album_publish_staggered(cfg, &head_slug, Some(publish_at), cfg.youtube.max_uploads_per_day).await
    {
        tracing::warn!(error = %e, "drop-next: publish-staggered first batch failed");
        return Err(anyhow::anyhow!("publish-staggered failed for {}: {}", head_slug, e));
    }

    // 10. Append to history.
    nightdrive_core::backlog::mutate(&backlog_path, |bl| {
        bl.history.push(nightdrive_core::backlog::HistoryEntry {
            slug: head_slug.clone(),
            dropped_at: now,
        });
        Ok(())
    })?;
    tracing::info!(slug = %head_slug, publish_at = %publish_at.to_rfc3339(), "drop-next: complete");
    let _ = nightdrive_core::telegram::notify(&format!(
        "nightdrive: {} 12/12 done — sync-drop {} armed.",
        head_slug, publish_at.to_rfc3339()
    ));
    println!("drop complete: {} (publish_at {})", head_slug, publish_at.to_rfc3339());
    Ok(())
}

/// Trivial alias — makes the peek-then-pop pattern self-documenting.
fn head_slug_clone(s: &str) -> String {
    s.to_string()
}

/// Set of track numbers in `slug` that already have a completed YouTube upload
/// (a uploads row with a non-null youtube_video_id). Used to resume staggered
/// uploads + to know what's already in the channel without re-uploading.
async fn album_uploaded_nums(
    db: &Db,
    slug: &str,
) -> anyhow::Result<std::collections::HashSet<u32>> {
    let rows = Uploads::list_recent(db, 5000).await.unwrap_or_default();
    let prefix = format!("nd-{slug}-");
    let mut set = std::collections::HashSet::new();
    for r in rows {
        if r.youtube_video_id.is_some() && r.track_id.0.starts_with(&prefix) {
            if let Some(n) = r.track_id.0.rsplit('-').next().and_then(|s| s.parse::<u32>().ok()) {
                set.insert(n);
            }
        }
    }
    Ok(set)
}

/// Ensure the album's YouTube playlist exists, contains every uploaded video,
/// and that each video's description carries the playlist link. Idempotent +
/// incremental — safe to call after every staggered batch and on already-shipped
/// albums (backfill). The API can't pin comments, so the link lives in the
/// description (see `nightdrive_youtube::ensure_playlist_link_in_description`).
async fn album_playlist_sync(cfg: &nightdrive_core::config::AppConfig, slug: &str) -> anyhow::Result<()> {
    let album_path = cfg.paths.albums_dir().join(format!("{slug}.json"));
    let album: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&album_path)
            .with_context(|| format!("read {}", album_path.display()))?,
    )
    .with_context(|| format!("parse {}", album_path.display()))?;
    let album_title = album["title"].as_str().unwrap_or(slug).to_string();

    let db = Db::connect_and_migrate(&cfg.paths.sqlite_db).await.context("open db")?;
    let rows = Uploads::list_recent(&db, 5000).await.unwrap_or_default();
    let prefix = format!("nd-{slug}-");
    // track_num -> video_id. list_recent is DESC by started_at, so the first
    // row seen per track is the most recent completed upload.
    let mut by_num: std::collections::BTreeMap<u32, String> = std::collections::BTreeMap::new();
    for r in rows {
        if let Some(vid) = r.youtube_video_id {
            if r.track_id.0.starts_with(&prefix) {
                if let Some(n) = r.track_id.0.rsplit('-').next().and_then(|s| s.parse::<u32>().ok()) {
                    by_num.entry(n).or_insert(vid);
                }
            }
        }
    }
    if by_num.is_empty() {
        println!("{slug}: no uploaded videos yet; nothing to sync.");
        return Ok(());
    }

    let yt = YoutubeClient::new(YoutubeCredentials::from_env()?)?;
    let pl_desc = format!("{album_title} — full album, in order. nightdrive autonomous synthwave for coding.");
    let playlist_id = yt.ensure_playlist(&album_title, &pl_desc, "public").await?;
    let url = YoutubeClient::playlist_url(&playlist_id);
    let existing: std::collections::HashSet<String> = yt
        .list_playlist_video_ids(&playlist_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .collect();

    let (mut added, mut fixed) = (0u32, 0u32);
    for (num, vid) in &by_num {
        if !existing.contains(vid) {
            match yt.add_video_to_playlist(&playlist_id, vid).await {
                Ok(()) => added += 1,
                Err(e) => tracing::warn!(track = *num, video_id = %vid, error = %e, "playlist add failed"),
            }
        }
        match yt.ensure_playlist_link_in_description(vid, &url).await {
            Ok(true) => fixed += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(track = *num, video_id = %vid, error = %e, "description link update failed"),
        }
    }
    println!(
        "{slug}: playlist \"{album_title}\" {url} — {} videos ({} newly added, {} descriptions updated).",
        by_num.len(),
        added,
        fixed
    );
    Ok(())
}

/// Upload `slug`'s tracks in batches of `per_day`, syncing the playlist after
/// each batch and self-scheduling a +25h continuation until every track is up.
/// Resumable + idempotent: skips already-uploaded tracks, so continuations (and
/// accidental re-runs) resume rather than duplicate.
async fn album_publish_staggered(
    cfg: &nightdrive_core::config::AppConfig,
    slug: &str,
    publish_at: Option<chrono::DateTime<chrono::Utc>>,
    per_day: u32,
) -> anyhow::Result<()> {
    let per_day = per_day.max(1);
    let album_path = cfg.paths.albums_dir().join(format!("{slug}.json"));
    let album: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&album_path)
            .with_context(|| format!("read {}", album_path.display()))?,
    )
    .with_context(|| format!("parse {}", album_path.display()))?;
    let track_count = album["tracks"].as_array().map(|a| a.len()).unwrap_or(0) as u32;
    if track_count == 0 {
        anyhow::bail!("album {slug} has no tracks");
    }

    let db = Db::connect_and_migrate(&cfg.paths.sqlite_db).await.context("open db")?;
    let uploaded_before = album_uploaded_nums(&db, slug).await?;
    let todo: Vec<u32> = (1..=track_count).filter(|n| !uploaded_before.contains(n)).collect();

    if todo.is_empty() {
        album_playlist_sync(cfg, slug).await?;
        let msg = format!("nightdrive: {slug} already fully uploaded ({track_count}/{track_count}); playlist synced.");
        tracing::info!("{msg}");
        let _ = nightdrive_core::telegram::notify(&msg);
        println!("{msg}");
        return Ok(());
    }

    let batch: Vec<u32> = todo.iter().copied().take(per_day as usize).collect();
    let publish_iso = publish_at.map(|p| p.to_rfc3339());
    tracing::info!(slug, ?batch, todo = todo.len(), "publish-staggered: uploading batch");

    let orch = std::env::var("NIGHTDRIVE_ORCHESTRATOR_BIN")
        .unwrap_or_else(|_| "/opt/nightdrive/bin/nightdrive-orchestrator".to_string());
    for num in &batch {
        let mut args: Vec<String> = vec![
            "run-album".into(),
            "--slug".into(),
            slug.into(),
            "--from-track".into(),
            num.to_string(),
            "--to-track".into(),
            num.to_string(),
        ];
        if let Some(iso) = &publish_iso {
            args.push("--publish-at".into());
            args.push(iso.clone());
        }
        tracing::info!(track = *num, "publish-staggered: run-album single track");
        match std::process::Command::new(&orch).args(&args).status() {
            Ok(s) if s.success() => {}
            Ok(s) => tracing::warn!(track = *num, code = ?s.code(), "run-album track non-zero (continuing)"),
            Err(e) => tracing::warn!(track = *num, error = %e, "run-album spawn failed (continuing)"),
        }
    }

    // Playlist + descriptions (incremental, idempotent, non-fatal).
    if let Err(e) = album_playlist_sync(cfg, slug).await {
        tracing::warn!(error = %e, "publish-staggered: playlist sync failed (non-fatal)");
    }

    let uploaded_after = album_uploaded_nums(&db, slug).await?;
    let remaining: Vec<u32> = (1..=track_count).filter(|n| !uploaded_after.contains(n)).collect();
    let progressed = uploaded_after.len() > uploaded_before.len();

    if remaining.is_empty() {
        let msg = format!(
            "nightdrive: {slug} COMPLETE — {track_count}/{track_count} uploaded + playlist synced. Sync-drop {}.",
            publish_iso.as_deref().unwrap_or("(per-track default)")
        );
        tracing::info!("{msg}");
        let _ = nightdrive_core::telegram::notify(&msg);
        println!("{msg}");
    } else if progressed {
        schedule_stagger_continuation(slug, publish_at, per_day, remaining.len())?;
    } else {
        let msg = format!(
            "nightdrive: {slug} STALLED — {} tracks un-uploaded after a no-progress batch. Not rescheduling; needs a look.",
            remaining.len()
        );
        tracing::warn!("{msg}");
        let _ = nightdrive_core::telegram::notify(&msg);
        println!("{msg}");
    }
    Ok(())
}

/// Arm a durable +25h systemd timer that re-invokes `album publish-staggered`
/// for the next batch (next Pacific day — clears both the rolling-24h channel
/// cap and the per-day GCP project cap, since 25h always crosses one midnight).
/// Best-effort; on failure prints the manual fallback command.
fn schedule_stagger_continuation(
    slug: &str,
    publish_at: Option<chrono::DateTime<chrono::Utc>>,
    per_day: u32,
    remaining: usize,
) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    let cwd = std::env::current_dir().context("current_dir")?;
    let unit = format!("nightdrive-stagger-{slug}");
    let _ = std::process::Command::new("systemctl")
        .args(["reset-failed", &format!("{unit}.timer"), &format!("{unit}.service")])
        .status();

    let mut args: Vec<String> = vec![
        "--on-active=25h".into(),
        format!("--unit={unit}"),
        "--collect".into(),
        format!("--property=WorkingDirectory={}", cwd.display()),
        "--property=EnvironmentFile=/etc/nightdrive/nightdrive.env".into(),
        exe.display().to_string(),
        "album".into(),
        "publish-staggered".into(),
        "--slug".into(),
        slug.into(),
        "--per-day".into(),
        per_day.to_string(),
    ];
    if let Some(p) = publish_at {
        args.push("--publish-at".into());
        args.push(p.to_rfc3339());
    }

    let manual = format!(
        "nightdrive-cli album publish-staggered --slug {slug} --per-day {per_day}{}",
        publish_at.map(|p| format!(" --publish-at {}", p.to_rfc3339())).unwrap_or_default()
    );

    match std::process::Command::new("systemd-run").args(&args).status() {
        Ok(s) if s.success() => {
            let msg = format!("nightdrive: {slug} batch done; {remaining} tracks armed for +25h (next GCP day) via {unit}.");
            tracing::info!("{msg}");
            let _ = nightdrive_core::telegram::notify(&msg);
            println!("scheduled +25h continuation for {remaining} remaining tracks ({unit}).");
        }
        other => {
            let msg = format!(
                "nightdrive: {slug} — FAILED to arm continuation ({other:?}). {remaining} tracks pending. Manual: {manual}"
            );
            tracing::error!("{msg}");
            let _ = nightdrive_core::telegram::notify(&msg);
            println!("{msg}");
        }
    }
    Ok(())
}

async fn stream_status() -> anyhow::Result<()> {
    // The livestream supervisor is a systemd service on the linux orchestrator
    // host (arch-controller). On Windows-side dev we can't query it. We print
    // platform-not-supported rather than erroring so a CI / lint pass over the
    // cli on a Windows dev box doesn't fail spuriously.
    #[cfg(unix)]
    {
        let output = tokio::process::Command::new("systemctl")
            .args(["is-active", "nightdrive-livestream.service"])
            .output()
            .await
            .context("spawn systemctl")?;
        let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
        // systemctl exits non-zero when status != active; we still report state
        // because "inactive" and "failed" are both useful for the operator.
        println!("nightdrive-livestream.service: {state}");
        if state != "active" {
            std::process::exit(3);
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        println!(
            "nightdrive-livestream.service: not-supported-on-this-platform \
             (production host is arch-controller / linux)"
        );
        Ok(())
    }
}
