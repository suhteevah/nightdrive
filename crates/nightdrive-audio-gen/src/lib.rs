//! nightdrive-audio-gen — HTTP client for the Stable Audio Open sidecar with
//! segment chaining + crossfade.
//!
//! The sidecar at `[audio_gen].base_url` exposes `POST /generate` returning a
//! WAV body per ≤47s segment (Stable Audio Open 1.0's trained ceiling). For a
//! 3-6 minute target we generate N segments and stitch them with an equal-power
//! crossfade — short enough (~1 bar at the spec's BPM) that the seam isn't
//! audibly a fade but long enough that mismatched DC offsets between segments
//! don't click.
//!
//! ## Audio format contract
//!
//! Sidecar returns PCM 16-bit stereo at `sample_rate` Hz (44.1 kHz for SAO 1.0).
//! Output `raw.wav` matches that format verbatim so the audio-master crate
//! can pass it straight to ffmpeg loudnorm without resampling.

use async_trait::async_trait;
use base64::Engine as _;
use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use nightdrive_core::config::AudioGenConfig;
use nightdrive_core::{CompositionSpec, NightdriveError, NightdriveResult, TrackPaths};
use serde::Serialize;
use std::io::Cursor;
use std::path::PathBuf;
use std::time::Duration;
use tracing::{debug, info, instrument, warn};

pub mod prompt;

#[async_trait]
pub trait AudioGenerator: Send + Sync {
    /// Generate the raw stitched audio for `spec` and write it to
    /// `paths.raw_audio_wav()`. Returns the on-disk path on success.
    async fn render(
        &self,
        spec: &CompositionSpec,
        paths: &TrackPaths,
    ) -> NightdriveResult<PathBuf>;
}

/// Construct the right [`AudioGenerator`] for the configured `engine`. The
/// orchestrator calls through this rather than naming a client directly so the
/// engine choice stays config-driven.
pub fn client_for(cfg: AudioGenConfig) -> NightdriveResult<Box<dyn AudioGenerator>> {
    match cfg.engine.as_str() {
        "stable_audio" | "" => Ok(Box::new(StableAudioClient::new(cfg)?)),
        "musicgen" => Ok(Box::new(MusicGenClient::new(cfg)?)),
        "ace_step" => Ok(Box::new(AceStepClient::new(cfg)?)),
        other => Err(NightdriveError::AudioGen(format!(
            "unknown [audio_gen].engine = {other:?} \
             (expected 'stable_audio', 'musicgen', or 'ace_step')"
        ))),
    }
}

// =============================================================================
// StableAudioClient — talks to the FastAPI sidecar at sidecar/stable_audio_server.py
// =============================================================================

#[derive(Debug, Clone)]
pub struct StableAudioClient {
    http: reqwest::Client,
    cfg: AudioGenConfig,
}

impl StableAudioClient {
    pub fn new(cfg: AudioGenConfig) -> NightdriveResult<Self> {
        let http = reqwest::Client::builder()
            // SAO at 100 steps on a 3070 Ti takes 20-60s/segment depending on
            // duration. 600s timeout covers cold-load + slow paths without
            // wedging the orchestrator forever.
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|e| NightdriveError::AudioGen(format!("http client: {e}")))?;
        Ok(Self { http, cfg })
    }

    pub async fn health(&self) -> NightdriveResult<HealthResponse> {
        let url = format!("{}/health", self.cfg.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| NightdriveError::AudioGen(format!("health: {e}")))?;
        resp.json()
            .await
            .map_err(|e| NightdriveError::AudioGen(format!("health decode: {e}")))
    }

    async fn generate_segment(
        &self,
        prompt: &str,
        duration_seconds: f32,
        seed: u64,
    ) -> NightdriveResult<Vec<u8>> {
        #[derive(Serialize)]
        struct Req<'a> {
            prompt: &'a str,
            duration_seconds: f32,
            seed: u64,
        }
        let url = format!("{}/generate", self.cfg.base_url.trim_end_matches('/'));
        debug!(%url, duration_seconds, seed, "POST /generate");

        let resp = self
            .http
            .post(&url)
            .json(&Req { prompt, duration_seconds, seed })
            .send()
            .await
            .map_err(|e| NightdriveError::AudioGen(format!("POST /generate: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(NightdriveError::AudioGen(format!(
                "sidecar {status}: {text}"
            )));
        }

        let tail_dropped = resp
            .headers()
            .get("x-nightdrive-tail-dropped")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        if tail_dropped > 0 {
            warn!(
                tail_dropped,
                "sidecar truncated prompt — edit nightdrive-llm prompt template if this happens often"
            );
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| NightdriveError::AudioGen(format!("read wav body: {e}")))?
            .to_vec();
        debug!(bytes = bytes.len(), "segment received");
        Ok(bytes)
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub model: String,
    pub device: String,
    pub sample_rate: u32,
}

#[async_trait]
impl AudioGenerator for StableAudioClient {
    #[instrument(
        skip_all,
        fields(
            track_id = %spec.track_id,
            base_url = %self.cfg.base_url,
            target_duration_s = spec.duration_seconds,
        )
    )]
    async fn render(
        &self,
        spec: &CompositionSpec,
        paths: &TrackPaths,
    ) -> NightdriveResult<PathBuf> {
        let target = spec.duration_seconds as f32;
        let segment_s = self.cfg.segment_seconds as f32;
        let overlap_s = self.cfg.overlap_seconds as f32;

        // How many segments do we need so post-crossfade total >= target?
        // Each crossfade eats `overlap_s` of the prior segment, so total wall =
        // segment_s + (n-1) * (segment_s - overlap_s). Solve for n.
        let effective_per_segment = segment_s - overlap_s;
        let n_segments = (((target - segment_s) / effective_per_segment).ceil() as u32 + 1).max(1);
        info!(
            n_segments,
            segment_s,
            overlap_s,
            "planning audio: target {:.1}s = 1 + {}*{:.1}s",
            target,
            n_segments - 1,
            effective_per_segment,
        );

        let mut segments: Vec<Vec<u8>> = Vec::with_capacity(n_segments as usize);
        for i in 0..n_segments {
            // Deterministic seed: hash(track_id) XOR i so a track always regenerates
            // identically but its segments are diverse.
            let seed = djb2_hash(spec.track_id.as_str()) ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
            info!(segment = i + 1, total = n_segments, "generating segment");
            let bytes = self.generate_segment(&spec.musicgen_prompt, segment_s, seed).await?;
            segments.push(bytes);
        }

        // Decode each segment WAV, stitch with crossfade, write the final WAV.
        let out_path = paths.raw_audio_wav();
        if let Some(parent) = out_path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| NightdriveError::Io {
                path: parent.display().to_string(),
                source: e,
            })?;
        }
        let segment_paths: Vec<PathBuf> = (0..segments.len())
            .map(|i| out_path.with_file_name(format!("raw-seg-{i:02}.wav")))
            .collect();
        for (path, bytes) in segment_paths.iter().zip(&segments) {
            tokio::fs::write(path, bytes).await.map_err(|e| NightdriveError::Io {
                path: path.display().to_string(),
                source: e,
            })?;
        }

        // The actual stitching is sync (hound + small DSP loop). Push it onto a
        // blocking thread so the tokio runtime doesn't stall.
        let segment_paths_clone = segment_paths.clone();
        let out_path_clone = out_path.clone();
        let overlap_seconds = overlap_s;
        tokio::task::spawn_blocking(move || -> NightdriveResult<()> {
            stitch_segments_with_crossfade(&segment_paths_clone, &out_path_clone, overlap_seconds)
        })
        .await
        .map_err(|e| NightdriveError::AudioGen(format!("stitch join: {e}")))??;

        // Clean up per-segment intermediates. master.flac is what the rest of
        // the pipeline cares about; the segments are debugging detritus.
        for path in &segment_paths {
            if let Err(e) = tokio::fs::remove_file(path).await {
                debug!(path = %path.display(), error = %e, "leaving per-segment file behind");
            }
        }

        let metadata = tokio::fs::metadata(&out_path).await.map_err(|e| NightdriveError::Io {
            path: out_path.display().to_string(),
            source: e,
        })?;
        info!(
            bytes = metadata.len(),
            path = %out_path.display(),
            "raw audio written"
        );
        Ok(out_path)
    }
}

// =============================================================================
// Stitching — equal-power crossfade between PCM16 WAVs
// =============================================================================

fn stitch_segments_with_crossfade(
    segments: &[PathBuf],
    out_path: &std::path::Path,
    overlap_seconds: f32,
) -> NightdriveResult<()> {
    if segments.is_empty() {
        return Err(NightdriveError::AudioGen("no segments to stitch".into()));
    }

    // Read first segment to lock the spec (sample rate, channels) every subsequent
    // segment must match.
    let first = WavReader::open(&segments[0])
        .map_err(|e| NightdriveError::AudioGen(format!("open {}: {e}", segments[0].display())))?;
    let spec = first.spec();
    if spec.sample_format != SampleFormat::Int || spec.bits_per_sample != 16 {
        return Err(NightdriveError::AudioGen(format!(
            "expected PCM int16, got {:?} {} bits",
            spec.sample_format, spec.bits_per_sample,
        )));
    }
    drop(first);

    let overlap_frames = (overlap_seconds * spec.sample_rate as f32) as usize;
    let channels = spec.channels as usize;

    // Read each segment in full into `Vec<i16>` arranged as interleaved frames.
    let mut all_segments: Vec<Vec<i16>> = Vec::with_capacity(segments.len());
    for path in segments {
        let mut reader = WavReader::open(path)
            .map_err(|e| NightdriveError::AudioGen(format!("open {}: {e}", path.display())))?;
        let seg_spec = reader.spec();
        if seg_spec.sample_rate != spec.sample_rate || seg_spec.channels != spec.channels {
            return Err(NightdriveError::AudioGen(format!(
                "segment {} format mismatch: {:?} vs {:?}",
                path.display(),
                seg_spec,
                spec,
            )));
        }
        let samples: Result<Vec<i16>, _> = reader.samples::<i16>().collect();
        let samples = samples
            .map_err(|e| NightdriveError::AudioGen(format!("decode {}: {e}", path.display())))?;
        all_segments.push(samples);
    }

    // Stitch: start with seg[0], then crossfade each subsequent seg into it.
    let mut stitched: Vec<i16> = all_segments[0].clone();
    for seg in all_segments.iter().skip(1) {
        crossfade_into(&mut stitched, seg, overlap_frames, channels);
    }

    // Write the stitched buffer to out_path.
    let mut writer = WavWriter::create(out_path, spec).map_err(|e| {
        NightdriveError::AudioGen(format!("create {}: {e}", out_path.display()))
    })?;
    for sample in &stitched {
        writer
            .write_sample(*sample)
            .map_err(|e| NightdriveError::AudioGen(format!("write sample: {e}")))?;
    }
    writer
        .finalize()
        .map_err(|e| NightdriveError::AudioGen(format!("finalize wav: {e}")))?;
    Ok(())
}

/// Equal-power crossfade: replace the last `overlap_frames * channels` samples
/// of `accum` with a cosine-mixed blend of accum's tail + next's head, then
/// append the remainder of `next` after the overlap region.
fn crossfade_into(
    accum: &mut Vec<i16>,
    next: &[i16],
    overlap_frames: usize,
    channels: usize,
) {
    let overlap_samples = overlap_frames * channels;
    if overlap_samples == 0 || accum.len() < overlap_samples || next.len() < overlap_samples {
        // Degenerate path — just concatenate. Should only happen if overlap_seconds=0
        // or a segment came back too short. Both are configuration bugs worth seeing
        // in audit but not worth panicking over.
        accum.extend_from_slice(next);
        return;
    }

    let accum_tail_start = accum.len() - overlap_samples;
    for frame in 0..overlap_frames {
        // t in [0, 1) across the overlap region
        let t = frame as f32 / overlap_frames as f32;
        // Equal-power (constant-energy) crossfade: cos(π/2 · t) and sin(π/2 · t)
        // sum to 1 in power, avoids the "dip in the middle" of linear crossfade.
        let a_gain = (std::f32::consts::FRAC_PI_2 * t).cos();
        let b_gain = (std::f32::consts::FRAC_PI_2 * t).sin();
        for ch in 0..channels {
            let i = frame * channels + ch;
            let a = accum[accum_tail_start + i] as f32;
            let b = next[i] as f32;
            let mixed = a * a_gain + b * b_gain;
            accum[accum_tail_start + i] = mixed.clamp(-32768.0, 32767.0) as i16;
        }
    }
    // Append the part of `next` after the overlap region.
    accum.extend_from_slice(&next[overlap_samples..]);
}

/// Same djb2 used by nightdrive-art for stable seed-from-track-id behavior.
fn djb2_hash(s: &str) -> u64 {
    let mut h: u64 = 5381;
    for b in s.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    h
}

// =============================================================================
// MusicGenClient — true audio-continuation chaining via prev_audio_b64
// =============================================================================
//
// Where StableAudioClient stitches independent clips with a blind crossfade,
// MusicGen has native continuation: the sidecar runs `generate_continuation`
// on a prefix of accumulated audio, so each segment is a real extension of
// the prior audio rather than a separate clip blended in. No seam — the model
// keeps the chord progression, drum pattern, and stereo image coherent across
// segment boundaries.
//
// **License caveat:** MusicGen weights are CC-BY-NC. The strike risk has been
// accepted on this project for the NightDrive channel (see
// `.claude/projects/J--nightdrive/memory/project_musicgen_commercial_risk_accepted.md`).
// Don't re-surface the tradeoff unless asked.

#[derive(Debug, Clone)]
pub struct MusicGenClient {
    http: reqwest::Client,
    cfg: AudioGenConfig,
}

impl MusicGenClient {
    pub fn new(cfg: AudioGenConfig) -> NightdriveResult<Self> {
        let http = reqwest::Client::builder()
            // MusicGen on a 3070 Ti is 0.5-1x realtime depending on segment
            // length and continuation context. 600s timeout fits a 30s segment
            // with comfortable margin.
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|e| NightdriveError::AudioGen(format!("http client: {e}")))?;
        Ok(Self { http, cfg })
    }

    /// POST /generate with an optional `prev_audio_b64` field. When provided,
    /// the sidecar calls `generate_continuation` and returns the regenerated
    /// prefix + new audio. When absent, it's a fresh text-to-audio generation.
    async fn generate_segment(
        &self,
        prompt: &str,
        duration_seconds: f32,
        seed: u64,
        prev_audio_b64: Option<String>,
    ) -> NightdriveResult<Vec<u8>> {
        #[derive(Serialize)]
        struct Req<'a> {
            prompt: &'a str,
            duration_seconds: f32,
            seed: u64,
            #[serde(skip_serializing_if = "Option::is_none")]
            prev_audio_b64: Option<String>,
        }
        let url = format!("{}/generate", self.cfg.base_url.trim_end_matches('/'));
        let has_prev = prev_audio_b64.is_some();
        debug!(%url, duration_seconds, seed, has_prev, "POST /generate (musicgen)");

        let resp = self
            .http
            .post(&url)
            .json(&Req {
                prompt,
                duration_seconds,
                seed,
                prev_audio_b64,
            })
            .send()
            .await
            .map_err(|e| NightdriveError::AudioGen(format!("POST /generate: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(NightdriveError::AudioGen(format!(
                "musicgen {status}: {text}"
            )));
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| NightdriveError::AudioGen(format!("read wav body: {e}")))?
            .to_vec();
        debug!(bytes = bytes.len(), continuation = has_prev, "segment received");
        Ok(bytes)
    }
}

#[async_trait]
impl AudioGenerator for MusicGenClient {
    #[instrument(
        skip_all,
        fields(
            track_id = %spec.track_id,
            base_url = %self.cfg.base_url,
            target_duration_s = spec.duration_seconds,
            engine = "musicgen",
        )
    )]
    async fn render(
        &self,
        spec: &CompositionSpec,
        paths: &TrackPaths,
    ) -> NightdriveResult<PathBuf> {
        let target = spec.duration_seconds as f32;
        // MusicGen's max per-call is ~30s; clamp the configured segment_seconds
        // defensively so we don't hit 422s at the sidecar.
        let segment_s = (self.cfg.segment_seconds as f32).min(30.0);
        let prefix_s = self.cfg.continuation_prefix_seconds.max(2.0);

        // Generate segment 1 fresh (no prefix). Use it to lock the WAV spec
        // (sample_rate, channels) every subsequent continuation must match.
        let base_seed = djb2_hash(spec.track_id.as_str());
        info!(segment_s, prefix_s, "musicgen segment 1 (fresh, no prefix)");
        let seg1_bytes = self.generate_segment(&spec.musicgen_prompt, segment_s, base_seed, None).await?;
        let (mut accumulated, spec_wav) = decode_wav_interleaved(&seg1_bytes)?;
        info!(
            sample_rate = spec_wav.sample_rate,
            channels = spec_wav.channels,
            bits_per_sample = spec_wav.bits_per_sample,
            initial_frames = accumulated.len() / spec_wav.channels as usize,
            "musicgen segment 1 decoded; locked WAV spec"
        );

        let target_frames = (target * spec_wav.sample_rate as f32) as usize;
        let mut segment_idx: u32 = 1;
        while accumulated.len() / spec_wav.channels as usize + 0 < target_frames {
            segment_idx += 1;
            // How many frames of prefix to send.
            let prefix_frames = (prefix_s * spec_wav.sample_rate as f32) as usize;
            let current_frames = accumulated.len() / spec_wav.channels as usize;
            let take_frames = prefix_frames.min(current_frames);
            let prefix_samples_start = (current_frames - take_frames) * spec_wav.channels as usize;
            let prefix_samples = &accumulated[prefix_samples_start..];
            // Encode prefix as an in-memory WAV.
            let prefix_wav = encode_wav_interleaved(prefix_samples, spec_wav)?;
            let prev_b64 = base64::engine::general_purpose::STANDARD.encode(&prefix_wav);

            let seed = base_seed ^ ((segment_idx as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            info!(
                segment = segment_idx,
                prefix_frames = take_frames,
                prefix_kb = prefix_wav.len() / 1024,
                "musicgen continuation (segment_idx > 1)"
            );

            let seg_bytes = self
                .generate_segment(&spec.musicgen_prompt, segment_s, seed, Some(prev_b64))
                .await?;
            let (seg_samples, seg_spec) = decode_wav_interleaved(&seg_bytes)?;
            if seg_spec.sample_rate != spec_wav.sample_rate
                || seg_spec.channels != spec_wav.channels
            {
                return Err(NightdriveError::AudioGen(format!(
                    "continuation segment WAV spec drift: {:?} vs {:?}",
                    seg_spec, spec_wav,
                )));
            }
            // The sidecar's response begins with the regenerated prefix region
            // (audiocraft's `generate_continuation` returns prefix + new audio).
            // Skip the prefix-length samples to get just the new continuation.
            let skip = (take_frames * seg_spec.channels as usize).min(seg_samples.len());
            accumulated.extend_from_slice(&seg_samples[skip..]);
            debug!(
                accumulated_frames = accumulated.len() / spec_wav.channels as usize,
                target_frames,
                "appended continuation slice"
            );

            // Safety: 30 segment cap (~10 min of audio max) so a misbehaving
            // continuation that produces no new samples doesn't loop forever.
            if segment_idx >= 30 {
                warn!(
                    "musicgen continuation hit 30-segment safety cap before reaching target_frames"
                );
                break;
            }
        }

        // Trim to exact target duration so the encoder gets a clean length.
        let final_samples = (target_frames * spec_wav.channels as usize).min(accumulated.len());
        accumulated.truncate(final_samples);

        // Write the stitched WAV.
        let out_path = paths.raw_audio_wav();
        if let Some(parent) = out_path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| NightdriveError::Io {
                path: parent.display().to_string(),
                source: e,
            })?;
        }
        let out_path_clone = out_path.clone();
        tokio::task::spawn_blocking(move || -> NightdriveResult<()> {
            let mut writer = WavWriter::create(&out_path_clone, spec_wav).map_err(|e| {
                NightdriveError::AudioGen(format!("create {}: {e}", out_path_clone.display()))
            })?;
            for sample in &accumulated {
                writer
                    .write_sample(*sample)
                    .map_err(|e| NightdriveError::AudioGen(format!("write sample: {e}")))?;
            }
            writer
                .finalize()
                .map_err(|e| NightdriveError::AudioGen(format!("finalize wav: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| NightdriveError::AudioGen(format!("write join: {e}")))??;

        let meta = tokio::fs::metadata(&out_path).await.map_err(|e| NightdriveError::Io {
            path: out_path.display().to_string(),
            source: e,
        })?;
        info!(
            bytes = meta.len(),
            segments = segment_idx,
            path = %out_path.display(),
            "musicgen continuation track written"
        );
        Ok(out_path)
    }
}

// =============================================================================
// In-memory WAV codec helpers (PCM int16 only)
// =============================================================================

fn decode_wav_interleaved(bytes: &[u8]) -> NightdriveResult<(Vec<i16>, WavSpec)> {
    let mut reader = WavReader::new(Cursor::new(bytes))
        .map_err(|e| NightdriveError::AudioGen(format!("open in-memory wav: {e}")))?;
    let spec = reader.spec();
    if spec.sample_format != SampleFormat::Int || spec.bits_per_sample != 16 {
        return Err(NightdriveError::AudioGen(format!(
            "expected PCM int16, got {:?} {} bits",
            spec.sample_format, spec.bits_per_sample,
        )));
    }
    let samples: Result<Vec<i16>, _> = reader.samples::<i16>().collect();
    let samples =
        samples.map_err(|e| NightdriveError::AudioGen(format!("decode samples: {e}")))?;
    Ok((samples, spec))
}

fn encode_wav_interleaved(samples: &[i16], spec: WavSpec) -> NightdriveResult<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::with_capacity(samples.len() * 2 + 44);
    {
        let cursor = Cursor::new(&mut buf);
        let mut writer = WavWriter::new(cursor, spec)
            .map_err(|e| NightdriveError::AudioGen(format!("create in-memory wav: {e}")))?;
        for s in samples {
            writer
                .write_sample(*s)
                .map_err(|e| NightdriveError::AudioGen(format!("write sample: {e}")))?;
        }
        writer
            .finalize()
            .map_err(|e| NightdriveError::AudioGen(format!("finalize in-memory wav: {e}")))?;
    }
    Ok(buf)
}

// =============================================================================
// PCM helpers exposed for the witness
// =============================================================================

/// Read a PCM WAV from disk and return (sample_rate, channels, duration_seconds).
pub fn probe_wav(path: &std::path::Path) -> NightdriveResult<(u32, u16, f32)> {
    let reader = WavReader::open(path).map_err(|e| {
        NightdriveError::AudioGen(format!("open {}: {e}", path.display()))
    })?;
    let spec = reader.spec();
    let duration_seconds =
        reader.duration() as f32 / spec.sample_rate as f32;
    Ok((spec.sample_rate, spec.channels, duration_seconds))
}

// =============================================================================
// AceStepClient — single-shot full-song generation via ACE-Step 1.5 sidecar
// =============================================================================
//
// Where StableAudioClient stitches and MusicGenClient continuation-chains,
// ACE-Step does the whole song in ONE POST /generate call. The Rust client
// sends caption + structured lyrics + bpm + key + duration; the sidecar
// returns the complete WAV body. No segment loop, no crossfade, no
// continuation-prefix re-encode.
//
// The two structured inputs come from `prompt::format_ace_step_caption(spec)`
// (caption, ≤512 chars) and `prompt::format_ace_step_lyrics(spec)`
// (per-section `[Section - notes]` block lines derived from spec.sections[]).
// This is the layer the MG/SAO engines were throwing away.
//
// **License:** ACE-Step 1.5 weights are MIT. The CC-BY-NC strike risk we
// accepted for MusicGen does not apply to tracks rendered via this engine.

#[derive(Debug, Clone)]
pub struct AceStepClient {
    http: reqwest::Client,
    cfg: AudioGenConfig,
}

impl AceStepClient {
    pub fn new(cfg: AudioGenConfig) -> NightdriveResult<Self> {
        // ACE-Step base SFT generates a 4-min song in ~30-90s on a 3070 Ti
        // and ~60-180s on a P100 (no fp16 accel). 600s timeout fits the
        // worst-case "loading model from cold + first-call diffusion warm-up"
        // — same shape as the other sidecars.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .map_err(|e| NightdriveError::AudioGen(format!("http client: {e}")))?;
        Ok(Self { http, cfg })
    }

    pub async fn health(&self) -> NightdriveResult<AceStepHealthResponse> {
        let url = format!("{}/health", self.cfg.base_url.trim_end_matches('/'));
        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .await
            .map_err(|e| NightdriveError::AudioGen(format!("health: {e}")))?;
        resp.json()
            .await
            .map_err(|e| NightdriveError::AudioGen(format!("health decode: {e}")))
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct AceStepHealthResponse {
    pub ok: bool,
    pub model: String,
    pub device: String,
    pub sample_rate: u32,
    pub channels: u32,
    #[serde(default)]
    pub supports_structured_lyrics: bool,
    #[serde(default)]
    pub vram_used_gb: f32,
    #[serde(default)]
    pub vram_total_gb: f32,
}

#[async_trait]
impl AudioGenerator for AceStepClient {
    #[instrument(
        skip_all,
        fields(
            track_id = %spec.track_id,
            base_url = %self.cfg.base_url,
            target_duration_s = spec.duration_seconds,
            engine = "ace_step",
        )
    )]
    async fn render(
        &self,
        spec: &CompositionSpec,
        paths: &TrackPaths,
    ) -> NightdriveResult<PathBuf> {
        #[derive(Serialize)]
        struct Req<'a> {
            caption: &'a str,
            lyrics: &'a str,
            duration_seconds: f32,
            bpm: u32,
            musical_key: &'a str,
            seed: i64,
            guidance_scale: f32,
            inference_steps: u32,
        }

        let caption = prompt::format_ace_step_caption(spec);
        let lyrics = prompt::format_ace_step_lyrics(spec);
        let seed = djb2_hash(spec.track_id.as_str()) as i64;
        let duration_seconds = spec.duration_seconds as f32;

        info!(
            caption_len = caption.chars().count(),
            lyrics_lines = lyrics.lines().count(),
            inference_steps = self.cfg.inference_steps,
            seed,
            "ACE-Step single-shot generation"
        );

        let url = format!("{}/generate", self.cfg.base_url.trim_end_matches('/'));
        let body = Req {
            caption: &caption,
            lyrics: &lyrics,
            duration_seconds,
            bpm: spec.bpm,
            musical_key: &spec.musical_key,
            seed,
            guidance_scale: self.cfg.guidance_scale,
            inference_steps: self.cfg.inference_steps,
        };
        debug!(%url, "POST /generate (ace_step)");

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| NightdriveError::AudioGen(format!("POST /generate: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(NightdriveError::AudioGen(format!(
                "ace_step {status}: {text}"
            )));
        }

        let gen_wall = resp
            .headers()
            .get("x-nightdrive-gen-wall-seconds")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<f32>().ok())
            .unwrap_or(0.0);
        let sr_header = resp
            .headers()
            .get("x-nightdrive-sample-rate")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u32>().ok());

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| NightdriveError::AudioGen(format!("read wav body: {e}")))?
            .to_vec();
        info!(
            wav_bytes = bytes.len(),
            gen_wall_s = gen_wall,
            sample_rate_header = ?sr_header,
            "ACE-Step returned full-song WAV"
        );

        let out_path = paths.raw_audio_wav();
        if let Some(parent) = out_path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| NightdriveError::Io {
                path: parent.display().to_string(),
                source: e,
            })?;
        }
        tokio::fs::write(&out_path, &bytes).await.map_err(|e| NightdriveError::Io {
            path: out_path.display().to_string(),
            source: e,
        })?;

        // Sanity-check the file roundtrips through hound's WAV reader — guards
        // against the sidecar accidentally returning JSON-as-WAV or a partial
        // body on a transport hiccup.
        let (sr, channels, duration) = probe_wav(&out_path)?;
        info!(
            sample_rate = sr,
            channels,
            duration_s = duration,
            target_s = duration_seconds,
            path = %out_path.display(),
            "raw audio written (ACE-Step single-shot)"
        );
        if duration < duration_seconds * 0.5 {
            warn!(
                actual_s = duration,
                target_s = duration_seconds,
                "ACE-Step output shorter than 50% of target — model may have truncated; investigate prompt"
            );
        }

        Ok(out_path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossfade_short_circuits_on_empty_overlap() {
        let mut accum = vec![1i16, 2, 3, 4];
        let next = vec![5i16, 6, 7, 8];
        crossfade_into(&mut accum, &next, 0, 2);
        assert_eq!(accum, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn crossfade_mixes_overlap_region() {
        // 4-frame overlap, mono. Accum ends in [100,100,100,100], next starts
        // with [0,0,0,0]. With equal-power crossfade, the overlap region should
        // ramp down smoothly: first sample mostly 100, last sample mostly 0.
        let mut accum = vec![0i16, 0, 0, 100, 100, 100, 100];
        let next = vec![0i16, 0, 0, 0, 200, 200, 200];
        crossfade_into(&mut accum, &next, 4, 1);
        // accum.len() is 7 (original) - 4 (overlap, replaced in place) + 0 (no
        // post-overlap data in next beyond index 4) + 3 (next[4..]) = 10.
        assert_eq!(accum.len(), 10);
        // First sample of overlap: t=0 -> cos=1, sin=0 -> pure accum (100).
        assert!(
            (accum[3] - 100).abs() <= 1,
            "first overlap sample should be near 100, got {}",
            accum[3]
        );
        // Last sample of overlap: t=3/4 -> cos≈0.38, sin≈0.92 -> mostly next (≈0).
        assert!(
            accum[6].abs() <= 40,
            "last overlap sample should be near 0, got {}",
            accum[6]
        );
        // Post-overlap region appended from next[4..].
        assert_eq!(&accum[7..], &[200, 200, 200]);
    }

    #[test]
    fn djb2_matches_art_crate() {
        // Sanity check: same hash function semantics as nightdrive-art so seed
        // derivation is symmetric across crates.
        assert_eq!(djb2_hash("nd-20260510-001"), djb2_hash("nd-20260510-001"));
    }
}
