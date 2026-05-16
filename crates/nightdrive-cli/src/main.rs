//! nightdrive-cli — manual triggers, db operations, YouTube auth, status queries.
//!
//! Subcommands:
//!   db migrate            — run pending sqlx migrations
//!   youtube auth          — OAuth Desktop flow to obtain a refresh token
//!   tracks list           — print recent tracks and their pipeline state
//!   uploads list          — print upload history
//!   stream status         — check whether the 24/7 livestream service is running
//!   stems generate        — run Demucs on a track or album to produce stems
//!   export album          — bundle FLAC + cover + (optional) stems for Spotify/DistroKid
//!
//! All subcommands that touch the database resolve their SQLite path via
//! `AppConfig` (NIGHTDRIVE_CONFIG env / fallback list). Override the config
//! file with `--config <path>` for one-off runs.

use anyhow::{Context, anyhow};
use clap::{Parser, Subcommand};
use nightdrive_core::{CompositionSpec, TrackPaths, config::AppConfig};
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
