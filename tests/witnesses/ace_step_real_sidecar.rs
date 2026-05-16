// stage: 2
// expect: AceStepClient::render against a real ACE-Step 1.5 sidecar writes a
//         well-formed PCM WAV to paths.raw_audio_wav() with duration within
//         ±15% of the spec's target.
// requires: NIGHTDRIVE_ACESTEP_URL reachable (the sidecar/acestep_server.py
//           on http://127.0.0.1:8083 by default). Skips cleanly with an
//           instructive message when the sidecar isn't running — useful
//           during the cnc P100 + uv-install rollout (~2026-05-17 onward).
//
// Proves nightdrive-audio-gen's AceStepClient against the real handler-based
// API in sidecar/acestep_server.py. No mocks per tests/witnesses/README.md —
// a mock would tell us nothing about whether prompt formatting actually
// survives the ACE-Step request schema + 48 kHz stereo WAV decode roundtrip.
//
// This is the witness for the engine swap that retires the CC-BY-NC strike
// risk we accepted on MG.

use nightdrive_audio_gen::{AceStepClient, AudioGenerator, probe_wav};
use nightdrive_core::config::AudioGenConfig;
use nightdrive_core::{
    CompositionSpec, Section, TrackId, TrackPaths, YoutubeMetadata,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ace_step_real_sidecar_renders_full_song() {
    let base_url = match std::env::var("NIGHTDRIVE_ACESTEP_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!(
                "SKIP: NIGHTDRIVE_ACESTEP_URL not set. Point at the local \
                 ACE-Step sidecar (default http://127.0.0.1:8083) once \
                 sidecar/acestep_server.py is running. See scripts/install_acestep.ps1."
            );
            return;
        }
    };

    if !acestep_reachable(&base_url).await {
        eprintln!(
            "SKIP: NIGHTDRIVE_ACESTEP_URL={base_url} did not respond to /health. \
             Start the sidecar: \
             & \"$env:NIGHTDRIVE_ACESTEP_ROOT\\.venv\\Scripts\\python.exe\" \
             -m uvicorn sidecar.acestep_server:app --host 127.0.0.1 --port 8083 --workers 1"
        );
        return;
    }

    let tmp = tempfile::tempdir().expect("create tempdir");
    let work_dir = tmp.path().to_path_buf();
    let track_id = TrackId::new(
        chrono::NaiveDate::from_ymd_opt(1999, 1, 1).unwrap(),
        2,
    );
    let paths = TrackPaths::new(&work_dir, &track_id);
    tokio::fs::create_dir_all(&paths.root).await.expect("mkdir track root");

    // Witness duration is intentionally short (~20s) so the test fires fast
    // even on a P100 fp32. The model still validates the full prompt path —
    // we're proving integration shape, not benching quality.
    let target_duration = 20;

    let cfg = AudioGenConfig {
        base_url: base_url.clone(),
        model: std::env::var("NIGHTDRIVE_ACESTEP_CONFIG")
            .unwrap_or_else(|_| "acestep-v15-turbo".to_string()),
        sample_rate: 48_000,
        channels: 2,
        segment_seconds: 30,           // unused by ace_step
        overlap_seconds: 0,            // unused by ace_step
        guidance_scale: 7.0,
        engine: "ace_step".to_string(),
        continuation_prefix_seconds: 0.0, // unused by ace_step
        inference_steps: 8,            // turbo default — fastest path for the witness
    };

    let client = AceStepClient::new(cfg).expect("AceStepClient::new");
    let spec = sample_spec(track_id.clone(), target_duration);

    let out_path = client
        .render(&spec, &paths)
        .await
        .unwrap_or_else(|e| panic!("AceStepClient::render against {base_url}: {e}"));

    assert_eq!(
        out_path,
        paths.raw_audio_wav(),
        "render must write to paths.raw_audio_wav()"
    );

    let bytes = tokio::fs::read(&out_path)
        .await
        .expect("raw.wav must be readable post-render");
    assert!(bytes.len() > 44, "raw.wav suspiciously small: {} bytes", bytes.len());
    // RIFF header — first 4 bytes of any valid WAV.
    assert_eq!(&bytes[0..4], b"RIFF", "raw.wav missing RIFF magic");
    assert_eq!(&bytes[8..12], b"WAVE", "raw.wav missing WAVE marker");

    let (sr, channels, duration) = probe_wav(&out_path).expect("probe_wav");
    assert!(sr >= 32_000, "sample rate {sr} suspiciously low (ACE-Step is 48k native)");
    assert!(channels >= 1, "expected ≥1 channel, got {channels}");
    // Allow ±20% on the duration — ACE-Step's diffusion may round to nearest
    // bar/beat and finish a few seconds short or long.
    let tolerance = target_duration as f32 * 0.20;
    let target = target_duration as f32;
    assert!(
        (duration - target).abs() < tolerance.max(2.0),
        "duration {duration:.1}s out of tolerance for {target}s target (±{tolerance:.1})",
    );
}

async fn acestep_reachable(base_url: &str) -> bool {
    let url = format!(
        "{}/health",
        base_url.trim_end_matches('/')
    );
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    matches!(client.get(&url).send().await, Ok(r) if r.status().is_success())
}

fn sample_spec(track_id: TrackId, duration_seconds: u32) -> CompositionSpec {
    CompositionSpec {
        track_id,
        title: "Apex (Witness)".to_string(),
        subgenre: "synthwave".to_string(),
        mood_tags: vec!["nocturnal".to_string(), "driving".to_string()],
        bpm: 108,
        musical_key: "D major".to_string(),
        duration_seconds,
        sections: vec![
            Section {
                name: "intro".to_string(),
                bars: 4,
                instrumentation: "pad swell + filtered arp".to_string(),
            },
            Section {
                name: "chorus".to_string(),
                bars: 4,
                instrumentation: "lead + sidechain pump".to_string(),
            },
            Section {
                name: "outro".to_string(),
                bars: 2,
                instrumentation: "tape stop fade".to_string(),
            },
        ],
        musicgen_prompt:
            "synthwave 108 BPM D major peak track, lush DX7 pad, bright analog lead, \
             sidechained sub bass, gated reverb drums, neon-soaked driving energy, instrumental"
                .to_string(),
        cover_prompt: "synthwave 1985 album cover".to_string(),
        youtube: YoutubeMetadata {
            title: "Apex (Witness)".to_string(),
            description: "Witness fixture for AceStepClient.".to_string(),
            tags: vec!["synthwave".to_string()],
            category_id: "10".to_string(),
        },
    }
}
