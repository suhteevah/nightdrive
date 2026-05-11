//! nightdrive-llm — talks to the local OpenClaw / Ollama instance to generate
//! a `CompositionSpec` for one track. Uses Ollama's structured-output (JSON
//! schema) mode when available, otherwise enforces JSON via prompt + parse.

use async_trait::async_trait;
use nightdrive_core::{CompositionSpec, NightdriveError, NightdriveResult, TrackId};
use nightdrive_core::config::OpenclawConfig;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{debug, info, instrument, warn};

const SYSTEM_PROMPT: &str = r#"You are a synthwave producer-composer-A&R rolled into one. \
You produce JSON-only output describing one new instrumental synthwave track suitable for \
a "coding / late-night-vibes / chill programming" YouTube channel. \

Hard rules:
- Output ONLY a valid JSON object matching the requested schema. No prose, no markdown.
- Subgenre is always one of: synthwave, outrun, chillsynth, dreamwave, darksynth.
- BPM between 80 and 118.
- Duration between 180 and 360 seconds.
- Musical key in standard notation (e.g. "F# minor", "C major").
- musicgen_prompt is a single paragraph, <= 60 words, describing instrumentation, mood, and production aesthetic. Mention tempo and key. Be specific about analog synth sounds (DX7 pad, OB-8, Juno pluck, sidechained bass, gated reverb drums, etc.).
- cover_prompt describes a synthwave album cover image. Visual only, no text. <= 50 words.
- YouTube title must be unique, evocative, and include a parenthetical tag like "[Synthwave for Coding]" or "[Late Night Programming Mix]".
- Tags: 8-12 short keywords for YouTube SEO.
"#;

const USER_PROMPT_TEMPLATE: &str = r#"Generate one new synthwave track. \
The track_id is "{track_id}". \
Today is {date_iso}. \
Prefer a mood you have not produced recently in this conversation. \

Output JSON shaped like:
{
  "track_id": "<track_id>",
  "title": "<short evocative title>",
  "subgenre": "<subgenre>",
  "mood_tags": ["..."],
  "bpm": <int>,
  "musical_key": "<key>",
  "duration_seconds": <int>,
  "sections": [{"name":"intro","bars":<int>,"instrumentation":"..."}, ...],
  "musicgen_prompt": "...",
  "cover_prompt": "...",
  "youtube": {
    "title": "...",
    "description": "...",
    "tags": ["...","..."],
    "category_id": "10"
  }
}"#;

#[async_trait]
pub trait CompositionLlm: Send + Sync {
    async fn generate_spec(&self, track_id: &TrackId) -> NightdriveResult<CompositionSpec>;
}

// =============================================================================
// Ollama / OpenClaw implementation
// =============================================================================

#[derive(Debug, Clone)]
pub struct OpenclawLlm {
    http: reqwest::Client,
    cfg: OpenclawConfig,
}

impl OpenclawLlm {
    pub fn new(cfg: OpenclawConfig) -> NightdriveResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_seconds))
            .build()
            .map_err(|e| NightdriveError::Llm(format!("build client: {e}")))?;
        Ok(Self { http, cfg })
    }
}

#[derive(Serialize)]
struct OllamaChatRequest<'a> {
    model: &'a str,
    messages: Vec<OllamaMessage<'a>>,
    stream: bool,
    format: &'a str,                // "json" enforces JSON mode
    options: OllamaOptions,
}

#[derive(Serialize)]
struct OllamaMessage<'a> {
    role: &'a str,
    content: String,
}

#[derive(Serialize)]
struct OllamaOptions {
    temperature: f32,
    num_predict: i32,
}

#[derive(Deserialize)]
struct OllamaChatResponse {
    message: OllamaResponseMessage,
}

#[derive(Deserialize)]
struct OllamaResponseMessage {
    content: String,
}

/// Retry budget: 1 initial attempt + 2 retries on parse/validate failure.
/// Per ROADMAP.md N1.4: "Retry on JSON parse failure with the same prompt
/// up to 2× before bubbling."
const MAX_ATTEMPTS: u32 = 3;

#[async_trait]
impl CompositionLlm for OpenclawLlm {
    #[instrument(skip(self), fields(model = %self.cfg.model, track_id = %track_id))]
    async fn generate_spec(&self, track_id: &TrackId) -> NightdriveResult<CompositionSpec> {
        let user = USER_PROMPT_TEMPLATE
            .replace("{track_id}", track_id.as_str())
            .replace("{date_iso}", &chrono::Utc::now().format("%Y-%m-%d").to_string());

        let mut last_err: Option<NightdriveError> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            match self.attempt_generate_spec(&user).await {
                Ok(spec) => {
                    info!(
                        attempt,
                        title = %spec.title,
                        bpm = spec.bpm,
                        key = %spec.musical_key,
                        duration_s = spec.duration_seconds,
                        sections = spec.sections.len(),
                        "composition spec generated"
                    );
                    return Ok(spec);
                }
                Err(e) if is_retryable(&e) && attempt < MAX_ATTEMPTS => {
                    warn!(attempt, max = MAX_ATTEMPTS, error = %e, "spec attempt failed, retrying");
                    last_err = Some(e);
                }
                Err(e) => {
                    warn!(attempt, error = %e, "spec attempt failed, giving up");
                    return Err(e);
                }
            }
        }
        // unreachable in practice — the loop returns on success or final failure —
        // but kept for the type-checker.
        Err(last_err.unwrap_or_else(|| NightdriveError::Llm("retry budget exhausted".into())))
    }
}

impl OpenclawLlm {
    /// Single round-trip to Ollama. The retry loop in `generate_spec` invokes
    /// this; surfaces all failure modes through `NightdriveError::Llm`.
    async fn attempt_generate_spec(&self, user: &str) -> NightdriveResult<CompositionSpec> {
        let body = OllamaChatRequest {
            model: &self.cfg.model,
            messages: vec![
                OllamaMessage { role: "system", content: SYSTEM_PROMPT.to_string() },
                OllamaMessage { role: "user", content: user.to_string() },
            ],
            stream: false,
            format: "json",
            options: OllamaOptions {
                temperature: self.cfg.temperature,
                num_predict: self.cfg.max_tokens as i32,
            },
        };

        let url = format!("{}/api/chat", self.cfg.base_url.trim_end_matches('/'));
        debug!(url = %url, "sending chat request");

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| NightdriveError::Llm(format!("send: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(NightdriveError::Llm(format!(
                "ollama returned {status}: {text}"
            )));
        }

        let parsed: OllamaChatResponse = resp
            .json()
            .await
            .map_err(|e| NightdriveError::Llm(format!("decode response: {e}")))?;

        debug!(content_len = parsed.message.content.len(), "received content");

        let spec: CompositionSpec = serde_json::from_str(&parsed.message.content)
            .map_err(|e| {
                warn!(error = %e, raw = %parsed.message.content, "spec parse failed");
                NightdriveError::Llm(format!("spec json parse: {e}"))
            })?;

        // Cheap sanity checks. The model is creative — keep it honest.
        validate_spec(&spec)?;
        Ok(spec)
    }
}

/// Only retry on parse/validate errors. Transport-level failures (timeouts,
/// connection refused, non-2xx HTTP) bubble immediately so we don't pound
/// a sick Ollama with three identical prompts.
fn is_retryable(err: &NightdriveError) -> bool {
    let NightdriveError::Llm(msg) = err else { return false; };
    msg.starts_with("spec json parse:")
        || msg.starts_with("empty title")
        || msg.starts_with("bpm out of range:")
        || msg.starts_with("duration out of range:")
        || msg.starts_with("no sections")
        || msg.starts_with("musicgen_prompt too long")
        || msg.starts_with("empty youtube tags")
}

fn validate_spec(spec: &CompositionSpec) -> NightdriveResult<()> {
    if spec.title.trim().is_empty() {
        return Err(NightdriveError::Llm("empty title".into()));
    }
    if !(80..=118).contains(&spec.bpm) {
        return Err(NightdriveError::Llm(format!("bpm out of range: {}", spec.bpm)));
    }
    if !(180..=360).contains(&spec.duration_seconds) {
        return Err(NightdriveError::Llm(format!(
            "duration out of range: {}",
            spec.duration_seconds
        )));
    }
    if spec.sections.is_empty() {
        return Err(NightdriveError::Llm("no sections".into()));
    }
    if spec.musicgen_prompt.split_whitespace().count() > 80 {
        return Err(NightdriveError::Llm("musicgen_prompt too long".into()));
    }
    if spec.youtube.tags.is_empty() {
        return Err(NightdriveError::Llm("empty youtube tags".into()));
    }
    Ok(())
}
