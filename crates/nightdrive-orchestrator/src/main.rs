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
use tracing::{error, info, instrument};

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
        Command::Livestream => livestream(&cfg).await,
        Command::Resume => resume(&cfg).await,
        Command::Status => status(&cfg).await,
    }
}

#[instrument(skip(cfg))]
async fn run_batch(cfg: &AppConfig, count: u32, dry_run: bool) -> anyhow::Result<()> {
    info!(count, dry_run, "starting batch");

    for sequence in 1..=count {
        let track_id = nightdrive_core::TrackId::new(chrono::Utc::now().date_naive(), sequence);
        let span = tracing::info_span!("track", id = %track_id, sequence);
        let _enter = span.enter();

        if let Err(e) = pipeline_one(cfg, &track_id, dry_run).await {
            error!(error = %e, error.chain = ?e, "pipeline failed for track");
            // continue with the next track — one failure doesn't abort the batch
        }
    }

    info!("batch complete");
    Ok(())
}

/// The full pipeline for one track. Each stage is its own instrumented call.
#[instrument(skip(cfg), fields(track_id = %track_id))]
async fn pipeline_one(
    cfg: &AppConfig,
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

    // -- Stage 2+3: audio + cover in parallel -----------------------------
    info!("stage=2-3 audio_gen and cover (parallel)");
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

    // -- Stage 4: master ---------------------------------------------------
    info!("stage=4 master");
    {
        use nightdrive_audio_master::AudioMaster;
        let master = nightdrive_audio_master::FfmpegMaster::new(cfg.mastering.clone());
        master.run(&paths).await?;
    }

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

    // -- Stage 7: upload ---------------------------------------------------
    if dry_run {
        info!("dry_run=true, skipping upload");
        return Ok(());
    }
    info!("stage=7 upload");
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
    // Custom-thumbnail upload requires the YouTube channel to be phone-verified
    // (Channel → Settings → Verify). Until that's done the API returns
    // 403 youtube.thumbnail.forbidden. The video itself uploads fine and
    // YouTube auto-generates a thumbnail from frame samples — that's good
    // enough for MVP, so we log + continue rather than fail the pipeline.
    if let Err(e) = yt.set_thumbnail(&result.video_id, &paths.thumbnail_jpg()).await {
        let msg = e.to_string();
        if msg.contains("403") && msg.contains("thumbnail") {
            tracing::warn!(
                video_id = %result.video_id,
                "thumbnail set 403 — channel needs phone verification at \
                 youtube.com/verify; using YouTube's auto-generated thumbnail"
            );
        } else {
            return Err(e.into());
        }
    }

    info!(video_id = %result.video_id, "track published");
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

#[instrument(skip(_cfg))]
async fn resume(_cfg: &AppConfig) -> anyhow::Result<()> {
    // TODO(nightdrive): find tracks where state IN (spec_generated, audio_rendered, ...)
    // and re-run the pipeline from that stage forward.
    bail!("resume not yet implemented in N1.1; see ROADMAP.md N1.12")
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
