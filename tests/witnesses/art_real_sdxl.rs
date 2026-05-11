// stage: 3
// expect: SdxlClient::render produces a PNG at paths.cover_png(), starting with
//         the PNG signature and reporting 1024x1024 in its IHDR chunk
// requires: NIGHTDRIVE_ART_URL reachable (a stable-diffusion-webui / Forge / A1111-
//           compatible HTTP endpoint at `/sdapi/v1/txt2img`). Skips cleanly with
//           an instructive message when the sidecar isn't deployed — useful
//           until the cnc P100s land (~2026-05-17) and the SDXL container is up.
//
// Proves nightdrive-art's CoverArtist trait + SdxlClient impl against a real SDXL
// endpoint. No mocks per tests/witnesses/README.md — a mocked SDXL would tell us
// nothing about whether the actual sidecar will accept our `txt2img` request shape.
//
// Probe order: env URL set -> /sdapi/v1/sd-models 200 -> proceed. Each skip
// branch prints a one-line reason so a developer running `cargo test` against
// a partially-set-up fleet can see exactly which precondition is missing.

use nightdrive_art::{CoverArtist, SdxlClient, parse_png_dimensions};
use nightdrive_core::config::ArtConfig;
use nightdrive_core::{
    CompositionSpec, Section, TrackId, TrackPaths, YoutubeMetadata,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn art_real_sdxl_renders_cover() {
    let base_url = match std::env::var("NIGHTDRIVE_ART_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!(
                "SKIP: NIGHTDRIVE_ART_URL not set. Point at a running \
                 stable-diffusion-webui / Forge / A1111 instance (e.g. \
                 http://cnc-server.tailb85819.ts.net:8081) once the SDXL \
                 sidecar is deployed post-P100s."
            );
            return;
        }
    };

    if !sdxl_reachable(&base_url).await {
        eprintln!(
            "SKIP: NIGHTDRIVE_ART_URL={base_url} did not respond to \
             /sdapi/v1/sd-models. Verify the sidecar is up + reachable over \
             Tailscale."
        );
        return;
    }

    let tmp = tempfile::tempdir().expect("create tempdir");
    let work_dir = tmp.path().to_path_buf();
    let track_id = TrackId::new(
        chrono::NaiveDate::from_ymd_opt(1999, 1, 1).unwrap(),
        3,
    );
    let paths = TrackPaths::new(&work_dir, &track_id);
    tokio::fs::create_dir_all(&paths.root).await.expect("mkdir track root");

    let cfg = ArtConfig {
        base_url: base_url.clone(),
        model: std::env::var("NIGHTDRIVE_ART_MODEL").unwrap_or_else(|_| "sdxl".to_string()),
        width: 1024,
        height: 1024,
        // Witness keeps the step count modest — we're proving the integration
        // works end-to-end, not benching quality. A1111 default sampler at 20
        // steps for SDXL produces a recognizable image without burning minutes.
        steps: 20,
        cfg_scale: 7.0,
        negative_prompt: "text, watermark, signature, blurry, lowres, deformed".to_string(),
    };

    let client = SdxlClient::new(cfg).expect("SdxlClient::new");
    let spec = sample_spec(track_id.clone());

    let out_path = client
        .render(&spec, &paths)
        .await
        .unwrap_or_else(|e| panic!("SdxlClient::render against {base_url}: {e}"));

    assert_eq!(out_path, paths.cover_png(), "render must write to paths.cover_png()");

    let bytes = tokio::fs::read(&out_path)
        .await
        .expect("cover.png must be readable post-render");
    assert!(bytes.len() > 1024, "cover.png suspiciously small: {} bytes", bytes.len());

    let (w, h) = parse_png_dimensions(&bytes)
        .expect("cover.png must have a valid PNG signature + IHDR");
    assert_eq!(w, 1024, "cover.png width must be 1024");
    assert_eq!(h, 1024, "cover.png height must be 1024");
}

async fn sdxl_reachable(base_url: &str) -> bool {
    let url = format!(
        "{}/sdapi/v1/sd-models",
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

fn sample_spec(track_id: TrackId) -> CompositionSpec {
    CompositionSpec {
        track_id,
        title: "Neon Drift on Highway 9".to_string(),
        subgenre: "synthwave".to_string(),
        mood_tags: vec!["nocturnal".to_string(), "driving".to_string()],
        bpm: 92,
        musical_key: "F# minor".to_string(),
        duration_seconds: 240,
        sections: vec![Section {
            name: "intro".to_string(),
            bars: 8,
            instrumentation: "pad + arp".to_string(),
        }],
        musicgen_prompt: "synthwave instrumental, 92 BPM, F# minor".to_string(),
        cover_prompt: "synthwave 1985 album cover, neon palm trees, chrome grid floor, \
                       setting sun reflecting on wet pavement, F#m mood"
            .to_string(),
        youtube: YoutubeMetadata {
            title: "Neon Drift on Highway 9 — Synthwave for Coding".to_string(),
            description: "Synthwave for late-night coding sessions.".to_string(),
            tags: vec!["synthwave".to_string()],
            category_id: "10".to_string(),
        },
    }
}
