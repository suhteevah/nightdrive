//! nightdrive-cli — manual triggers, db operations, YouTube auth, status queries.
//!
//! Subcommands:
//!   db migrate       — run pending sqlx migrations
//!   youtube auth     — OAuth Desktop flow to obtain a refresh token
//!   tracks list      — print recent tracks and their pipeline state
//!   uploads list     — print upload history
//!   stream status    — check whether the 24/7 livestream service is running
//!
//! All subcommands that touch the database resolve their SQLite path via
//! `AppConfig` (NIGHTDRIVE_CONFIG env / fallback list). Override the config
//! file with `--config <path>` for one-off runs.

use anyhow::{Context, anyhow};
use clap::{Parser, Subcommand};
use nightdrive_core::config::AppConfig;
use nightdrive_storage::{Db, Tracks, Uploads};

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
