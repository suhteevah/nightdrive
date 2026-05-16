//! nightdrive-stems — split a mastered audio file into per-instrument stems
//! via Demucs (Facebook Research's hybrid transformer source separator).
//!
//! Stems unlock three downstream artifacts:
//!
//! 1. **DAW-friendly remix kits** — drop the 4 stems into Reaper / Ableton,
//!    re-mix, alternative arrangements, post-release remasters.
//! 2. **MIDI transcription** — per-stem audio-to-MIDI is much higher quality
//!    than transcribing a mastered mix (basic-pitch / MT3 work best on
//!    single-timbre input).
//! 3. **Vocal-presence QC** — for instrumental tracks the `vocals.wav` should
//!    be near-silence. Non-trivial energy in it means the audio-gen engine
//!    hallucinated vocals → audit catches the regression.
//!
//! Demucs is shelled out as a subprocess rather than wrapped in an HTTP
//! sidecar: we run it once per track post-mastering, no need for a daemon.
//! `htdemucs_ft` (fine-tuned hybrid transformer) is the SOTA checkpoint at
//! ~9.0 dB SDR on MUSDB-HQ; we default to it but expose the model name as
//! config.

use async_trait::async_trait;
use nightdrive_core::{NightdriveError, NightdriveResult, TrackPaths};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, info, instrument, warn};

/// Configuration for the Demucs CLI shell-out. Loaded from `[stems]` in the
/// nightdrive config TOML; sensible defaults match a CPU-or-GPU Demucs 4
/// install via `pip install demucs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StemsConfig {
    /// Path to the demucs executable. `"demucs"` works when it's on $PATH
    /// (typical when installed in the synthwave-gen / acestep venv with
    /// the venv's Scripts/bin dir on PATH).
    #[serde(default = "default_demucs_path")]
    pub demucs_path: String,

    /// Demucs model name. `htdemucs_ft` (default) is the fine-tuned hybrid
    /// transformer, the current SOTA. `htdemucs` is the standard checkpoint
    /// (faster, ~0.2 dB worse SDR). `mdx_extra` is a different family with
    /// good vocal isolation.
    #[serde(default = "default_demucs_model")]
    pub model: String,

    /// Device for inference: `"cuda"` or `"cpu"`. Demucs falls back to CPU
    /// automatically if CUDA isn't available, but explicit is better.
    #[serde(default = "default_demucs_device")]
    pub device: String,

    /// Per-track timeout for the demucs subprocess. Demucs on a 4-min track
    /// takes ~30s on a 3070 Ti or ~5 min on CPU. 600s covers both with margin.
    #[serde(default = "default_demucs_timeout")]
    pub timeout_seconds: u64,

    /// Optional explicit shift augmentation count for higher-quality output
    /// (`--shifts N`). 0 = off (default, fastest). 5-10 = best quality at
    /// linear cost. Demucs's `htdemucs_ft` -n + --shifts 2 is a common
    /// "release-quality" setting.
    #[serde(default)]
    pub shifts: u32,
}

fn default_demucs_path() -> String { "demucs".to_string() }
fn default_demucs_model() -> String { "htdemucs_ft".to_string() }
fn default_demucs_device() -> String { "cuda".to_string() }
fn default_demucs_timeout() -> u64 { 600 }

impl Default for StemsConfig {
    fn default() -> Self {
        Self {
            demucs_path: default_demucs_path(),
            model: default_demucs_model(),
            device: default_demucs_device(),
            timeout_seconds: default_demucs_timeout(),
            shifts: 0,
        }
    }
}

/// Output paths from a successful 4-stem separation. Demucs emits these as
/// 4 separate WAV files under a model-named subdirectory; we normalize
/// them to `<track_root>/stems/<stem>.wav` after the process exits.
#[derive(Debug, Clone)]
pub struct StemPaths {
    pub drums: PathBuf,
    pub bass: PathBuf,
    pub vocals: PathBuf,
    pub other: PathBuf,
}

impl StemPaths {
    pub fn new(track_root: &Path) -> Self {
        let dir = track_root.join("stems");
        Self {
            drums: dir.join("drums.wav"),
            bass: dir.join("bass.wav"),
            vocals: dir.join("vocals.wav"),
            other: dir.join("other.wav"),
        }
    }

    pub fn all(&self) -> [&PathBuf; 4] {
        [&self.drums, &self.bass, &self.vocals, &self.other]
    }
}

#[async_trait]
pub trait StemSeparator: Send + Sync {
    /// Split the mastered FLAC at `paths.master_flac()` into 4 stems and
    /// place them under `paths.root.join("stems")/`. Returns the canonical
    /// stem paths on success. On per-track failure returns
    /// [`NightdriveError::AudioMaster`] — stems failures shouldn't kill
    /// the rest of the pipeline since the master.flac is the canonical
    /// product.
    async fn separate(&self, paths: &TrackPaths) -> NightdriveResult<StemPaths>;
}

/// Demucs CLI wrapper. Shells out to `demucs` and normalizes the output
/// layout to the canonical `<track_root>/stems/<stem>.wav` shape.
#[derive(Debug, Clone)]
pub struct DemucsCli {
    cfg: StemsConfig,
}

impl DemucsCli {
    pub fn new(cfg: StemsConfig) -> Self {
        Self { cfg }
    }
}

#[async_trait]
impl StemSeparator for DemucsCli {
    #[instrument(
        skip_all,
        fields(
            input = %paths.master_flac().display(),
            model = %self.cfg.model,
            device = %self.cfg.device,
        )
    )]
    async fn separate(&self, paths: &TrackPaths) -> NightdriveResult<StemPaths> {
        let input = paths.master_flac();
        if !input.exists() {
            return Err(NightdriveError::AudioMaster(format!(
                "stems: input master.flac missing at {}",
                input.display()
            )));
        }

        let stems_dir = paths.root.join("stems");
        tokio::fs::create_dir_all(&stems_dir).await.map_err(|e| NightdriveError::Io {
            path: stems_dir.display().to_string(),
            source: e,
        })?;

        // Demucs writes to <out_dir>/<model_name>/<input_basename>/{drums,bass,vocals,other}.wav
        // We pass `stems_dir` as `-o`, so the intermediate path is:
        //   stems_dir / <model_name> / <input_basename>/*.wav
        // We then move those into `stems_dir/*.wav` and clean up the nesting.
        let mut cmd = Command::new(&self.cfg.demucs_path);
        cmd.arg("-n").arg(&self.cfg.model);
        cmd.arg("-o").arg(&stems_dir);
        cmd.arg("--device").arg(&self.cfg.device);
        if self.cfg.shifts > 0 {
            cmd.arg("--shifts").arg(self.cfg.shifts.to_string());
        }
        cmd.arg(&input);

        info!("spawning demucs subprocess");
        let start = std::time::Instant::now();
        let output = tokio::time::timeout(
            Duration::from_secs(self.cfg.timeout_seconds),
            cmd.output(),
        )
        .await
        .map_err(|_| {
            NightdriveError::AudioMaster(format!(
                "demucs timed out after {}s",
                self.cfg.timeout_seconds
            ))
        })?
        .map_err(|e| NightdriveError::AudioMaster(format!("demucs spawn: {e}")))?;

        let elapsed = start.elapsed().as_secs_f32();
        debug!(
            elapsed_s = elapsed,
            status = ?output.status,
            stdout_bytes = output.stdout.len(),
            stderr_bytes = output.stderr.len(),
            "demucs subprocess exited"
        );

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(NightdriveError::AudioMaster(format!(
                "demucs exit {:?}: {}",
                output.status.code(),
                stderr.trim()
            )));
        }

        // Find the model-nested output dir and flatten.
        let input_stem = input.file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| NightdriveError::AudioMaster(
                "stems: master.flac has no usable file_stem".into()
            ))?;
        let nested = stems_dir.join(&self.cfg.model).join(input_stem);

        let stem_paths = StemPaths::new(&paths.root);
        let nested_paths = [
            (&stem_paths.drums,  nested.join("drums.wav")),
            (&stem_paths.bass,   nested.join("bass.wav")),
            (&stem_paths.vocals, nested.join("vocals.wav")),
            (&stem_paths.other,  nested.join("other.wav")),
        ];
        for (final_path, nested_path) in nested_paths {
            if !nested_path.exists() {
                return Err(NightdriveError::AudioMaster(format!(
                    "demucs ran but expected stem missing: {}",
                    nested_path.display()
                )));
            }
            // Move (rename) — same filesystem so this is cheap.
            tokio::fs::rename(&nested_path, final_path).await.map_err(|e| NightdriveError::Io {
                path: final_path.display().to_string(),
                source: e,
            })?;
        }

        // Best-effort: remove the now-empty <model>/<input_basename>/ dirs.
        let _ = tokio::fs::remove_dir(&nested).await;
        let _ = tokio::fs::remove_dir(stems_dir.join(&self.cfg.model)).await;

        info!(
            elapsed_s = elapsed,
            drums = %stem_paths.drums.display(),
            bass = %stem_paths.bass.display(),
            vocals = %stem_paths.vocals.display(),
            other = %stem_paths.other.display(),
            "stems written"
        );

        // Lightweight vocal-presence QC for instrumental tracks. If the
        // vocals stem is suspiciously large (>10% of master.flac), warn —
        // the audio-gen engine likely hallucinated singing.
        if let (Ok(vocals_meta), Ok(master_meta)) = (
            tokio::fs::metadata(&stem_paths.vocals).await,
            tokio::fs::metadata(&input).await,
        ) {
            let ratio = vocals_meta.len() as f64 / master_meta.len() as f64;
            if ratio > 0.10 {
                warn!(
                    vocals_bytes = vocals_meta.len(),
                    master_bytes = master_meta.len(),
                    ratio = ratio,
                    "vocals stem is suspiciously large for an instrumental track — model may have hallucinated singing"
                );
            }
        }

        Ok(stem_paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stem_paths_layout_is_canonical() {
        let root = Path::new("/tmp/nd/tracks/nd-20260516-001");
        let p = StemPaths::new(root);
        assert_eq!(p.drums, root.join("stems").join("drums.wav"));
        assert_eq!(p.bass, root.join("stems").join("bass.wav"));
        assert_eq!(p.vocals, root.join("stems").join("vocals.wav"));
        assert_eq!(p.other, root.join("stems").join("other.wav"));
        assert_eq!(p.all().len(), 4);
    }

    #[test]
    fn default_config_picks_htdemucs_ft_on_cuda() {
        let cfg = StemsConfig::default();
        assert_eq!(cfg.model, "htdemucs_ft");
        assert_eq!(cfg.device, "cuda");
        assert_eq!(cfg.shifts, 0);
    }
}
