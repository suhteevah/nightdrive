//! nightdrive-encoder — ffmpeg final mux: master + cover + waveform + 3-panel
//! TWC-style HUD → final.mp4.
//!
//! ## What this produces
//!
//! 1920×1080 30fps H.264 high + AAC 320k MP4 with `+faststart` (so YouTube's
//! resumable upload can start streaming before the upload completes).
//!
//! ## Layout (locked per
//! `.claude/projects/J--nightdrive/memory/feedback_twc_3panel_layout_locked.md`)
//!
//! - **y=0..200 top band** — synthwave cover bleeds through, no panels. VT323
//!   title centered + BPM/key subtitle below.
//! - **y=200..820 twin panels** — left RADAR (radar header + inner inset for
//!   the SDXL-rendered map), right 5-DAY FORECAST (header + 5 rows of day +
//!   glyph + current° in white, HI in pink, LO in cyan). Both panels are
//!   78%-alpha dark fill with cyan borders, meeting in the middle at x=960.
//! - **y=820..1020 waveform** — full-width `showwaves` cyan|magenta.
//! - **CTA** bottom-right corner.
//!
//! ## Forecast data
//!
//! Generated deterministically from `track_id` via djb2 hash so re-renders of
//! the same track produce the same forecast. For live data integration see
//! `project_catalog_before_livestream.md` — real NWS feed lands when the
//! channel reaches the 240-minute catalog milestone.

use async_trait::async_trait;
use nightdrive_core::config::EncoderConfig;
use nightdrive_core::{CompositionSpec, NightdriveError, NightdriveResult, TrackPaths};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tracing::{debug, info, instrument, warn};

mod weather;
pub use weather::{
    DayForecast, Forecast, ForecastSource, fetch_or_synthesize, fetch_radar_gif, region_city_for,
};

#[async_trait]
pub trait FinalEncoder: Send + Sync {
    /// Read `paths.master_flac()` + `paths.cover_png()`, write `paths.final_mp4()`.
    /// The `spec` provides the title for the on-screen drawtext overlay.
    async fn compose(&self, paths: &TrackPaths, spec: &CompositionSpec)
        -> NightdriveResult<PathBuf>;
}

#[derive(Debug, Clone)]
pub struct FfmpegEncoder {
    cfg: EncoderConfig,
}

impl FfmpegEncoder {
    pub fn new(cfg: EncoderConfig) -> Self {
        Self { cfg }
    }
}

// =============================================================================
// FfmpegEncoder
// =============================================================================

#[async_trait]
impl FinalEncoder for FfmpegEncoder {
    #[instrument(skip(self, spec), fields(track_root = %paths.root.display(), title = %spec.title))]
    async fn compose(
        &self,
        paths: &TrackPaths,
        spec: &CompositionSpec,
    ) -> NightdriveResult<PathBuf> {
        let cover = paths.cover_png();
        let audio = paths.master_flac();
        let out = paths.final_mp4();

        if !cover.exists() {
            return Err(NightdriveError::Encoder(format!(
                "cover.png missing at {}",
                cover.display()
            )));
        }
        if !audio.exists() {
            return Err(NightdriveError::Encoder(format!(
                "master.flac missing at {}",
                audio.display()
            )));
        }

        let ffmpeg: PathBuf = if self.cfg.ffmpeg_path.as_os_str().is_empty() {
            PathBuf::from("ffmpeg")
        } else {
            self.cfg.ffmpeg_path.clone()
        };

        if !self.cfg.font_path.exists() {
            warn!(
                font_path = %self.cfg.font_path.display(),
                "configured font file missing — overlays will be skipped; \
                 final.mp4 will only have cover+waveform"
            );
        }
        let overlays_ok = self.cfg.font_path.exists();

        // All overlay text gets written to per-track files so we never have
        // to escape arbitrary strings for ffmpeg's filter parser. Subdir
        // keeps paths.root tidy.
        let text_dir = paths.root.join("_text");
        tokio::fs::create_dir_all(&text_dir).await.map_err(|e| NightdriveError::Io {
            path: text_dir.display().to_string(),
            source: e,
        })?;

        // Fetch live forecast (NWS) with deterministic-synthetic fallback.
        // Saved to paths.root/forecast.json as the time-capsule archive — every
        // VOD ends up with a timestamped record of the actual weather at the
        // city we pulled from. Synthetic fallback is fine for offline runs;
        // the .json still records that and exactly what was rendered.
        let forecast_data = fetch_or_synthesize(&spec.track_id).await;
        let forecast_archive = paths.root.join("forecast.json");
        let archive_json = serde_json::to_vec_pretty(&forecast_data).map_err(|e| {
            NightdriveError::Encoder(format!("serialize forecast: {e}"))
        })?;
        tokio::fs::write(&forecast_archive, &archive_json).await.map_err(|e| {
            NightdriveError::Io {
                path: forecast_archive.display().to_string(),
                source: e,
            }
        })?;

        // Pull the NWS Ridge2 animated radar loop for this region and archive
        // it next to forecast.json. Best-effort: if NWS is unreachable or the
        // station is offline, the inset stays as the empty dark-blue
        // placeholder. The negate filter + pillarbox-scale composite happens
        // in the filter graph; here we just download + path-record.
        let radar_archive = paths.root.join("radar.gif");
        let radar_path: Option<PathBuf> =
            match fetch_radar_gif(&forecast_data, &radar_archive, &ffmpeg).await {
                Ok(p) => {
                    info!(
                        archive = %p.display(),
                        region = %forecast_data.region,
                        "radar loop downloaded + archived"
                    );
                    Some(p)
                }
                Err(e) => {
                    warn!(
                        region = %forecast_data.region,
                        error = %e,
                        "NWS radar unreachable — left panel will use the empty dark-blue inset"
                    );
                    None
                }
            };

        info!(
            archive = %forecast_archive.display(),
            source = ?forecast_data.source,
            region = %forecast_data.region,
            cities = forecast_data.cities.len(),
            primary_city = %forecast_data.cities.first().map(|c| c.full_label.as_str()).unwrap_or("?"),
            fetched_at = %forecast_data.fetched_at,
            radar_present = radar_path.is_some(),
            "forecast resolved + archived"
        );

        // Write every overlay file. Layout from feedback_twc_3panel_layout_locked.md.
        let title_path = write_text(&text_dir, "title.txt", &spec.title).await?;
        let subtitle_path = write_text(
            &text_dir,
            "subtitle.txt",
            &format!("{} BPM · {}", spec.bpm, spec.musical_key),
        )
        .await?;
        let radar_header_path = write_text(
            &text_dir,
            "radar_header.txt",
            &format!("RADAR · {}", forecast_data.region),
        )
        .await?;

        // Per-city overlay files. 4 cities × (1 header + 5 fc + 5 hi + 5 lo) = 64
        // text files per render. ffmpeg drawtext layers reference them
        // individually with `enable=between(mod(t,120),slot_start,slot_end)`
        // to cycle every 30s in TWC "Local on the 8s" style.
        let mut city_headers: Vec<PathBuf> = Vec::with_capacity(forecast_data.cities.len());
        let mut city_fc_paths: Vec<Vec<PathBuf>> = Vec::with_capacity(forecast_data.cities.len());
        let mut city_hi_paths: Vec<Vec<PathBuf>> = Vec::with_capacity(forecast_data.cities.len());
        let mut city_lo_paths: Vec<Vec<PathBuf>> = Vec::with_capacity(forecast_data.cities.len());
        let fetched_hhmm_utc = forecast_data.fetched_at.format("%H:%M");
        for (city_idx, city) in forecast_data.cities.iter().enumerate() {
            // Header on the right panel: "5-DAY FORECAST · <CITY> · 14:30 UTC".
            // The trailing UTC timestamp is the time we pulled the NWS data
            // (not the time the forecast was issued — close enough). At
            // fontsize 36 the longest city ("FORT LAUDERDALE") + timestamp
            // measures ~880px which fits in the 920px right-panel space.
            let hdr = write_text(
                &text_dir,
                &format!("fc_header_c{city_idx}.txt"),
                &format!(
                    "5-DAY FORECAST · {} · {} UTC",
                    city.display_name, fetched_hhmm_utc
                ),
            )
            .await?;
            city_headers.push(hdr);

            let mut fc = Vec::with_capacity(city.days.len());
            let mut hi = Vec::with_capacity(city.days.len());
            let mut lo = Vec::with_capacity(city.days.len());
            for (d_idx, day) in city.days.iter().enumerate() {
                fc.push(
                    write_text(
                        &text_dir,
                        &format!("fc_c{city_idx}_d{}.txt", d_idx + 1),
                        &format!("{}   {}  {}°", day.name, day.glyph, day.current),
                    )
                    .await?,
                );
                hi.push(
                    write_text(
                        &text_dir,
                        &format!("hi_c{city_idx}_d{}.txt", d_idx + 1),
                        &format!("HI {}", day.high),
                    )
                    .await?,
                );
                lo.push(
                    write_text(
                        &text_dir,
                        &format!("lo_c{city_idx}_d{}.txt", d_idx + 1),
                        &format!("LO {}", day.low),
                    )
                    .await?,
                );
            }
            city_fc_paths.push(fc);
            city_hi_paths.push(hi);
            city_lo_paths.push(lo);
        }

        let cta_active = !self.cfg.cta_text.trim().is_empty();
        let cta_path = if cta_active {
            Some(write_text(&text_dir, "cta.txt", &self.cfg.cta_text).await?)
        } else {
            None
        };

        let font = ffmpeg_filter_path(&self.cfg.font_path);
        let filter = build_filter_graph(
            overlays_ok,
            &font,
            &title_path,
            &subtitle_path,
            &radar_header_path,
            &city_headers,
            &city_fc_paths,
            &city_hi_paths,
            &city_lo_paths,
            cta_path.as_deref(),
            radar_path.is_some(),
            forecast_data.radar_prestyled,
        );

        let mut cmd = tokio::process::Command::new(&ffmpeg);
        cmd.args(["-y", "-hide_banner", "-nostats"])
            .args(["-loop", "1", "-framerate", "30", "-i"])
            .arg(&cover)
            .arg("-i")
            .arg(&audio);
        // Radar input is conditional — `-stream_loop -1` makes the GIF loop
        // for the full song duration. Index 2 in the filter graph.
        if let Some(ref rp) = radar_path {
            cmd.args(["-stream_loop", "-1", "-i"]).arg(rp);
        }
        cmd.args([
                "-filter_complex",
                &filter,
                "-map",
                "[v]",
                "-map",
                "1:a",
                "-c:v",
                &self.cfg.video_codec,
                "-preset",
                &self.cfg.preset,
                "-crf",
                &self.cfg.crf.to_string(),
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                &self.cfg.audio_codec,
                "-b:a",
                &self.cfg.audio_bitrate,
                "-shortest",
                "-movflags",
                "+faststart",
            ])
            .arg(&out)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        debug!(?cmd, "ffmpeg encode launching");
        info!(
            cover = %cover.display(),
            audio = %audio.display(),
            out = %out.display(),
            region = %forecast_data.region,
            forecast_source = ?forecast_data.source,
            "starting final encode (3-panel TWC layout)"
        );

        let output = cmd
            .output()
            .await
            .map_err(|e| NightdriveError::Encoder(format!("spawn ffmpeg encode: {e}")))?;

        if !output.status.success() {
            let tail = String::from_utf8_lossy(&output.stderr);
            return Err(NightdriveError::Encoder(format!(
                "ffmpeg encode exited {}: {}",
                output.status,
                tail_n_lines(&tail, 30),
            )));
        }

        // Clean up per-track text directory.
        let _ = tokio::fs::remove_dir_all(&text_dir).await;

        let meta = tokio::fs::metadata(&out).await.map_err(|e| NightdriveError::Io {
            path: out.display().to_string(),
            source: e,
        })?;
        info!(bytes = meta.len(), out = %out.display(), "final.mp4 written");
        Ok(out)
    }
}

/// One city's 30-second slot in the forecast rotation. With 4 cities cycling
/// the total loop is 120s, which loops 2-3 times per typical 240-360s track.
const CYCLE_SECONDS: f64 = 120.0;
const SLOT_SECONDS: f64 = 30.0;

#[allow(clippy::too_many_arguments)]
fn build_filter_graph(
    overlays_ok: bool,
    font: &str,
    title_path: &Path,
    subtitle_path: &Path,
    radar_header_path: &Path,
    city_headers: &[PathBuf],
    city_fc_paths: &[Vec<PathBuf>],
    city_hi_paths: &[Vec<PathBuf>],
    city_lo_paths: &[Vec<PathBuf>],
    cta_path: Option<&Path>,
    radar_present: bool,
    radar_prestyled: bool,
) -> String {
    // Base layers: showwaves → cover scale/crop → panel boxes → seam → radar
    // inner box → radar gif overlay (if present) → waveform overlay → drawtexts.
    let mut graph = String::with_capacity(4096);
    graph.push_str(
        "[1:a]showwaves=size=1920x200:mode=line:colors=cyan|magenta,\
         format=yuva420p,colorchannelmixer=aa=0.8[wave];\
         [0:v]scale=1920:1080:force_original_aspect_ratio=increase,\
         crop=1920:1080,format=yuv420p[bg];\
         [bg]drawbox=x=0:y=200:w=960:h=620:color=0x0a0a18@0.78:t=fill[bg_l1];\
         [bg_l1]drawbox=x=960:y=200:w=960:h=620:color=0x0a0a18@0.78:t=fill[bg_l2];\
         [bg_l2]drawbox=x=0:y=200:w=1920:h=620:color=0x00FFFF@0.7:t=4[bg_outer];\
         [bg_outer]drawbox=x=956:y=200:w=8:h=620:color=0x00FFFF@0.7:t=fill[bg_seam];\
         [bg_seam]drawbox=x=40:y=300:w=880:h=480:color=0x0e2a4d@0.85:t=fill[bg_radar]",
    );

    // Radar gif overlay onto the inner inset. The chain:
    //   1. format=rgba — ensure an alpha channel before chromakey writes to it
    //   2. chromakey color=0xC2EAF0 — kill the NWS basemap's pale-cyan WATER
    //      fill (#C2EAF0 measured = 59% of pixels in the source GIF). This
    //      becomes a peach-orange after `negate` and clashes with the dark
    //      navy synthwave inset; chromakey-then-overlay shows the inset
    //      underneath instead. similarity=0.12 is tight enough that the
    //      cyan precip blobs (different saturation/hue) are not keyed.
    //   3. negate — inverts the remaining RGB (white land → dark, dark
    //      outlines → bright, precip oranges/reds → magentas/cyans).
    //      Alpha is preserved (the keyed water stays transparent).
    //   4. scale=-1:480 — preserves aspect, pillarbox-centers at x=218
    //      (=(880-524)/2+40).
    if radar_present {
        if radar_prestyled {
            // RainViewer path: the GIF is already a dark synthwave map with
            // magenta precip (composited in weather::build_rainviewer_gif).
            // Skip chromakey+negate — just scale the 512² tile to the inset
            // height and center it (square → x=(880-480)/2+40=240).
            graph.push_str(
                ";[2:v]format=rgba,scale=-1:480[radar_scaled];\
                 [bg_radar][radar_scaled]overlay=x=240:y=300:shortest=0[bg_with_radar];\
                 [bg_with_radar][wave]overlay=x=0:y=820:format=yuv420[v_base]",
            );
        } else {
            // NWS Ridge2 path: light basemap + colored precip. Key the
            // pale-cyan water, negate to dark land + magenta/cyan precip.
            graph.push_str(
                ";[2:v]format=rgba,chromakey=color=0xC2EAF0:similarity=0.12:blend=0.04,\
                      negate,scale=-1:480[radar_scaled];\
                 [bg_radar][radar_scaled]overlay=x=218:y=300:shortest=0[bg_with_radar];\
                 [bg_with_radar][wave]overlay=x=0:y=820:format=yuv420[v_base]",
            );
        }
    } else {
        graph.push_str(";[bg_radar][wave]overlay=x=0:y=820:format=yuv420[v_base]");
    }

    if !overlays_ok {
        graph.push_str(";[v_base]copy[v]");
        return graph;
    }

    let mut prev = "v_base".to_string();
    let mut layer_idx = 0usize;
    let mut add = |graph: &mut String, prev: &mut String, idx: &mut usize, dt: String| {
        *idx += 1;
        let next = format!("v{}", *idx);
        graph.push(';');
        graph.push('[');
        graph.push_str(prev);
        graph.push(']');
        graph.push_str(&dt);
        graph.push('[');
        graph.push_str(&next);
        graph.push(']');
        *prev = next;
    };

    // Title: white VT323 80px, y=30, heavy shadow + thick border.
    add(
        &mut graph,
        &mut prev,
        &mut layer_idx,
        drawtext_layer(font, title_path, "white", 80, "(w-text_w)/2", "30")
            + ":borderw=5:bordercolor=black:shadowcolor=black@0.9:shadowx=8:shadowy=8",
    );
    // Subtitle: cyan 40px, y=130.
    add(
        &mut graph,
        &mut prev,
        &mut layer_idx,
        drawtext_layer(font, subtitle_path, "0x00FFFF", 40, "(w-text_w)/2", "130")
            + ":borderw=3:bordercolor=black:shadowcolor=black@0.9:shadowx=5:shadowy=5",
    );
    // Radar panel header (static — radar IS regional).
    add(
        &mut graph,
        &mut prev,
        &mut layer_idx,
        drawtext_layer(font, radar_header_path, "0x00FFFF", 36, "40", "240")
            + ":borderw=2:bordercolor=black",
    );
    // 4-city cycling forecast panel: each city gets a 30s slot, total cycle
    // 120s, loops for the song length. `enable=between(mod(t,120),a,b)`
    // makes a drawtext visible only when current time mod 120 is in [a,b).
    for (city_idx, header) in city_headers.iter().enumerate() {
        let slot_start = (city_idx as f64) * SLOT_SECONDS;
        let slot_end = slot_start + SLOT_SECONDS;
        let enable_expr = format!(
            ":enable='between(mod(t\\,{cycle:.0})\\,{a:.0}\\,{b:.0})'",
            cycle = CYCLE_SECONDS,
            a = slot_start,
            b = slot_end,
        );
        // Header for this city (cycles to match the forecast rows).
        add(
            &mut graph,
            &mut prev,
            &mut layer_idx,
            drawtext_layer(font, header, "0x00FFFF", 36, "1000", "240")
                + ":borderw=2:bordercolor=black"
                + &enable_expr,
        );
        let fc_paths = &city_fc_paths[city_idx];
        let hi_paths = &city_hi_paths[city_idx];
        let lo_paths = &city_lo_paths[city_idx];
        for (i, ((fc, hi), lo)) in fc_paths.iter().zip(hi_paths).zip(lo_paths).enumerate() {
            let y = 320 + (i as i32) * 90;
            let y_str = y.to_string();
            add(
                &mut graph,
                &mut prev,
                &mut layer_idx,
                drawtext_layer(font, fc, "white", 48, "1000", &y_str)
                    + ":borderw=2:bordercolor=black"
                    + &enable_expr,
            );
            add(
                &mut graph,
                &mut prev,
                &mut layer_idx,
                drawtext_layer(font, hi, "0xFF66CC", 48, "1370", &y_str)
                    + ":borderw=2:bordercolor=black"
                    + &enable_expr,
            );
            add(
                &mut graph,
                &mut prev,
                &mut layer_idx,
                drawtext_layer(font, lo, "0x00FFFF", 48, "1620", &y_str)
                    + ":borderw=2:bordercolor=black"
                    + &enable_expr,
            );
        }
    }
    if let Some(p) = cta_path {
        add(
            &mut graph,
            &mut prev,
            &mut layer_idx,
            drawtext_layer(font, p, "0x00FFFF", 44, "w-text_w-60", "h-text_h-40")
                + ":borderw=3:bordercolor=black:shadowcolor=black@0.9:shadowx=5:shadowy=5",
        );
    }

    // Rename the last labeled output to [v] for the -map.
    graph.push_str(&format!(";[{prev}]copy[v]"));
    graph
}

fn drawtext_layer(
    font: &str,
    textfile: &Path,
    color: &str,
    fontsize: u32,
    x: &str,
    y: &str,
) -> String {
    let textfile_str = ffmpeg_filter_path(textfile);
    format!(
        "drawtext=fontfile='{font}':textfile='{textfile_str}':fontcolor={color}:\
         fontsize={fontsize}:x={x}:y={y}"
    )
}

async fn write_text(dir: &Path, name: &str, content: &str) -> NightdriveResult<PathBuf> {
    let path = dir.join(name);
    tokio::fs::write(&path, content).await.map_err(|e| NightdriveError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    Ok(path)
}

fn tail_n_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

/// Format a Path for the inside of an ffmpeg filter graph string. ffmpeg
/// treats `:` and `\` specially inside filter args, so we flip backslashes
/// to forward slashes (still works on Windows) and escape the drive letter's
/// colon. Wrap the result in single quotes when interpolating.
fn ffmpeg_filter_path(p: &Path) -> String {
    p.to_string_lossy()
        .replace('\\', "/")
        .replace(':', "\\:")
}

/// Generate a thumbnail (cover.png copy) at `paths.thumbnail_jpg()`. YouTube
/// thumbnails are JPEG ≤2 MB; cover.png is typically PNG well over that for
/// 1024×1024 album art. This re-encodes via ffmpeg to JPEG at quality 90.
#[instrument(fields(track_root = %paths.root.display()))]
pub async fn make_thumbnail(paths: &TrackPaths) -> NightdriveResult<PathBuf> {
    let cover = paths.cover_png();
    let thumb = paths.thumbnail_jpg();
    let output = tokio::process::Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-nostats", "-i"])
        .arg(&cover)
        .args(["-q:v", "2"])
        .arg(&thumb)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| NightdriveError::Encoder(format!("spawn thumbnail re-encode: {e}")))?;
    if !output.status.success() {
        let tail = String::from_utf8_lossy(&output.stderr);
        return Err(NightdriveError::Encoder(format!(
            "thumbnail encode failed: {}",
            tail_n_lines(&tail, 20),
        )));
    }
    Ok(thumb)
}

// Encoder-level forecast tests live in weather.rs (synthetic determinism,
// glyph mapping, etc.); the integration test for forecast.json archive
// generation belongs in tests/witnesses/ once we have a per-stage witness
// for stage 6 — not blocking the rewrite.
