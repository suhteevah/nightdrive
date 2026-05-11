//! nightdrive-audio-master — ffmpeg loudnorm two-pass + fades + dual export.
//!
//! Reads `raw.wav` from `nightdrive-audio-gen`, runs the ffmpeg `loudnorm`
//! filter in two passes to hit the [`MasteringConfig::target_lufs`] / `true_peak`
//! / `loudness_range` targets, applies the configured fade-in/out, and writes
//! both `master.flac` (lossless intermediate the encoder consumes) and
//! `master.mp3` (CBR 320k fallback for any path that wants a single-file copy).
//!
//! ## Why two passes
//!
//! ffmpeg's `loudnorm` is the de-facto standard for YouTube/Spotify-grade
//! mastering. Pass 1 analyzes the input and emits measured LUFS + peak + LRA
//! to stderr as a JSON-shaped block; pass 2 uses those measurements as
//! `measured_*` inputs so the filter can hit the target on the first try
//! without the conservative attack/release of single-pass mode. Skipping
//! pass 1 produces audibly different output ~30% of the time.

use async_trait::async_trait;
use nightdrive_core::config::MasteringConfig;
use nightdrive_core::{NightdriveError, NightdriveResult, TrackPaths};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tracing::{debug, info, instrument};

#[async_trait]
pub trait AudioMaster: Send + Sync {
    /// Master the file at `paths.raw_audio_wav()` into `paths.master_flac()`
    /// (and `paths.master_mp3()`). Returns the FLAC path on success.
    async fn run(&self, paths: &TrackPaths) -> NightdriveResult<PathBuf>;
}

#[derive(Debug, Clone)]
pub struct FfmpegMaster {
    cfg: MasteringConfig,
    ffmpeg_path: PathBuf,
}

impl FfmpegMaster {
    pub fn new(cfg: MasteringConfig) -> Self {
        Self { cfg, ffmpeg_path: PathBuf::from("ffmpeg") }
    }

    /// Override the ffmpeg binary path (defaults to "ffmpeg" which resolves
    /// via PATH). Useful for tests / non-standard installs.
    pub fn with_ffmpeg(mut self, path: impl Into<PathBuf>) -> Self {
        self.ffmpeg_path = path.into();
        self
    }

    /// Pass 1: run loudnorm in measurement mode, parse the JSON block ffmpeg
    /// prints to stderr.
    #[instrument(skip(self), fields(input = %input.display()))]
    async fn measure(&self, input: &Path) -> NightdriveResult<LoudnormMeasured> {
        // print_format=json makes ffmpeg emit the measurement struct as the
        // last lines of stderr — much easier to parse than the default
        // "key=value\n" dump.
        let filter = format!(
            "loudnorm=I={i}:TP={tp}:LRA={lra}:print_format=json",
            i = self.cfg.target_lufs,
            tp = self.cfg.true_peak_db,
            lra = self.cfg.loudness_range,
        );
        let mut cmd = tokio::process::Command::new(&self.ffmpeg_path);
        cmd.args(["-hide_banner", "-nostats", "-i"])
            .arg(input)
            .args(["-af", &filter, "-f", "null", "-"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        debug!("ffmpeg pass 1 (measure) launching");
        let mut child = cmd.spawn().map_err(|e| {
            NightdriveError::AudioMaster(format!("spawn ffmpeg pass 1: {e}"))
        })?;
        let mut stderr_buf = Vec::with_capacity(8192);
        if let Some(mut stderr) = child.stderr.take() {
            stderr
                .read_to_end(&mut stderr_buf)
                .await
                .map_err(|e| NightdriveError::AudioMaster(format!("read pass 1 stderr: {e}")))?;
        }
        let status = child
            .wait()
            .await
            .map_err(|e| NightdriveError::AudioMaster(format!("await pass 1: {e}")))?;
        if !status.success() {
            let tail = String::from_utf8_lossy(&stderr_buf);
            return Err(NightdriveError::AudioMaster(format!(
                "ffmpeg pass 1 exited {status}: {}",
                tail_n_lines(&tail, 20),
            )));
        }

        let stderr_str = String::from_utf8_lossy(&stderr_buf);
        let measured = parse_loudnorm_json(&stderr_str).ok_or_else(|| {
            NightdriveError::AudioMaster(format!(
                "loudnorm pass 1 emitted no JSON block; tail:\n{}",
                tail_n_lines(&stderr_str, 30)
            ))
        })?;
        debug!(?measured, "pass 1 measurement");
        Ok(measured)
    }

    /// Pass 2: apply the measurement as the `measured_*` params and write the
    /// mastered FLAC + MP3.
    #[instrument(
        skip(self, measured),
        fields(
            input = %input.display(),
            flac = %flac_out.display(),
            mp3 = %mp3_out.display(),
        )
    )]
    async fn apply(
        &self,
        input: &Path,
        flac_out: &Path,
        mp3_out: &Path,
        measured: &LoudnormMeasured,
    ) -> NightdriveResult<()> {
        // We need the input duration to position the fade-out's start time.
        // Cheaper than spawning ffprobe.
        let duration = probe_duration_seconds(&self.ffmpeg_path, input).await?;
        let fade_out_start = (duration - self.cfg.fade_out_seconds).max(0.0);

        let filter = format!(
            "loudnorm=I={i}:TP={tp}:LRA={lra}:measured_I={mi}:measured_TP={mtp}:\
             measured_LRA={mlra}:measured_thresh={mt}:offset={off}:linear=true,\
             afade=t=in:st=0:d={fade_in},afade=t=out:st={fade_out_start}:d={fade_out}",
            i = self.cfg.target_lufs,
            tp = self.cfg.true_peak_db,
            lra = self.cfg.loudness_range,
            mi = measured.input_i,
            mtp = measured.input_tp,
            mlra = measured.input_lra,
            mt = measured.input_thresh,
            off = measured.target_offset,
            fade_in = self.cfg.fade_in_seconds,
            fade_out_start = fade_out_start,
            fade_out = self.cfg.fade_out_seconds,
        );
        debug!(%filter, duration, fade_out_start, "ffmpeg pass 2 filter graph");

        // FLAC pass
        let mut cmd = tokio::process::Command::new(&self.ffmpeg_path);
        cmd.args(["-y", "-hide_banner", "-nostats", "-i"])
            .arg(input)
            .args(["-af", &filter, "-c:a", "flac", "-compression_level", "5"])
            .arg(flac_out)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        run_capture(cmd, "ffmpeg pass 2 (flac)").await?;

        // MP3 pass — re-encode from the FLAC we just wrote (already mastered)
        // rather than re-running loudnorm. CBR 320k for "single-file" listeners.
        let mut cmd = tokio::process::Command::new(&self.ffmpeg_path);
        cmd.args(["-y", "-hide_banner", "-nostats", "-i"])
            .arg(flac_out)
            .args(["-c:a", "libmp3lame", "-b:a", "320k"])
            .arg(mp3_out)
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        run_capture(cmd, "ffmpeg mp3 export").await?;
        Ok(())
    }
}

#[async_trait]
impl AudioMaster for FfmpegMaster {
    #[instrument(skip(self), fields(track_root = %paths.root.display()))]
    async fn run(&self, paths: &TrackPaths) -> NightdriveResult<PathBuf> {
        let input = paths.raw_audio_wav();
        let flac_out = paths.master_flac();
        let mp3_out = paths.master_mp3();

        info!(input = %input.display(), "starting two-pass master");
        let measured = self.measure(&input).await?;
        info!(
            input_i = measured.input_i,
            input_tp = measured.input_tp,
            input_lra = measured.input_lra,
            "pass 1 measurement complete",
        );
        self.apply(&input, &flac_out, &mp3_out, &measured).await?;
        info!(
            flac = %flac_out.display(),
            mp3 = %mp3_out.display(),
            "master written",
        );
        Ok(flac_out)
    }
}

// =============================================================================
// loudnorm JSON parsing
// =============================================================================

#[derive(Debug, Clone, Deserialize)]
pub struct LoudnormMeasured {
    #[serde(rename = "input_i", deserialize_with = "deser_f32_str")]
    pub input_i: f32,
    #[serde(rename = "input_tp", deserialize_with = "deser_f32_str")]
    pub input_tp: f32,
    #[serde(rename = "input_lra", deserialize_with = "deser_f32_str")]
    pub input_lra: f32,
    #[serde(rename = "input_thresh", deserialize_with = "deser_f32_str")]
    pub input_thresh: f32,
    #[serde(rename = "target_offset", deserialize_with = "deser_f32_str")]
    pub target_offset: f32,
}

fn deser_f32_str<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f32, D::Error> {
    use serde::Deserialize;
    let s = String::deserialize(d)?;
    s.trim().parse::<f32>().map_err(serde::de::Error::custom)
}

/// Find the trailing JSON object in ffmpeg stderr from `loudnorm=print_format=json`.
/// ffmpeg emits a single `{ ... }` block at the very end; this finds the last
/// matching pair.
pub fn parse_loudnorm_json(stderr: &str) -> Option<LoudnormMeasured> {
    let last_open = stderr.rfind('{')?;
    let last_close = stderr.rfind('}')?;
    if last_close < last_open {
        return None;
    }
    let block = &stderr[last_open..=last_close];
    serde_json::from_str::<LoudnormMeasured>(block).ok()
}

// =============================================================================
// helpers
// =============================================================================

async fn run_capture(mut cmd: tokio::process::Command, label: &str) -> NightdriveResult<()> {
    let output = cmd
        .output()
        .await
        .map_err(|e| NightdriveError::AudioMaster(format!("{label} spawn: {e}")))?;
    if !output.status.success() {
        let tail = String::from_utf8_lossy(&output.stderr);
        return Err(NightdriveError::AudioMaster(format!(
            "{label} exited {}: {}",
            output.status,
            tail_n_lines(&tail, 20),
        )));
    }
    Ok(())
}

fn tail_n_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

async fn probe_duration_seconds(ffmpeg: &Path, input: &Path) -> NightdriveResult<f32> {
    // `ffmpeg -i <input>` exits non-zero (no output specified) but always prints
    // `Duration: HH:MM:SS.MS` on stderr. Cheap and avoids an ffprobe dep.
    let output = tokio::process::Command::new(ffmpeg)
        .args(["-hide_banner", "-i"])
        .arg(input)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| NightdriveError::AudioMaster(format!("probe spawn: {e}")))?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_duration_seconds(&stderr).ok_or_else(|| {
        NightdriveError::AudioMaster(format!(
            "couldn't parse Duration from ffmpeg stderr:\n{}",
            tail_n_lines(&stderr, 10)
        ))
    })
}

pub fn parse_duration_seconds(stderr: &str) -> Option<f32> {
    let needle = "Duration: ";
    let i = stderr.find(needle)?;
    let after = &stderr[i + needle.len()..];
    let end = after.find(',').unwrap_or(after.len());
    let span = after[..end].trim();
    let parts: Vec<&str> = span.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let h: f32 = parts[0].parse().ok()?;
    let m: f32 = parts[1].parse().ok()?;
    let s: f32 = parts[2].parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_loudnorm_picks_trailing_json() {
        let stderr = r#"
[Parsed_loudnorm_0 @ 0x7f8c4c0010c0] starting analysis...
some other noise
{
    "input_i" : "-15.30",
    "input_tp" : "-1.05",
    "input_lra" : "10.20",
    "input_thresh" : "-25.50",
    "output_i" : "-14.00",
    "output_tp" : "-1.00",
    "output_lra" : "10.10",
    "output_thresh" : "-24.20",
    "normalization_type" : "dynamic",
    "target_offset" : "1.30"
}
"#;
        let m = parse_loudnorm_json(stderr).expect("must parse trailing JSON");
        assert!((m.input_i - -15.30).abs() < 0.01);
        assert!((m.input_tp - -1.05).abs() < 0.01);
        assert!((m.target_offset - 1.30).abs() < 0.01);
    }

    #[test]
    fn parse_duration_picks_first_match() {
        let stderr = r#"
Input #0, wav, from 'raw.wav':
  Metadata:
    encoder         : nightdrive-audio-gen 0.1
  Duration: 00:04:00.05, bitrate: 1411 kb/s
"#;
        let d = parse_duration_seconds(stderr).expect("must parse Duration line");
        assert!((d - 240.05).abs() < 0.01, "got {}", d);
    }
}
