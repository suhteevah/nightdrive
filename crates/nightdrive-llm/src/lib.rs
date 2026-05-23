//! nightdrive-llm — talks to an OpenAI-compatible chat endpoint (LiteLLM,
//! Ollama's /v1 layer, real OpenAI, etc.) to generate a `CompositionSpec`
//! for one track. Uses `response_format: {"type":"json_object"}` for JSON
//! enforcement.
//!
//! Historical note: this crate POSTed Ollama's native `/api/chat` until
//! 2026-05-23 when nightdrive moved to a shared cnc box that runs LiteLLM
//! (master-key-gated, OpenAI-format-only) in front of multiple LLM
//! backends. The migration kept the same `OpenclawConfig` struct + added
//! an optional `api_key` field for Bearer auth.

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

// OpenAI-format chat completion request. LiteLLM, Ollama's /v1 layer, and
// real OpenAI all speak this shape.
#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    temperature: f32,
    max_tokens: u32,
    response_format: ResponseFormat<'a>,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: String,
}

#[derive(Serialize)]
struct ResponseFormat<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
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
    /// Single round-trip to the OpenAI-compatible chat endpoint. The retry
    /// loop in `generate_spec` invokes this; surfaces all failure modes
    /// through `NightdriveError::Llm`.
    async fn attempt_generate_spec(&self, user: &str) -> NightdriveResult<CompositionSpec> {
        let body = ChatRequest {
            model: &self.cfg.model,
            messages: vec![
                ChatMessage { role: "system", content: SYSTEM_PROMPT.to_string() },
                ChatMessage { role: "user", content: user.to_string() },
            ],
            temperature: self.cfg.temperature,
            max_tokens: self.cfg.max_tokens,
            response_format: ResponseFormat { kind: "json_object" },
        };

        let url = format!(
            "{}/v1/chat/completions",
            self.cfg.base_url.trim_end_matches('/'),
        );
        debug!(url = %url, "sending chat request");

        let mut req = self.http.post(&url).json(&body);
        if let Some(key) = self.cfg.api_key.as_deref().filter(|k| !k.is_empty()) {
            req = req.bearer_auth(key);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| NightdriveError::Llm(format!("send: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(NightdriveError::Llm(format!(
                "llm endpoint returned {status}: {text}"
            )));
        }

        let parsed: ChatResponse = resp
            .json()
            .await
            .map_err(|e| NightdriveError::Llm(format!("decode response: {e}")))?;

        let content = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| NightdriveError::Llm("no choices in response".into()))?
            .message
            .content;

        debug!(content_len = content.len(), "received content");

        // Some models (notably Anthropic via LiteLLM) wrap JSON in markdown
        // code fences even when response_format:json_object is requested.
        // Strip leading ```json ... ``` / ``` ... ``` blocks before parsing.
        let cleaned = strip_md_code_fences(&content);

        let spec: CompositionSpec = serde_json::from_str(cleaned)
            .map_err(|e| {
                warn!(error = %e, raw = %content, "spec parse failed");
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

/// Strip a leading ```json / ``` code-fence block from an LLM response.
/// Defensive against models that wrap JSON in markdown despite explicit
/// response_format hints. Returns the trimmed slice; if no fences match,
/// returns the input as-is.
fn strip_md_code_fences(s: &str) -> &str {
    let t = s.trim();
    let after_open = if let Some(rest) = t.strip_prefix("```json") {
        rest.trim_start_matches('\n').trim_start()
    } else if let Some(rest) = t.strip_prefix("```") {
        rest.trim_start_matches('\n').trim_start()
    } else {
        return t;
    };
    after_open
        .trim_end()
        .strip_suffix("```")
        .map(|x| x.trim_end())
        .unwrap_or(after_open)
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
