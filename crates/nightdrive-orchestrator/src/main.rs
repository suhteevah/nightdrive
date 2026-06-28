//! nightdrive-orchestrator — the pipeline binary.
//!
//! Subcommands:
//!   run-batch    — generate N tracks end to end (cron entry point)
//!   livestream   — long-running 24/7 stream supervisor
//!   resume       — pick up failed/incomplete tracks and finish them
//!   status       — print pipeline + recent runs

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use nightdrive_core::config::AppConfig;
use nightdrive_core::observability;
use nightdrive_core::TrackState;
use nightdrive_storage::{Db, Tracks, Uploads};
use tracing::{error, info, instrument, warn};

#[derive(Parser)]
#[command(name = "nightdrive-orchestrator", version)]
struct Cli {
    /// Path to nightdrive.toml. Overrides NIGHTDRIVE_CONFIG.
    #[arg(long, env = "NIGHTDRIVE_CONFIG")]
    config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the full pipeline for N tracks and exit. Cron entry point.
    RunBatch {
        #[arg(long, default_value_t = 1)]
        count: u32,
        /// Skip the YouTube upload step.
        #[arg(long)]
        dry_run: bool,
    },
    /// Render every track of a pre-composed album. Reads
    /// `docs/albums/<slug>.json` for the per-track specs and uses pre-rendered
    /// covers from `assets/covers/albums/<slug>/track-NN.png`. Skips stage 1
    /// (LLM) and stage 3 (art) since both are pre-baked; runs audio + master +
    /// encode + upload per track.
    RunAlbum {
        /// Album slug, matches docs/albums/<slug>.json.
        #[arg(long)]
        slug: String,
        /// Only render tracks at or after this number (1-indexed). For resume.
        #[arg(long, default_value_t = 1)]
        from_track: u32,
        /// Stop after this track number (inclusive). 0 = render through end.
        #[arg(long, default_value_t = 0)]
        to_track: u32,
        /// Synchronized 1-shot album drop — every track's scheduled_publish_at
        /// is this exact RFC3339 timestamp (e.g. `2026-05-15T18:00:00Z`),
        /// regardless of when individual uploads complete. When unset, each
        /// track falls back to `now + [youtube].schedule_offset_hours` per-
        /// upload (the trickle release pattern). For album conventions and
        /// coordinated press moments, set this. Per
        /// `memory/feedback_sync_drop_for_future_albums.md`.
        #[arg(long)]
        publish_at: Option<String>,
        /// Skip the YouTube upload step.
        #[arg(long)]
        dry_run: bool,
    },
    /// Long-running 24/7 livestream supervisor.
    Livestream,
    /// Retry any tracks left in a non-terminal state.
    Resume,
    /// Print pipeline status.
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    observability::init().context("failed to init tracing")?;

    info!(version = env!("CARGO_PKG_VERSION"), "nightdrive-orchestrator starting");

    let cli = Cli::parse();

    if let Some(p) = &cli.config {
        // SAFETY: single-threaded section before tokio spawn, fine to set.
        unsafe { std::env::set_var("NIGHTDRIVE_CONFIG", p) };
    }
    let cfg = AppConfig::load().context("failed to load config")?;

    match cli.command {
        Command::RunBatch { count, dry_run } => run_batch(&cfg, count, dry_run).await,
        Command::RunAlbum { slug, from_track, to_track, publish_at, dry_run } => {
            let parsed_publish_at = publish_at
                .as_deref()
                .map(|s| chrono::DateTime::parse_from_rfc3339(s)
                    .map(|d| d.with_timezone(&chrono::Utc)))
                .transpose()
                .context("--publish-at must be RFC3339 (e.g. 2026-05-15T18:00:00Z)")?;
            if let Some(t) = parsed_publish_at {
                let min_future = chrono::Utc::now() + chrono::Duration::hours(1);
                if t < min_future {
                    anyhow::bail!(
                        "--publish-at {t} must be at least 1h in the future to avoid races with MG audio gen wall time"
                    );
                }
            }
            run_album(&cfg, &slug, from_track, to_track, parsed_publish_at, dry_run).await
        }
        Command::Livestream => livestream(&cfg).await,
        Command::Resume => resume(&cfg).await,
        Command::Status => status(&cfg).await,
    }
}

#[instrument(skip(cfg))]
async fn run_batch(cfg: &AppConfig, count: u32, dry_run: bool) -> anyhow::Result<()> {
    info!(count, dry_run, "starting batch");

    // One DB handle for the whole batch — the pool is cheap to clone if a
    // future change wants to fan tracks across tokio tasks.
    let db = Db::connect_and_migrate(&cfg.paths.sqlite_db)
        .await
        .context("open sqlite + run migrations")?;

    for sequence in 1..=count {
        let track_id = nightdrive_core::TrackId::new(chrono::Utc::now().date_naive(), sequence);
        let span = tracing::info_span!("track", id = %track_id, sequence);
        let _enter = span.enter();

        if let Err(e) = pipeline_one(cfg, &db, &track_id, dry_run).await {
            error!(error = %e, error.chain = ?e, "pipeline failed for track");
            // Best-effort: mark Failed so `resume` doesn't try to revive it.
            // "track not found" is the expected path when stage 1 itself
            // failed before the Tracks::insert landed — log + move on.
            if let Err(mark_err) =
                Tracks::update_state(&db, &track_id, TrackState::Failed).await
            {
                warn!(error = %mark_err, "couldn't mark track failed (likely pre-insert)");
            }
        }
    }

    info!("batch complete");
    Ok(())
}

/// The full pipeline for one track. Each stage is its own instrumented call.
///
/// Storage transitions happen at every stage boundary so `resume` (and any
/// future operator-facing status query) can see exactly where a track is.
/// Stages 2 and 3 run in parallel and `join!` together, so the state machine
/// skips directly from `SpecGenerated` to `CoverRendered` once the join
/// completes — both audio and cover are durable on disk by then.
#[instrument(skip(cfg, db), fields(track_id = %track_id))]
async fn pipeline_one(
    cfg: &AppConfig,
    db: &Db,
    track_id: &nightdrive_core::TrackId,
    dry_run: bool,
) -> anyhow::Result<()> {
    let paths = nightdrive_core::TrackPaths::new(&cfg.paths.work_dir, track_id);
    tokio::fs::create_dir_all(&paths.root)
        .await
        .with_context(|| format!("mkdir {}", paths.root.display()))?;

    // -- Stage 1: composition spec ----------------------------------------
    info!("stage=1 composition_spec");
    let llm = nightdrive_llm::OpenclawLlm::new(cfg.openclaw.clone())?;
    use nightdrive_llm::CompositionLlm;
    let spec = llm.generate_spec(track_id).await?;
    tokio::fs::write(paths.spec_json(), serde_json::to_vec_pretty(&spec)?).await?;
    // Persist track row immediately so a stage-2+ failure is recoverable via
    // `resume`. Seed is derived from track_id deterministically so re-renders
    // of the same id produce the same audio + cover.
    let seed = djb2_hash(track_id.as_str()) as i64;
    Tracks::insert(db, &spec, seed).await?;
    Tracks::update_state(db, track_id, TrackState::SpecGenerated).await?;

    // -- Stage 2+3: audio + cover in parallel -----------------------------
    info!("stage=2-3 audio_gen and cover (parallel)");
    run_audio_and_cover(cfg, &spec, &paths).await?;
    // Audio and cover both landed; the state machine compresses the two
    // parallel stages into one transition.
    Tracks::update_state(db, track_id, TrackState::CoverRendered).await?;

    // -- Stage 4: master ---------------------------------------------------
    info!("stage=4 master");
    {
        use nightdrive_audio_master::AudioMaster;
        let master = nightdrive_audio_master::FfmpegMaster::new(cfg.mastering.clone());
        master.run(&paths).await?;
    }
    Tracks::update_state(db, track_id, TrackState::AudioMastered).await?;

    // -- Stage 5: visualizer (MVP placeholder via ffmpeg showwaves) -------
    // The wgpu visualizer is N3.1 (multi-week). MVP path per ROADMAP §10:
    // "Visuals at this stage can be a static cover art + waveform (ffmpeg
    // showwaves filter) — ugly but ships." The showwaves overlay actually
    // lives inside nightdrive-encoder's filter graph so this stage is a
    // structural no-op until N3.1 lands.
    info!("stage=5 visualizer (placeholder: showwaves overlay baked into stage 6)");

    // -- Stage 6: final encode --------------------------------------------
    info!("stage=6 final encode");
    {
        use nightdrive_encoder::FinalEncoder;
        let encoder = nightdrive_encoder::FfmpegEncoder::new(cfg.encoder.clone());
        encoder.compose(&paths, &spec).await?;
        // Re-encode the cover.png to JPEG for the thumbnail (YouTube caps
        // thumbnails at 2 MB / JPEG; SDXL covers are PNG often 1-2 MB+).
        nightdrive_encoder::make_thumbnail(&paths).await?;
    }
    Tracks::update_state(db, track_id, TrackState::VideoEncoded).await?;

    // -- Stage 7: upload ---------------------------------------------------
    if dry_run {
        info!("dry_run=true, skipping upload");
        return Ok(());
    }
    info!("stage=7 upload");
    // Insert the upload row in `queued` state BEFORE the PUT begins so a
    // mid-upload crash leaves a discoverable trail. set_youtube_id flips to
    // `complete` on success; a failed upload stays `queued` for `resume`.
    let upload_id = Uploads::insert(db, track_id).await?;
    let creds = nightdrive_youtube::YoutubeCredentials::from_env()?;
    let yt = nightdrive_youtube::YoutubeClient::new(creds)?;
    use nightdrive_youtube::YoutubeUploader;
    let req = nightdrive_youtube::UploadRequest {
        spec: &spec,
        paths: &paths,
        privacy: match cfg.youtube.default_privacy.as_str() {
            "public" => nightdrive_youtube::Privacy::Public,
            "unlisted" => nightdrive_youtube::Privacy::Unlisted,
            _ => nightdrive_youtube::Privacy::Private,
        },
        scheduled_publish_at: Some(
            chrono::Utc::now() + chrono::Duration::hours(cfg.youtube.schedule_offset_hours),
        ),
        declare_synthetic_content: cfg.youtube.declare_synthetic_content,
    };
    let result = yt.upload_video(req).await?;
    Uploads::set_youtube_id(db, upload_id, &result.video_id).await?;
    // Custom-thumbnail upload requires the YouTube channel to be phone-verified
    // (Channel → Settings → Verify). Until that's done the API returns
    // 403 youtube.thumbnail.forbidden. The video itself uploads fine and
    // YouTube auto-generates a thumbnail from frame samples — that's good
    // enough for MVP, so we log + continue rather than fail the pipeline.
    set_thumbnail_best_effort(&yt, db, track_id, &result.video_id, &paths.thumbnail_jpg()).await?;

    Tracks::update_state(db, track_id, TrackState::Published).await?;
    info!(video_id = %result.video_id, "track published");
    Ok(())
}

/// Set the YouTube thumbnail with best-effort semantics: the video upload
/// itself has already succeeded by the time we call this, so two known
/// failure modes — channel-not-verified (403) and rate-limit (429) — are
/// downgraded to a warn rather than failing the whole pipeline. YouTube
/// auto-generates a thumbnail from frame samples when we don't set one,
/// so the video is still presentable. Any other thumbnail error (auth
/// expired, malformed JPEG, etc.) still bubbles.
///
/// On success, stamps `custom_thumbnail_set = 1` in the DB.
/// On 403/429, stamps `thumbnail_last_attempt_at` so the retry sweep
/// can skip recent attempts.
async fn set_thumbnail_best_effort(
    yt: &nightdrive_youtube::YoutubeClient,
    db: &Db,
    track_id: &nightdrive_core::TrackId,
    video_id: &str,
    thumb_path: &std::path::Path,
) -> anyhow::Result<()> {
    use nightdrive_youtube::YoutubeUploader;
    if let Err(e) = yt.set_thumbnail(video_id, thumb_path).await {
        let msg = e.to_string();
        let is_403_unverified = msg.contains("403") && msg.contains("thumbnail");
        let is_429_ratelimit = msg.contains("429") && msg.contains("thumbnail");
        if is_403_unverified {
            warn!(
                video_id,
                "thumbnail 403 — channel needs phone verification at youtube.com/verify; YT auto-generated thumbnail will be used"
            );
            if let Err(db_err) = Tracks::mark_thumbnail_attempted(db, track_id).await {
                warn!(%db_err, "failed to stamp thumbnail_last_attempt_at after 403");
            }
        } else if is_429_ratelimit {
            warn!(
                video_id,
                "thumbnail 429 — YT per-channel thumbnail upload rate limit hit (~100/day); auto-generated thumbnail will be used. Retry later via a `nightdrive-cli thumbnails retry-failed` pass."
            );
            if let Err(db_err) = Tracks::mark_thumbnail_attempted(db, track_id).await {
                warn!(%db_err, "failed to stamp thumbnail_last_attempt_at after 429");
            }
        } else {
            return Err(e.into());
        }
    } else {
        if let Err(db_err) = Tracks::mark_thumbnail_set(db, track_id).await {
            warn!(%db_err, "thumbnail uploaded OK but failed to stamp custom_thumbnail_set=1");
        }
    }
    Ok(())
}

/// djb2 hash of a track id. Same function used by nightdrive-audio-gen and
/// nightdrive-art so seed-derived behavior (audio segment seeds, cover
/// palette, cover library pick) stays symmetric across crates.
fn djb2_hash(s: &str) -> u64 {
    let mut h: u64 = 5381;
    for b in s.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    h
}

/// Render audio (stage 2) and cover (stage 3) in parallel.
///
/// Extracted from `pipeline_one` so `resume_one` can call the same
/// implementation when picking up a track in `SpecGenerated` /
/// `AudioRendered` state. Does NOT touch the storage state machine —
/// callers handle their own transitions so resume + run-batch can update
/// the same row from different angles.
async fn run_audio_and_cover(
    cfg: &AppConfig,
    spec: &nightdrive_core::CompositionSpec,
    paths: &nightdrive_core::TrackPaths,
) -> anyhow::Result<()> {
    let audio_task = {
        let cfg = cfg.audio_gen.clone();
        let spec = spec.clone();
        let paths = paths.clone();
        tokio::spawn(async move {
            // `client_for` picks StableAudioClient or MusicGenClient based on
            // `[audio_gen].engine` in nightdrive.toml.
            let client = nightdrive_audio_gen::client_for(cfg)?;
            client.render(&spec, &paths).await.map_err(anyhow::Error::from)
        })
    };
    let art_task = {
        let cfg = cfg.art.clone();
        let spec = spec.clone();
        let paths = paths.clone();
        tokio::spawn(async move {
            use nightdrive_art::CoverArtist;
            // Cover-art fallback chain, top to bottom:
            //   1. Real SDXL sidecar (live SDXL inference — N1.7, runs on cnc
            //      post-P100s with VRAM headroom)
            //   2. Pre-rendered library at assets/covers/library/ — generated
            //      offline via sidecar/generate_cover_library.py, picked by
            //      hash(track_id) mod library_size so the same track always
            //      gets the same cover
            //   3. ffmpeg-rendered gradient — deterministic per-track palette,
            //      the "ugly but ships" floor of the ROADMAP §10 MVP cutoff
            // Once cnc is up the first path always wins. Until then, the
            // library is the visual-appeal path.
            let client = nightdrive_art::SdxlClient::new(cfg.clone())?;
            match client.render(&spec, &paths).await {
                Ok(p) => Ok::<std::path::PathBuf, anyhow::Error>(p),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "SDXL sidecar unreachable — trying cover library"
                    );
                    match pick_from_library(&spec, &paths).await {
                        Ok(true) => {
                            tracing::info!("cover served from library");
                            Ok(paths.cover_png())
                        }
                        Ok(false) => {
                            tracing::warn!("cover library empty — falling through to gradient");
                            placeholder_cover(&spec, &paths, cfg.width, cfg.height).await?;
                            Ok(paths.cover_png())
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "cover library pick failed — gradient");
                            placeholder_cover(&spec, &paths, cfg.width, cfg.height).await?;
                            Ok(paths.cover_png())
                        }
                    }
                }
            }
        })
    };
    let (audio_res, art_res) = tokio::join!(audio_task, art_task);
    audio_res??;
    art_res??;
    Ok(())
}

/// Render every track of a pre-composed album. Reads
/// `docs/albums/<slug>.json` for specs + uses pre-rendered covers from
/// `assets/covers/albums/<slug>/track-NN.png`. Stages 1 (LLM) and 3 (art)
/// are skipped; everything else runs identical to the normal pipeline.
#[instrument(skip(cfg), fields(slug, from_track, to_track, dry_run))]
async fn run_album(
    cfg: &AppConfig,
    slug: &str,
    from_track: u32,
    to_track: u32,
    publish_at: Option<chrono::DateTime<chrono::Utc>>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let album_json_path = std::path::PathBuf::from("docs/albums").join(format!("{slug}.json"));
    let album_json = tokio::fs::read_to_string(&album_json_path).await
        .with_context(|| format!("read {}", album_json_path.display()))?;
    let album: serde_json::Value = serde_json::from_str(&album_json)
        .with_context(|| format!("parse {}", album_json_path.display()))?;
    let album_title = album["title"].as_str().unwrap_or(slug).to_string();
    let tracks = album["tracks"].as_array()
        .ok_or_else(|| anyhow::anyhow!("album.tracks must be an array"))?;
    info!(
        slug = %slug,
        album_title = %album_title,
        track_count = tracks.len(),
        "album loaded"
    );

    let db = Db::connect_and_migrate(&cfg.paths.sqlite_db).await
        .context("open sqlite + run migrations")?;

    let cover_dir = std::path::PathBuf::from("assets/covers/albums").join(slug);

    for track_value in tracks {
        let track_num = track_value["track_number"].as_u64()
            .ok_or_else(|| anyhow::anyhow!("track_number missing"))? as u32;
        if track_num < from_track { continue; }
        if to_track > 0 && track_num > to_track { continue; }

        let track_id = nightdrive_core::TrackId(format!("nd-{slug}-{track_num:03}"));
        let cover_src = cover_dir.join(format!("track-{track_num:02}.png"));

        let span = tracing::info_span!(
            "album_track",
            id = %track_id,
            num = track_num,
            title = %track_value["title"].as_str().unwrap_or("?"),
        );
        let _enter = span.enter();

        let spec = match spec_from_album_track(track_value, track_id.clone(), slug, &album_title) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "couldn't build spec from album track");
                continue;
            }
        };

        if let Err(e) = pipeline_one_album(cfg, &db, &track_id, &spec, &cover_src, publish_at, dry_run).await {
            error!(error = %e, error.chain = ?e, "album track pipeline failed");
            if let Err(mark_err) =
                Tracks::update_state(&db, &track_id, TrackState::Failed).await
            {
                warn!(error = %mark_err, "couldn't mark album track failed");
            }
        }
    }

    info!("album complete");
    Ok(())
}

/// Assemble a YouTube-safe video title. YouTube hard-rejects titles over 100
/// characters with a 400 "invalid or empty video title", so build the full
/// decorated title and, only if it overflows, shed the least-essential
/// segments in order: first "(Track NN)" (recoverable from playlist order),
/// then the "[Synthwave for Coding]" SEO suffix, and finally a hard
/// char-boundary truncation as a last resort. Counts Unicode chars (em dash = 1).
fn build_youtube_title(title: &str, album_title: &str, track_number: u32) -> String {
    let full =
        format!("{title} — {album_title} (Track {track_number:02}) [Synthwave for Coding]");
    if full.chars().count() <= 100 {
        return full;
    }
    let no_track = format!("{title} — {album_title} [Synthwave for Coding]");
    if no_track.chars().count() <= 100 {
        return no_track;
    }
    let bare = format!("{title} — {album_title}");
    if bare.chars().count() <= 100 {
        return bare;
    }
    bare.chars().take(100).collect()
}

/// Build a [`CompositionSpec`] from one entry in `docs/albums/<slug>.json`.
/// Maps the album JSON's loose schema (which has extras like `composer_notes`,
/// `key_relationship_to_prior`, etc) into the strict CompositionSpec the
/// pipeline crates consume. Constructs YT metadata from album-level info.
fn spec_from_album_track(
    track_value: &serde_json::Value,
    track_id: nightdrive_core::TrackId,
    slug: &str,
    album_title: &str,
) -> anyhow::Result<nightdrive_core::CompositionSpec> {
    use nightdrive_core::{CompositionSpec, Section, YoutubeMetadata};
    let title = track_value["title"].as_str()
        .ok_or_else(|| anyhow::anyhow!("title"))?.to_string();
    let bpm = track_value["bpm"].as_u64()
        .ok_or_else(|| anyhow::anyhow!("bpm"))? as u32;
    // Album JSON uses "key"; CompositionSpec uses "musical_key".
    let musical_key = track_value["key"].as_str()
        .ok_or_else(|| anyhow::anyhow!("key"))?.to_string();
    let duration_seconds = track_value["duration_seconds"].as_u64()
        .ok_or_else(|| anyhow::anyhow!("duration_seconds"))? as u32;
    let track_number = track_value["track_number"].as_u64().unwrap_or(0) as u32;
    let mood_tags: Vec<String> = track_value["mood_tags"].as_array()
        .ok_or_else(|| anyhow::anyhow!("mood_tags"))?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    let musicgen_prompt = track_value["musicgen_prompt"].as_str()
        .ok_or_else(|| anyhow::anyhow!("musicgen_prompt"))?.to_string();
    let cover_prompt = track_value["cover_prompt"].as_str()
        .ok_or_else(|| anyhow::anyhow!("cover_prompt"))?.to_string();
    let sections: Vec<Section> = track_value["sections"].as_array()
        .ok_or_else(|| anyhow::anyhow!("sections"))?
        .iter()
        .map(|s| -> anyhow::Result<Section> {
            Ok(Section {
                name: s["name"].as_str()
                    .ok_or_else(|| anyhow::anyhow!("section.name"))?.to_string(),
                bars: s["bars"].as_u64()
                    .ok_or_else(|| anyhow::anyhow!("section.bars"))? as u32,
                instrumentation: s["instrumentation"].as_str()
                    .ok_or_else(|| anyhow::anyhow!("section.instrumentation"))?.to_string(),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    // YouTube metadata: build a presentable title + description from album
    // context. Title format mirrors the manual tracks already on the channel.
    let yt_title = build_youtube_title(&title, album_title, track_number);
    let yt_description = format!(
        "Track {track_number:02} of {album_title}.\n\n\
         Key: {musical_key} · {bpm} BPM · {duration_seconds}s\n\n\
         Part of nightdrive's autonomous synthwave album series. \
         Best listened to in order.\n\n\
         #synthwave #codingmusic #lofi #study #{slug}",
        slug = slug.replace('-', "")
    );

    Ok(CompositionSpec {
        track_id,
        title,
        subgenre: "synthwave".to_string(),
        mood_tags,
        bpm,
        musical_key,
        duration_seconds,
        sections,
        musicgen_prompt,
        cover_prompt,
        youtube: YoutubeMetadata {
            title: yt_title,
            description: yt_description,
            tags: vec![
                "synthwave".to_string(),
                "coding music".to_string(),
                "lofi".to_string(),
                "study".to_string(),
                "retrowave".to_string(),
                album_title.to_string(),
                slug.to_string(),
            ],
            category_id: "10".to_string(),
        },
    })
}

/// Album-mode pipeline for one track. Identical to `pipeline_one` minus
/// stage 1 (LLM — spec is pre-baked) and stage 3 (art — cover is pre-rendered
/// on disk). Audio, master, encode, upload all run; state transitions are
/// the same so `resume` works on partially-rendered albums.
#[instrument(skip(cfg, db, spec), fields(track_id = %track_id))]
async fn pipeline_one_album(
    cfg: &AppConfig,
    db: &Db,
    track_id: &nightdrive_core::TrackId,
    spec: &nightdrive_core::CompositionSpec,
    cover_src: &std::path::Path,
    publish_at: Option<chrono::DateTime<chrono::Utc>>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let paths = nightdrive_core::TrackPaths::new(&cfg.paths.work_dir, track_id);
    tokio::fs::create_dir_all(&paths.root).await
        .with_context(|| format!("mkdir {}", paths.root.display()))?;

    // Stage 1 SKIPPED. Spec is pre-baked from the album JSON. Write spec.json
    // for downstream tooling visibility.
    info!("stage=1 SKIPPED (album mode — spec pre-baked)");
    tokio::fs::write(paths.spec_json(), serde_json::to_vec_pretty(spec)?).await?;
    let seed = djb2_hash(track_id.as_str()) as i64;
    Tracks::insert(db, spec, seed).await?;
    Tracks::update_state(db, track_id, TrackState::SpecGenerated).await?;

    // Stage 3 SKIPPED. Copy pre-rendered album cover into the per-track dir.
    info!("stage=3 cover (copying pre-rendered: {})", cover_src.display());
    if !tokio::fs::try_exists(cover_src).await.unwrap_or(false) {
        anyhow::bail!(
            "album cover missing at {} — generate it first via `python sidecar/generate_cover_library.py --album <slug>`",
            cover_src.display()
        );
    }
    tokio::fs::copy(cover_src, paths.cover_png()).await
        .with_context(|| format!("copy {} -> {}", cover_src.display(), paths.cover_png().display()))?;

    // Skip-on-state: file-existence-based stage gating so a failed-late re-run
    // (e.g. YT OAuth invalid_grant on stage 7) doesn't re-do the ~3 min/track
    // of audio + master + encode. File-existence is more robust than DB state
    // because it survives state drift / Failed-marker overwrites.
    let raw_wav_exists =
        tokio::fs::try_exists(paths.raw_audio_wav()).await.unwrap_or(false);
    let master_flac_exists =
        tokio::fs::try_exists(paths.master_flac()).await.unwrap_or(false);
    let final_mp4_exists =
        tokio::fs::try_exists(paths.final_mp4()).await.unwrap_or(false);

    // Stage 2: audio. Same code path as `pipeline_one`; the music is the
    // expensive bit (~42 s/track on cnc ACE-Step split-GPU).
    if raw_wav_exists || master_flac_exists || final_mp4_exists {
        info!("stage=2 audio_gen SKIPPED (raw.wav / downstream artifact present)");
    } else {
        info!("stage=2 audio_gen");
        let client = nightdrive_audio_gen::client_for(cfg.audio_gen.clone())?;
        client.render(spec, &paths).await?;
    }
    Tracks::update_state(db, track_id, TrackState::CoverRendered).await?;

    // Stage 4: master
    if master_flac_exists || final_mp4_exists {
        info!("stage=4 master SKIPPED (master.flac / final.mp4 present)");
    } else {
        info!("stage=4 master");
        use nightdrive_audio_master::AudioMaster;
        let master = nightdrive_audio_master::FfmpegMaster::new(cfg.mastering.clone());
        master.run(&paths).await?;
    }
    Tracks::update_state(db, track_id, TrackState::AudioMastered).await?;

    info!("stage=5 visualizer (placeholder: showwaves overlay baked into stage 6)");

    // Stage 6: encode
    if final_mp4_exists {
        info!("stage=6 final encode SKIPPED (final.mp4 present)");
    } else {
        info!("stage=6 final encode");
        use nightdrive_encoder::FinalEncoder;
        let encoder = nightdrive_encoder::FfmpegEncoder::new(cfg.encoder.clone());
        encoder.compose(&paths, spec).await?;
        nightdrive_encoder::make_thumbnail(&paths).await?;
    }
    Tracks::update_state(db, track_id, TrackState::VideoEncoded).await?;

    // Stage 7: upload
    if dry_run {
        info!("dry_run=true, skipping upload — state stays VideoEncoded");
        return Ok(());
    }
    info!("stage=7 upload");
    let upload_id = Uploads::insert(db, track_id).await?;
    let creds = nightdrive_youtube::YoutubeCredentials::from_env()?;
    let yt = nightdrive_youtube::YoutubeClient::new(creds)?;
    use nightdrive_youtube::YoutubeUploader;
    let req = nightdrive_youtube::UploadRequest {
        spec,
        paths: &paths,
        privacy: match cfg.youtube.default_privacy.as_str() {
            "public" => nightdrive_youtube::Privacy::Public,
            "unlisted" => nightdrive_youtube::Privacy::Unlisted,
            _ => nightdrive_youtube::Privacy::Private,
        },
        scheduled_publish_at: Some(publish_at.unwrap_or_else(||
            chrono::Utc::now() + chrono::Duration::hours(cfg.youtube.schedule_offset_hours)
        )),
        declare_synthetic_content: cfg.youtube.declare_synthetic_content,
    };
    let result = yt.upload_video(req).await?;
    Uploads::set_youtube_id(db, upload_id, &result.video_id).await?;
    set_thumbnail_best_effort(&yt, db, track_id, &result.video_id, &paths.thumbnail_jpg()).await?;
    Tracks::update_state(db, track_id, TrackState::Published).await?;
    info!(video_id = %result.video_id, "album track published");
    Ok(())
}

#[instrument(skip(cfg))]
async fn livestream(cfg: &AppConfig) -> anyhow::Result<()> {
    info!(
        port = cfg.livestream.visualizer_ws_port,
        shuffle = cfg.livestream.shuffle_buffer_size,
        "livestream supervisor starting"
    );
    // TODO(nightdrive):
    //   1. open SQLite, pull tracks ORDER BY last_streamed_at ASC LIMIT N
    //   2. spin up an audio player (rodio or libpulse) into a virtual sink
    //   3. spin up a tiny WS server on visualizer_ws_port broadcasting
    //      { now_playing, fft_spectrum } at metadata_refresh_seconds
    //   4. OBS (running elsewhere) consumes the WS + virtual sink and pushes RTMP
    //   5. update last_streamed_at after each track completes
    //   6. on SIGTERM, finish current track then exit
    Ok(())
}

/// Resolve the cover-library directory. Looks for `assets/covers/library`
/// relative to the orchestrator's current working directory; the operator
/// is expected to run the binary from the workspace root (or the systemd
/// unit sets WorkingDirectory there). Returns None if the directory doesn't
/// exist or has no PNG files — that's the "library empty" case the fallback
/// chain handles upstream.
fn cover_library_dir() -> Option<std::path::PathBuf> {
    let candidates = [
        std::path::PathBuf::from("assets/covers/library"),
        std::path::PathBuf::from("J:/nightdrive/assets/covers/library"),
    ];
    candidates.into_iter().find(|p| p.is_dir())
}

/// Pick a cover from `assets/covers/library/` by hashing the track_id mod
/// library_size, copy it to `paths.cover_png()`. Returns Ok(true) if a cover
/// was successfully placed, Ok(false) if the library was empty.
async fn pick_from_library(
    spec: &nightdrive_core::CompositionSpec,
    paths: &nightdrive_core::TrackPaths,
) -> anyhow::Result<bool> {
    let Some(library_dir) = cover_library_dir() else {
        return Ok(false);
    };
    let mut entries = tokio::fs::read_dir(&library_dir).await?;
    let mut covers: Vec<std::path::PathBuf> = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref()
            == Some("png")
        {
            covers.push(path);
        }
    }
    if covers.is_empty() {
        return Ok(false);
    }
    // Sort so the index is stable across runs even if read_dir order isn't.
    covers.sort();

    // djb2(track_id) mod library_size — same track always picks the same cover.
    let mut h: u64 = 5381;
    for b in spec.track_id.as_str().bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    let idx = (h as usize) % covers.len();
    let pick = &covers[idx];

    let dest = paths.cover_png();
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    tokio::fs::copy(pick, &dest).await?;
    tracing::info!(
        track_id = %spec.track_id,
        library_size = covers.len(),
        picked = %pick.file_name().unwrap_or_default().to_string_lossy(),
        "library cover selected"
    );
    Ok(true)
}

/// ffmpeg-generated fallback cover. Used when the real SDXL sidecar isn't
/// available (pre-P100s on kokonoe, VRAM-contended with the SAO sidecar).
/// Produces a 1024×1024 PNG with a deep-purple → magenta diagonal gradient
/// derived from the track id (so each track gets a slightly different palette).
/// Per the ROADMAP §10 MVP cutoff this is the "ugly but ships" path; once
/// N1.7 SDXL is live this branch never runs.
///
/// Intentionally no `drawtext` — Windows ffmpeg defaults blow up with
/// `0xc0000005 ACCESS_VIOLATION` when no `fontfile=` is supplied. The track
/// title still shows in the YouTube video metadata + description, so the
/// cover art being text-free is fine.
async fn placeholder_cover(
    spec: &nightdrive_core::CompositionSpec,
    paths: &nightdrive_core::TrackPaths,
    width: u32,
    height: u32,
) -> anyhow::Result<()> {
    use std::process::Stdio;
    let out = paths.cover_png();
    if let Some(parent) = out.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }

    // Hash the track_id into a hue offset so different tracks get visually
    // distinguishable covers. The base palette stays "synthwave purple/magenta"
    // but the gradient angle varies.
    let mut h: u64 = 5381;
    for b in spec.track_id.as_str().bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    // Two seed-derived purple-magenta endpoints.
    let r1 = ((h & 0x3F) + 0x20) as u32;
    let g1 = ((h >> 6) & 0x1F) as u32;
    let b1 = ((h >> 11) & 0x7F) as u32 + 0x40;
    let r2 = ((h >> 18) & 0x7F) as u32 + 0x60;
    let g2 = ((h >> 25) & 0x3F) as u32;
    let b2 = ((h >> 31) & 0x7F) as u32 + 0x80;
    let c1 = format!("0x{r1:02X}{g1:02X}{b1:02X}");
    let c2 = format!("0x{r2:02X}{g2:02X}{b2:02X}");

    // gradients filter (ffmpeg ≥ 6): linear two-color, single frame.
    let filter = format!(
        "gradients=s={width}x{height}:c0={c1}:c1={c2}:x0=0:y0=0:x1={width}:y1={height}:d=1[v]"
    );

    let status = tokio::process::Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-nostats"])
        .args(["-f", "lavfi", "-i", &filter.replace("[v]", "")])
        .args(["-frames:v", "1"])
        .arg(&out)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !status.status.success() {
        let tail = String::from_utf8_lossy(&status.stderr);
        anyhow::bail!(
            "ffmpeg placeholder cover failed (status {}): {}",
            status.status,
            tail.lines().rev().take(5).collect::<Vec<_>>().join("\n"),
        );
    }
    tracing::info!(path = %out.display(), c0=%c1, c1=%c2, "placeholder cover written");
    Ok(())
}

/// Pick up every non-terminal track and re-run it from its current state.
///
/// Terminal states (`Published`, `Failed`) are skipped. Everything else gets
/// dispatched to [`resume_one`] which inspects [`TrackState`] and runs only
/// the stages that haven't completed yet. Per-track failures don't abort the
/// resume — same one-failure-isn't-batch-abort policy as `run_batch`.
#[instrument(skip(cfg))]
async fn resume(cfg: &AppConfig) -> anyhow::Result<()> {
    let db = Db::connect_and_migrate(&cfg.paths.sqlite_db)
        .await
        .context("open sqlite for resume")?;
    resume_with_db(cfg, &db, false).await
}

/// Inner [`resume`] body that takes an already-opened DB handle. Extracted
/// so the witness can drive it against a tempdir SQLite without going
/// through the cfg.paths.sqlite_db path.
#[instrument(skip(cfg, db), fields(dry_run))]
async fn resume_with_db(cfg: &AppConfig, db: &Db, dry_run: bool) -> anyhow::Result<()> {
    // Pull every non-terminal track. Each state gets its own `list(Some(_))`
    // call rather than a `WHERE state NOT IN (...)` join because the typed
    // storage API only exposes the filtered list.
    let non_terminal_states = [
        TrackState::Pending,
        TrackState::SpecGenerated,
        TrackState::AudioRendered,
        TrackState::CoverRendered,
        TrackState::AudioMastered,
        TrackState::VideoEncoded,
    ];
    let mut needs_resume: Vec<nightdrive_storage::TrackRow> = Vec::new();
    for state in non_terminal_states {
        let rows = Tracks::list(db, Some(state))
            .await
            .with_context(|| format!("list tracks in state {state:?}"))?;
        needs_resume.extend(rows);
    }
    info!(count = needs_resume.len(), dry_run, "resuming non-terminal tracks");
    if needs_resume.is_empty() {
        return Ok(());
    }

    for row in needs_resume {
        let track_id = row.id.clone();
        let state_label = row.state.as_str();
        let span = tracing::info_span!("resume_track", id = %track_id, from_state = state_label);
        let _enter = span.enter();
        if let Err(e) = resume_one(cfg, db, row, dry_run).await {
            error!(error = %e, error.chain = ?e, "resume failed for track");
            if let Err(mark_err) =
                Tracks::update_state(db, &track_id, TrackState::Failed).await
            {
                warn!(error = %mark_err, "couldn't mark resumed track failed");
            }
        }
    }
    info!("resume complete");
    Ok(())
}

/// Resume a single track from its stored state. State transitions are
/// idempotent (re-running a completed stage just overwrites the on-disk
/// artifact with the deterministic-seed re-render). The dispatch is
/// monotonic: each `if` only fires when the row's recorded progress is
/// before that stage.
async fn resume_one(
    cfg: &AppConfig,
    db: &Db,
    row: nightdrive_storage::TrackRow,
    dry_run: bool,
) -> anyhow::Result<()> {
    let spec: nightdrive_core::CompositionSpec = serde_json::from_str(&row.spec_json)
        .with_context(|| format!("parse spec_json for {}", row.id))?;
    let track_id = spec.track_id.clone();
    let paths = nightdrive_core::TrackPaths::new(&cfg.paths.work_dir, &track_id);
    tokio::fs::create_dir_all(&paths.root)
        .await
        .with_context(|| format!("mkdir {}", paths.root.display()))?;
    // Re-materialize spec.json on disk if missing — the encoder filter graph
    // doesn't read it but some downstream tooling does, and it's free.
    if tokio::fs::metadata(paths.spec_json()).await.is_err() {
        tokio::fs::write(paths.spec_json(), serde_json::to_vec_pretty(&spec)?).await?;
    }

    // `AudioRendered` is currently unreachable from the run-batch wiring
    // (stages 2 and 3 commit together as `CoverRendered`), but the storage
    // enum has it and the resume logic should still do the right thing if
    // it's ever set manually. Treat it like `SpecGenerated`: re-run both
    // stages 2 and 3. The audio re-render is wasteful but the seed is
    // deterministic so the output is bit-identical.
    let needs_audio_cover = matches!(
        row.state,
        TrackState::Pending | TrackState::SpecGenerated | TrackState::AudioRendered
    );
    let needs_master = needs_audio_cover || row.state == TrackState::CoverRendered;
    let needs_encode = needs_master || row.state == TrackState::AudioMastered;
    let needs_upload = needs_encode || row.state == TrackState::VideoEncoded;

    if needs_audio_cover {
        info!("resume stage=2-3 audio_gen and cover (parallel)");
        run_audio_and_cover(cfg, &spec, &paths).await?;
        Tracks::update_state(db, &track_id, TrackState::CoverRendered).await?;
    }
    if needs_master {
        info!("resume stage=4 master");
        use nightdrive_audio_master::AudioMaster;
        let master = nightdrive_audio_master::FfmpegMaster::new(cfg.mastering.clone());
        master.run(&paths).await?;
        Tracks::update_state(db, &track_id, TrackState::AudioMastered).await?;
    }
    if needs_encode {
        info!("resume stage=6 encode");
        use nightdrive_encoder::FinalEncoder;
        let encoder = nightdrive_encoder::FfmpegEncoder::new(cfg.encoder.clone());
        encoder.compose(&paths, &spec).await?;
        nightdrive_encoder::make_thumbnail(&paths).await?;
        Tracks::update_state(db, &track_id, TrackState::VideoEncoded).await?;
    }
    if needs_upload {
        if dry_run {
            info!("resume dry_run=true, skipping upload — state stays VideoEncoded");
            return Ok(());
        }
        info!("resume stage=7 upload");
        let upload_id = Uploads::insert(db, &track_id).await?;
        let creds = nightdrive_youtube::YoutubeCredentials::from_env()?;
        let yt = nightdrive_youtube::YoutubeClient::new(creds)?;
        use nightdrive_youtube::YoutubeUploader;
        let req = nightdrive_youtube::UploadRequest {
            spec: &spec,
            paths: &paths,
            privacy: match cfg.youtube.default_privacy.as_str() {
                "public" => nightdrive_youtube::Privacy::Public,
                "unlisted" => nightdrive_youtube::Privacy::Unlisted,
                _ => nightdrive_youtube::Privacy::Private,
            },
            scheduled_publish_at: Some(
                chrono::Utc::now() + chrono::Duration::hours(cfg.youtube.schedule_offset_hours),
            ),
            declare_synthetic_content: cfg.youtube.declare_synthetic_content,
        };
        let result = yt.upload_video(req).await?;
        Uploads::set_youtube_id(db, upload_id, &result.video_id).await?;
        set_thumbnail_best_effort(&yt, db, &track_id, &result.video_id, &paths.thumbnail_jpg()).await?;
        Tracks::update_state(db, &track_id, TrackState::Published).await?;
        info!(video_id = %result.video_id, "resumed track published");
    }
    Ok(())
}

#[instrument(skip(_cfg))]
async fn status(_cfg: &AppConfig) -> anyhow::Result<()> {
    // TODO(nightdrive): print:
    //   - last successful batch timestamp
    //   - last failed track + reason
    //   - count of tracks in each TrackState
    //   - is livestream service up
    bail!("status not yet implemented in N1.1; see ROADMAP.md N1.12")
}

#[cfg(test)]
mod title_tests {
    use super::build_youtube_title;

    #[test]
    fn youtube_title_never_exceeds_100_chars() {
        let cases = [
            ("Down Into the Deepest Dark", "Atlantis: The Drowned Motherland, Vol. 1", 12u32),
            ("Among the Stars (Homecoming)", "Gate of Ra: The Outward Flight, Vol. 1", 11),
            ("Through the Water Column", "Atlantis: The Drowned Motherland, Vol. 1", 9),
            (
                "An Extremely Long Track Title That Should Force Hard Truncation No Matter What",
                "Some Equally Verbose Album Subtitle That Keeps Going And Going, Vol. 1",
                11,
            ),
            ("Short", "Tiny", 1),
        ];
        for (t, a, n) in cases {
            let title = build_youtube_title(t, a, n);
            assert!(
                title.chars().count() <= 100,
                "title too long ({}): {title}",
                title.chars().count()
            );
            assert!(!title.is_empty(), "title empty");
        }
    }

    #[test]
    fn youtube_title_keeps_full_form_when_it_fits() {
        let title =
            build_youtube_title("The Deluge", "Atlantis: The Drowned Motherland, Vol. 1", 7);
        assert!(title.contains("(Track 07)"));
        assert!(title.contains("[Synthwave for Coding]"));
    }

    #[test]
    fn youtube_title_drops_track_tag_first_when_over() {
        let title = build_youtube_title(
            "Among the Stars (Homecoming)",
            "Gate of Ra: The Outward Flight, Vol. 1",
            11,
        );
        assert!(title.chars().count() <= 100);
        assert!(!title.contains("(Track 11)"), "should drop track tag: {title}");
        assert!(title.contains("[Synthwave for Coding]"), "should keep SEO suffix: {title}");
    }
}
