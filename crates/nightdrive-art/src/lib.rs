//! nightdrive-art — cover-art HTTP client.
//!
//! Sits in front of an AUTOMATIC1111 `stable-diffusion-webui` (or compatible)
//! HTTP endpoint exposed at the URL in `[art].base_url` of `nightdrive.toml`.
//! The endpoint is expected to honor the `/sdapi/v1/txt2img` contract — that's
//! the de facto standard for SDXL self-hosted sidecars, supported by A1111,
//! Forge, and several ComfyUI bridges via plugins. ComfyUI's native
//! `/prompt` API uses a different shape; if we add a `ComfyUiClient` later it
//! goes behind the same [`CoverArtist`] trait so the orchestrator doesn't care.
//!
//! ## Image flow
//!
//! Each `txt2img` response carries a base64-encoded PNG in `images[0]`. We
//! decode it, sniff the IHDR for declared width/height, and write to
//! `tracks/<id>/cover.png`. Bytes from the sidecar are trusted but not blindly
//! — the PNG signature + IHDR dimensions are checked against the request
//! params so a misconfigured sidecar (wrong model, wrong size) can't silently
//! emit a non-cover image without the witness catching it.

use async_trait::async_trait;
use base64::Engine;
use nightdrive_core::config::ArtConfig;
use nightdrive_core::{CompositionSpec, NightdriveError, NightdriveResult, TrackPaths};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use tracing::{debug, info, instrument};

#[async_trait]
pub trait CoverArtist: Send + Sync {
    /// Generate a cover image for `spec` and write it to
    /// `paths.cover_png()`. Returns the on-disk path on success. Implementations
    /// must produce a `width × height` PNG (matching `ArtConfig`) or surface
    /// `NightdriveError::Art`.
    async fn render(
        &self,
        spec: &CompositionSpec,
        paths: &TrackPaths,
    ) -> NightdriveResult<PathBuf>;
}

// =============================================================================
// SdxlClient — A1111 / Forge / stable-diffusion-webui txt2img
// =============================================================================

#[derive(Debug, Clone)]
pub struct SdxlClient {
    http: reqwest::Client,
    cfg: ArtConfig,
}

impl SdxlClient {
    pub fn new(cfg: ArtConfig) -> NightdriveResult<Self> {
        let http = reqwest::Client::builder()
            // SDXL on a P100 fp32 takes 60-90s per image; on a 3070 Ti fp16
            // ~10-20s. 300s timeout covers cold-load + slow sampler combos
            // without hanging the orchestrator forever.
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| NightdriveError::Art(format!("http client: {e}")))?;
        Ok(Self { http, cfg })
    }
}

#[derive(Serialize)]
struct Txt2ImgRequest<'a> {
    prompt: &'a str,
    negative_prompt: &'a str,
    width: u32,
    height: u32,
    steps: u32,
    cfg_scale: f32,
    sampler_name: &'a str,
    seed: i64,
    // n_iter / batch_size kept at 1 each — we want exactly one cover per call.
    n_iter: u32,
    batch_size: u32,
}

#[derive(Deserialize)]
struct Txt2ImgResponse {
    images: Vec<String>,
    #[serde(default)]
    info: String,
}

#[async_trait]
impl CoverArtist for SdxlClient {
    #[instrument(
        skip_all,
        fields(
            track_id = %spec.track_id,
            base_url = %self.cfg.base_url,
            model = %self.cfg.model,
            width = self.cfg.width,
            height = self.cfg.height,
        )
    )]
    async fn render(
        &self,
        spec: &CompositionSpec,
        paths: &TrackPaths,
    ) -> NightdriveResult<PathBuf> {
        // Derive a deterministic seed from the track_id so the same track always
        // produces the same cover on re-run. djb2 hash → fold to i64 (A1111 seed
        // is i64 with -1 meaning random; any non-negative value pins the seed).
        let seed = (djb2_hash(spec.track_id.as_str()) as i64) & 0x7FFF_FFFF_FFFF_FFFF;

        let body = Txt2ImgRequest {
            prompt: spec.cover_prompt.as_str(),
            negative_prompt: self.cfg.negative_prompt.as_str(),
            width: self.cfg.width,
            height: self.cfg.height,
            steps: self.cfg.steps,
            cfg_scale: self.cfg.cfg_scale,
            // Euler a is the conservative default for SDXL across most sampler
            // sets; A1111 / Forge / ComfyUI all expose it under this exact name.
            sampler_name: "Euler a",
            seed,
            n_iter: 1,
            batch_size: 1,
        };

        let url = format!(
            "{}/sdapi/v1/txt2img",
            self.cfg.base_url.trim_end_matches('/')
        );
        debug!(%url, seed, "sending txt2img request");

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| NightdriveError::Art(format!("send: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(NightdriveError::Art(format!(
                "sdxl returned {status}: {text}"
            )));
        }

        let parsed: Txt2ImgResponse = resp
            .json()
            .await
            .map_err(|e| NightdriveError::Art(format!("decode response: {e}")))?;

        let b64 = parsed.images.first().ok_or_else(|| {
            NightdriveError::Art("sdxl response had no images[]".into())
        })?;

        let png_bytes = base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .map_err(|e| NightdriveError::Art(format!("base64 decode: {e}")))?;

        // Validate the PNG matches what we asked for before writing it — fails
        // loudly rather than letting the orchestrator carry a bad cover through
        // to the final encode + upload.
        let (w, h) = parse_png_dimensions(&png_bytes)?;
        if w != self.cfg.width || h != self.cfg.height {
            return Err(NightdriveError::Art(format!(
                "sdxl returned {w}x{h} but config wants {}x{}",
                self.cfg.width, self.cfg.height
            )));
        }

        let out_path = paths.cover_png();
        if let Some(parent) = out_path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| NightdriveError::Io {
                path: parent.display().to_string(),
                source: e,
            })?;
        }
        tokio::fs::write(&out_path, &png_bytes)
            .await
            .map_err(|e| NightdriveError::Io {
                path: out_path.display().to_string(),
                source: e,
            })?;

        info!(
            bytes = png_bytes.len(),
            width = w,
            height = h,
            path = %out_path.display(),
            "cover written"
        );
        // info field can be noisy (the full A1111 metadata dump); kept off the
        // span fields but accessible in debug logs.
        debug!(info = %parsed.info, "sdxl info");
        Ok(out_path)
    }
}

// =============================================================================
// PNG validation
// =============================================================================

const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Pull declared width + height out of a PNG's IHDR chunk. Validates the file
/// starts with the 8-byte PNG signature followed by an IHDR chunk header at
/// byte 8 (`length: u32`, `type: "IHDR"`, then 13-byte data: width u32, height
/// u32, ...). Returns `Err(Art)` on any structural mismatch.
pub fn parse_png_dimensions(bytes: &[u8]) -> NightdriveResult<(u32, u32)> {
    if bytes.len() < 24 {
        return Err(NightdriveError::Art(format!(
            "PNG too short ({} bytes, need >= 24 for IHDR)",
            bytes.len()
        )));
    }
    if bytes[..8] != PNG_SIGNATURE {
        return Err(NightdriveError::Art("not a PNG (signature mismatch)".into()));
    }
    if &bytes[12..16] != b"IHDR" {
        return Err(NightdriveError::Art("PNG missing IHDR chunk type".into()));
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Ok((width, height))
}

// =============================================================================
// Misc helpers
// =============================================================================

/// Cheap deterministic hash for seeding SDXL from a track_id. We don't need
/// cryptographic properties — just stability across re-runs.
fn djb2_hash(s: &str) -> u64 {
    let mut h: u64 = 5381;
    for b in s.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_dimensions_rejects_non_png() {
        assert!(parse_png_dimensions(&[0u8; 24]).is_err());
        assert!(parse_png_dimensions(b"not enough bytes").is_err());
    }

    #[test]
    fn png_dimensions_parses_real_header() {
        // Minimal PNG fixture: signature + IHDR length(13) + "IHDR" + width(2)
        // + height(3) + the remaining 5 IHDR data bytes (don't care about values).
        let mut buf = Vec::new();
        buf.extend_from_slice(&PNG_SIGNATURE);
        buf.extend_from_slice(&13u32.to_be_bytes()); // length
        buf.extend_from_slice(b"IHDR");
        buf.extend_from_slice(&2u32.to_be_bytes()); // width
        buf.extend_from_slice(&3u32.to_be_bytes()); // height
        buf.extend_from_slice(&[8, 6, 0, 0, 0]);    // bit_depth, color_type, …
        let (w, h) = parse_png_dimensions(&buf).expect("valid PNG header");
        assert_eq!((w, h), (2, 3));
    }

    #[test]
    fn djb2_is_stable() {
        // Sanity: same input always yields same hash within a run. Critical for
        // the witness-style "rerunning the same track_id produces a stable seed"
        // contract.
        assert_eq!(djb2_hash("nd-20260510-001"), djb2_hash("nd-20260510-001"));
        assert_ne!(djb2_hash("nd-20260510-001"), djb2_hash("nd-20260510-002"));
    }
}
