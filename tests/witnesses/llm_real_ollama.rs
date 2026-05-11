// stage: 1
// expect: OpenclawLlm::generate_spec returns a parseable, validated CompositionSpec
//         from a real Ollama running qwen2.5:7b-instruct (or whatever model NIGHTDRIVE_OPENCLAW_MODEL says)
// requires: Ollama reachable at NIGHTDRIVE_OPENCLAW_URL (default http://localhost:11434),
//           with the configured model pulled. Skips with eprintln! if unreachable.
//
// Proves nightdrive-llm's OpenclawLlm + retry loop work end-to-end against a
// real Ollama instance — no mocks per tests/witnesses/README.md. The "no mocks"
// rule exists because last quarter mocked tests passed while the prod migration
// failed; the model's JSON-mode adherence is one of the things mocks can't fake.
//
// The validation step inside generate_spec (BPM 80-118, duration 180-360,
// non-empty title/sections/tags, musicgen_prompt <= 80 words) doubles as a
// witness for the retry-on-parse-failure budget — if the model emits a
// borderline-bad spec, we expect up to 2 retries before bubbling.

use nightdrive_core::TrackId;
use nightdrive_core::config::OpenclawConfig;
use nightdrive_llm::{CompositionLlm, OpenclawLlm};

const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";
const DEFAULT_MODEL: &str = "qwen2.5:7b-instruct";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn llm_real_ollama_generates_valid_spec() {
    let base_url = std::env::var("NIGHTDRIVE_OPENCLAW_URL")
        .unwrap_or_else(|_| DEFAULT_OLLAMA_URL.to_string());
    let model = std::env::var("NIGHTDRIVE_OPENCLAW_MODEL")
        .unwrap_or_else(|_| DEFAULT_MODEL.to_string());

    // Probe before we commit to the test — skip cleanly if Ollama isn't
    // reachable OR the configured model isn't pulled. CI-less local: dev
    // can `cargo test` and have the suite pass without dragging in 4.5 GB
    // of weights.
    match probe_ollama(&base_url, &model).await {
        OllamaProbe::ModelPresent => {}
        OllamaProbe::Unreachable => {
            eprintln!(
                "SKIP: ollama not reachable at {base_url} (set NIGHTDRIVE_OPENCLAW_URL or start ollama)"
            );
            return;
        }
        OllamaProbe::ModelMissing(present) => {
            eprintln!(
                "SKIP: ollama at {base_url} is up but model '{model}' is not pulled. \
                 Available: {present:?}. Pull with `ollama pull {model}` or set \
                 NIGHTDRIVE_OPENCLAW_MODEL to one of the present models."
            );
            return;
        }
    }

    let cfg = OpenclawConfig {
        base_url: base_url.clone(),
        model: model.clone(),
        temperature: 0.85,
        max_tokens: 2048,
        // Generous timeout — qwen2.5:7b-instruct on a cold load on a 3070 Ti
        // can take 20-30s for the first token, then ~2-5s/track on warm cache.
        timeout_seconds: 180,
    };

    let llm = OpenclawLlm::new(cfg).expect("OpenclawLlm::new should succeed with valid config");

    // Stable, far-past date so we never collide with real track IDs.
    let date = chrono::NaiveDate::from_ymd_opt(1999, 1, 1)
        .expect("1999-01-01 is a valid date");
    let track_id = TrackId::new(date, 1);

    let spec = llm
        .generate_spec(&track_id)
        .await
        .unwrap_or_else(|e| {
            panic!("generate_spec failed against real ollama at {base_url} model={model}: {e}")
        });

    // The validate_spec call inside generate_spec already enforced range invariants;
    // re-assert the contract here so the witness fails loudly if validation ever
    // gets weakened.
    assert!(
        !spec.title.trim().is_empty(),
        "spec.title must be non-empty (validate_spec contract)"
    );
    assert!(
        (80..=118).contains(&spec.bpm),
        "spec.bpm = {} must be in 80..=118 (validate_spec contract)",
        spec.bpm
    );
    assert!(
        (180..=360).contains(&spec.duration_seconds),
        "spec.duration_seconds = {} must be in 180..=360 (validate_spec contract)",
        spec.duration_seconds
    );
    assert!(
        !spec.sections.is_empty(),
        "spec.sections must be non-empty (validate_spec contract)"
    );
    assert!(
        !spec.youtube.tags.is_empty(),
        "spec.youtube.tags must be non-empty (validate_spec contract)"
    );
    assert!(
        spec.musicgen_prompt.split_whitespace().count() <= 80,
        "spec.musicgen_prompt over 80 words: {}",
        spec.musicgen_prompt
    );

    // The model is told the track_id in the prompt; it usually echoes it back.
    // We don't fail on a mismatch (creative models drift) but we do log it so a
    // human reading the witness output can sanity-check.
    if spec.track_id.as_str() != track_id.as_str() {
        eprintln!(
            "NOTE: model returned track_id={} but we asked for {} — non-fatal",
            spec.track_id.as_str(),
            track_id.as_str()
        );
    }
}

enum OllamaProbe {
    Unreachable,
    ModelMissing(Vec<String>),
    ModelPresent,
}

async fn probe_ollama(base_url: &str, model: &str) -> OllamaProbe {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(_) => return OllamaProbe::Unreachable,
    };
    let resp = match client.get(&url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return OllamaProbe::Unreachable,
    };
    #[derive(serde::Deserialize)]
    struct Tags { models: Vec<TagModel> }
    #[derive(serde::Deserialize)]
    struct TagModel { name: String }

    let tags: Tags = match resp.json().await {
        Ok(t) => t,
        Err(_) => return OllamaProbe::Unreachable,
    };
    let names: Vec<String> = tags.models.into_iter().map(|m| m.name).collect();
    // Ollama tags can come back as "qwen2.5:7b-instruct" or just "qwen2.5:7b-instruct:latest".
    // Match both.
    if names.iter().any(|n| n == model || n.starts_with(&format!("{model}:"))) {
        OllamaProbe::ModelPresent
    } else {
        OllamaProbe::ModelMissing(names)
    }
}
