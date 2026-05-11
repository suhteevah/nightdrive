// stage: 0
// expect: AppConfig::load_from(config/nightdrive.toml.example) succeeds and every section field is typed-correct
// requires: none (pure file IO + toml parse)
//
// Proves crates/nightdrive-core/src/config.rs::AppConfig deserializes the
// canonical nightdrive.toml.example into all 10 typed sections without
// missing fields, panics, or default-fallback drift. The example file is
// the on-disk contract every operator copies to /etc/nightdrive/nightdrive.toml.
// Note: nightdrive.toml.example contains a [storage] section that AppConfig
// does not declare — toml ignores unknown fields by default, which is intentional
// (storage config is owned by nightdrive-storage, not nightdrive-core).
#[tokio::test]
async fn core_loads_real_config() {
    use nightdrive_core::config::AppConfig;
    use std::path::Path;

    // CARGO_MANIFEST_DIR = tests/witnesses  →  .parent() = tests  →  .parent() = repo root
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent().expect("tests dir must exist")
        .parent().expect("repo root must exist")
        .join("config")
        .join("nightdrive.toml.example");

    assert!(path.exists(), "config example not found at {}", path.display());

    let cfg = AppConfig::load_from(&path)
        .expect("config/nightdrive.toml.example must parse without error");

    // [paths]
    assert_eq!(
        cfg.paths.work_dir.to_string_lossy(),
        "/var/lib/nightdrive",
        "paths.work_dir"
    );
    assert_eq!(
        cfg.paths.sqlite_db.to_string_lossy(),
        "/var/lib/nightdrive/nightdrive.sqlite",
        "paths.sqlite_db"
    );
    assert_eq!(
        cfg.paths.tracks_dir.to_string_lossy(),
        "/var/lib/nightdrive/tracks",
        "paths.tracks_dir"
    );

    // [openclaw]
    assert!(
        cfg.openclaw.base_url.starts_with("http://"),
        "openclaw.base_url must be http, got {}",
        cfg.openclaw.base_url
    );
    assert_eq!(cfg.openclaw.model, "qwen2.5:7b-instruct", "openclaw.model");
    assert!((cfg.openclaw.temperature - 0.85_f32).abs() < f32::EPSILON, "openclaw.temperature");
    assert_eq!(cfg.openclaw.max_tokens, 2048, "openclaw.max_tokens");
    assert_eq!(cfg.openclaw.timeout_seconds, 120, "openclaw.timeout_seconds");

    // [audio_gen]
    assert_eq!(cfg.audio_gen.model, "musicgen-large", "audio_gen.model");
    assert_eq!(cfg.audio_gen.sample_rate, 32000, "audio_gen.sample_rate");
    assert_eq!(cfg.audio_gen.channels, 2, "audio_gen.channels");
    assert_eq!(cfg.audio_gen.segment_seconds, 28, "audio_gen.segment_seconds");
    assert_eq!(cfg.audio_gen.overlap_seconds, 2, "audio_gen.overlap_seconds");
    assert!((cfg.audio_gen.guidance_scale - 3.0_f32).abs() < f32::EPSILON, "audio_gen.guidance_scale");

    // [art]
    assert_eq!(cfg.art.model, "sdxl", "art.model");
    assert_eq!(cfg.art.width, 1024, "art.width");
    assert_eq!(cfg.art.height, 1024, "art.height");
    assert_eq!(cfg.art.steps, 30, "art.steps");
    assert!((cfg.art.cfg_scale - 7.0_f32).abs() < f32::EPSILON, "art.cfg_scale");

    // [mastering]
    assert!((cfg.mastering.target_lufs - (-14.0_f32)).abs() < f32::EPSILON, "mastering.target_lufs");
    assert!((cfg.mastering.true_peak_db - (-1.0_f32)).abs() < f32::EPSILON, "mastering.true_peak_db");
    assert!((cfg.mastering.loudness_range - 11.0_f32).abs() < f32::EPSILON, "mastering.loudness_range");
    assert!((cfg.mastering.fade_in_seconds - 2.0_f32).abs() < f32::EPSILON, "mastering.fade_in_seconds");
    assert!((cfg.mastering.fade_out_seconds - 4.0_f32).abs() < f32::EPSILON, "mastering.fade_out_seconds");

    // [visualizer]
    assert_eq!(cfg.visualizer.width, 1920, "visualizer.width");
    assert_eq!(cfg.visualizer.height, 1080, "visualizer.height");
    assert_eq!(cfg.visualizer.fps, 30, "visualizer.fps");
    assert_eq!(cfg.visualizer.quality_preset, "high", "visualizer.quality_preset");
    assert!(cfg.visualizer.seed_from_track_id, "visualizer.seed_from_track_id");
    assert!(!cfg.visualizer.include_code_scroll, "visualizer.include_code_scroll");

    // [encoder]
    assert_eq!(cfg.encoder.video_codec, "libx264", "encoder.video_codec");
    assert_eq!(cfg.encoder.crf, 18, "encoder.crf");
    assert_eq!(cfg.encoder.preset, "slow", "encoder.preset");
    assert_eq!(cfg.encoder.audio_codec, "aac", "encoder.audio_codec");
    assert_eq!(cfg.encoder.audio_bitrate, "320k", "encoder.audio_bitrate");
    assert_eq!(cfg.encoder.intro_seconds, 3, "encoder.intro_seconds");
    assert_eq!(cfg.encoder.outro_seconds, 3, "encoder.outro_seconds");

    // [youtube]
    assert_eq!(cfg.youtube.default_privacy, "private", "youtube.default_privacy");
    assert_eq!(cfg.youtube.default_category_id, "10", "youtube.default_category_id");
    assert_eq!(cfg.youtube.schedule_offset_hours, 24, "youtube.schedule_offset_hours");
    assert_eq!(cfg.youtube.publish_window_start_hour, 19, "youtube.publish_window_start_hour");
    assert_eq!(cfg.youtube.publish_window_end_hour, 23, "youtube.publish_window_end_hour");
    assert!(cfg.youtube.declare_synthetic_content, "youtube.declare_synthetic_content");

    // [livestream]
    assert_eq!(cfg.livestream.visualizer_ws_port, 7373, "livestream.visualizer_ws_port");
    assert_eq!(cfg.livestream.metadata_refresh_seconds, 1, "livestream.metadata_refresh_seconds");
    assert_eq!(cfg.livestream.shuffle_buffer_size, 12, "livestream.shuffle_buffer_size");
    assert_eq!(cfg.livestream.min_replay_gap_hours, 24, "livestream.min_replay_gap_hours");

    // [metrics] — has #[serde(default)] so presence or absence must both yield 9091
    assert_eq!(cfg.metrics.prometheus_port, 9091, "metrics.prometheus_port");
}
