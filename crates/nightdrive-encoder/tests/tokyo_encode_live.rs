//! Live verification of the per-album weather path: encode a `nd-tokyo-*`
//! track end to end through `FfmpegEncoder::compose`, exercising the real
//! Open-Meteo forecast fetch + RainViewer radar GIF build + the pre-styled
//! (no-negate) radar overlay. Reuses an existing master.flac/cover.png so no
//! GPU audio gen is needed.
//!
//! `#[ignore]` — opt in explicitly. Hits live endpoints (Open-Meteo,
//! RainViewer, CARTO) and needs ffmpeg + the VT323 font, so it only runs on a
//! configured host:
//!
//! ```bash
//! NIGHTDRIVE_TEST_SRC=/var/lib/nightdrive/tracks/nd-sovetskiy-drive-vol-1-001 \
//! NIGHTDRIVE_TEST_WORK=/opt/nightdrive-ws/scratch/tokyo-verify \
//! NIGHTDRIVE_TEST_FONT=/opt/nightdrive/assets/fonts/VT323-Regular.ttf \
//!   cargo test -p nightdrive-encoder --release --test tokyo_encode_live -- --ignored --nocapture
//! ```

use std::path::PathBuf;

use nightdrive_core::{CompositionSpec, TrackId, TrackPaths};
use nightdrive_core::config::EncoderConfig;
use nightdrive_encoder::{FfmpegEncoder, FinalEncoder};

#[tokio::test]
#[ignore = "live endpoints + ffmpeg; run explicitly with --ignored on a configured host"]
async fn tokyo_track_encodes_with_japanese_weather() {
    let src = PathBuf::from(
        std::env::var("NIGHTDRIVE_TEST_SRC")
            .expect("set NIGHTDRIVE_TEST_SRC to an existing track dir with master.flac + cover.png"),
    );
    let work = PathBuf::from(
        std::env::var("NIGHTDRIVE_TEST_WORK").unwrap_or_else(|_| "/tmp/tokyo-verify".to_string()),
    );
    let font = PathBuf::from(
        std::env::var("NIGHTDRIVE_TEST_FONT")
            .unwrap_or_else(|_| "/opt/nightdrive/assets/fonts/VT323-Regular.ttf".to_string()),
    );

    let id = TrackId("nd-tokyo-cyberpunk-vol-1-001".to_string());
    let paths = TrackPaths::new(&work, &id);
    tokio::fs::create_dir_all(&paths.root).await.unwrap();

    // Reuse an existing master + cover so we skip audio gen / cover gen.
    tokio::fs::copy(src.join("master.flac"), paths.master_flac())
        .await
        .expect("copy master.flac");
    tokio::fs::copy(src.join("cover.png"), paths.cover_png())
        .await
        .expect("copy cover.png");

    // Borrow an existing valid spec, retarget it to the Tokyo album.
    let spec_json = tokio::fs::read(src.join("spec.json")).await.expect("read source spec.json");
    let mut spec: CompositionSpec =
        serde_json::from_slice(&spec_json).expect("parse source spec.json into CompositionSpec");
    spec.track_id = id.clone();
    spec.title = "Neon Shrine, 0300".to_string();

    let cfg = EncoderConfig {
        ffmpeg_path: PathBuf::from(std::env::var("NIGHTDRIVE_TEST_FFMPEG").unwrap_or_else(|_| "ffmpeg".into())),
        video_codec: "libx264".into(),
        crf: 20,
        preset: "veryfast".into(),
        audio_codec: "aac".into(),
        audio_bitrate: "320k".into(),
        intro_seconds: 1,
        outro_seconds: 1,
        font_path: font,
        cta_text: "LIKE • SUBSCRIBE".into(),
    };

    let encoder = FfmpegEncoder::new(cfg);
    let out = encoder
        .compose(&paths, &spec)
        .await
        .expect("compose tokyo track");

    assert!(out.exists(), "final.mp4 must exist at {}", out.display());
    let meta = std::fs::metadata(&out).unwrap();
    assert!(meta.len() > 50_000, "final.mp4 suspiciously small: {} bytes", meta.len());

    // The weather archive should record Tokyo + Open-Meteo + a pre-styled radar.
    let forecast_json = std::fs::read_to_string(paths.root.join("forecast.json")).unwrap();
    println!("--- forecast.json ---\n{forecast_json}\n---");
    assert!(forecast_json.contains("TOKYO"), "forecast should be the Tokyo region");
    assert!(
        forecast_json.contains("open_meteo") || forecast_json.contains("\"source\": \"open_meteo\""),
        "forecast source should be Open-Meteo"
    );

    println!("OK tokyo encode -> {}  ({} bytes)", out.display(), meta.len());
    println!("radar.gif present: {}", paths.root.join("radar.gif").exists());
}
