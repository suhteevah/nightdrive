// stage: 1
// expect: real openclaw main produces a valid 4-track AlbumSpec for a throwaway theme

use nightdrive_album_composer::{compose, ComposeRequest};
use nightdrive_openclaw_main::GatewayConfig;
use std::path::PathBuf;

#[tokio::test]
#[ignore = "real endpoint — run with `cargo test -p nightdrive-album-composer -- --ignored`"]
async fn real_compose_smoke() {
    let cfg = GatewayConfig::from_env().expect("gateway env present");
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().parent().unwrap().to_path_buf();
    let req = ComposeRequest {
        slug: "test-witness-vol-1".into(),
        theme: "Quiet 1990s parking-garage at 3 AM, fluorescent hum, no people".into(),
        track_count: 4,
        danger_zone_keys: vec![],
        albums_dir: repo_root.join("docs/albums"),
        danger_zone_path: repo_root.join("docs/album-danger-zone.json"),
        max_retries: 3,
    };
    let spec = compose(&cfg, &req).await.expect("compose succeeds");
    assert_eq!(spec.tracks.len(), 4);
    assert_eq!(spec.album_slug, "test-witness-vol-1");
    for t in &spec.tracks {
        assert!((80..=118).contains(&t.bpm), "track BPM out of range: {}", t.bpm);
        assert!((180..=360).contains(&t.duration_seconds), "track duration out of range: {}", t.duration_seconds);
        assert!(!t.title.trim().is_empty());
    }
}
