//! Typed runtime configuration. Loads from a TOML file (path in
//! `NIGHTDRIVE_CONFIG` env var or `./config/nightdrive.toml`), with env-var
//! overrides for secret-bearing fields.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub paths: PathsConfig,
    pub openclaw: OpenclawConfig,
    pub audio_gen: AudioGenConfig,
    pub art: ArtConfig,
    pub mastering: MasteringConfig,
    pub visualizer: VisualizerConfig,
    pub encoder: EncoderConfig,
    pub youtube: YoutubeConfig,
    pub livestream: LivestreamConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PathsConfig {
    pub work_dir: PathBuf,
    pub sqlite_db: PathBuf,
    pub tracks_dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenclawConfig {
    pub base_url: String,
    pub model: String,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_llm_timeout")]
    pub timeout_seconds: u64,
}
fn default_temperature() -> f32 { 0.85 }
fn default_max_tokens() -> u32 { 2048 }
fn default_llm_timeout() -> u64 { 120 }

#[derive(Debug, Clone, Deserialize)]
pub struct AudioGenConfig {
    pub base_url: String,
    pub model: String,
    pub sample_rate: u32,
    pub channels: u8,
    pub segment_seconds: u32,
    pub overlap_seconds: u32,
    #[serde(default = "default_guidance_scale")]
    pub guidance_scale: f32,
    /// Which audio-gen engine the sidecar at `base_url` implements. Drives
    /// which AudioGenerator impl the orchestrator picks. Allowed values:
    ///   - `"stable_audio"` (default; commercial-license-safe, blind crossfade)
    ///   - `"musicgen"` (CC-BY-NC, native audio continuation, seamless chain)
    /// Older configs without this field default to `"stable_audio"`.
    #[serde(default = "default_engine")]
    pub engine: String,
    /// Continuation-only: how many seconds of accumulated audio to send back
    /// to the MusicGen sidecar as `prev_audio_b64` prefix. 5s is the audiocraft
    /// default + reasonable balance between context (helps continuity) and
    /// throughput (sending more makes each call slower). Ignored by SAO.
    #[serde(default = "default_continuation_prefix")]
    pub continuation_prefix_seconds: f32,
}
fn default_guidance_scale() -> f32 { 3.0 }
fn default_engine() -> String { "stable_audio".to_string() }
fn default_continuation_prefix() -> f32 { 5.0 }

#[derive(Debug, Clone, Deserialize)]
pub struct ArtConfig {
    pub base_url: String,
    pub model: String,
    pub width: u32,
    pub height: u32,
    pub steps: u32,
    pub cfg_scale: f32,
    pub negative_prompt: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MasteringConfig {
    pub target_lufs: f32,
    pub true_peak_db: f32,
    pub loudness_range: f32,
    pub fade_in_seconds: f32,
    pub fade_out_seconds: f32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VisualizerConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub quality_preset: String,
    pub seed_from_track_id: bool,
    pub include_code_scroll: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EncoderConfig {
    pub ffmpeg_path: PathBuf,
    pub video_codec: String,
    pub crf: u8,
    pub preset: String,
    pub audio_codec: String,
    pub audio_bitrate: String,
    pub intro_seconds: u32,
    pub outro_seconds: u32,
    /// Path to the TTF/OTF used by the title + CTA + subtitle drawtext overlays.
    /// Defaults to VT323 (the CRT terminal pixel font that ships with
    /// nightdrive under `assets/fonts/`). Swap for any TTF you prefer —
    /// Cascadia Mono / Press Start 2P / Audiowide are all reasonable
    /// alternatives. The default is OFL-licensed (commercial use OK).
    #[serde(default = "default_font_path")]
    pub font_path: PathBuf,
    /// Static lower-right CTA shown across the whole video. Per-track title
    /// is overlaid separately. Set to empty string in config to disable.
    #[serde(default = "default_cta_text")]
    pub cta_text: String,
}
fn default_font_path() -> PathBuf { PathBuf::from("assets/fonts/VT323-Regular.ttf") }
fn default_cta_text() -> String { "LIKE • SUBSCRIBE".to_string() }

#[derive(Debug, Clone, Deserialize)]
pub struct YoutubeConfig {
    pub default_privacy: String,
    pub default_category_id: String,
    pub schedule_offset_hours: i64,
    pub publish_window_start_hour: u32,
    pub publish_window_end_hour: u32,
    pub declare_synthetic_content: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LivestreamConfig {
    pub visualizer_ws_port: u16,
    pub metadata_refresh_seconds: u32,
    pub shuffle_buffer_size: u32,
    pub min_replay_gap_hours: u32,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MetricsConfig {
    #[serde(default = "default_prometheus_port")]
    pub prometheus_port: u16,
}
fn default_prometheus_port() -> u16 { 9091 }

impl AppConfig {
    /// Resolve the config file path: NIGHTDRIVE_CONFIG env, else fallback list.
    pub fn resolve_path() -> PathBuf {
        if let Ok(p) = std::env::var("NIGHTDRIVE_CONFIG") {
            return PathBuf::from(p);
        }
        for candidate in &[
            "/etc/nightdrive/nightdrive.toml",
            "./config/nightdrive.toml",
            "./nightdrive.toml",
        ] {
            let p = PathBuf::from(candidate);
            if p.exists() {
                return p;
            }
        }
        PathBuf::from("./config/nightdrive.toml")
    }

    pub fn load() -> crate::NightdriveResult<Self> {
        let path = Self::resolve_path();
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> crate::NightdriveResult<Self> {
        tracing::info!(path = %path.display(), "loading config");
        let text = std::fs::read_to_string(path).map_err(|e| crate::NightdriveError::Io {
            path: path.display().to_string(),
            source: e,
        })?;
        let cfg: AppConfig = toml::from_str(&text)
            .map_err(|e| crate::NightdriveError::Config(format!("parse {}: {}", path.display(), e)))?;
        tracing::info!(
            work_dir = %cfg.paths.work_dir.display(),
            openclaw = %cfg.openclaw.base_url,
            audio_gen = %cfg.audio_gen.base_url,
            art = %cfg.art.base_url,
            "config loaded"
        );
        Ok(cfg)
    }
}
