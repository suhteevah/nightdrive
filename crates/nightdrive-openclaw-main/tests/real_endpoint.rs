// stage: 1
// expect: real openclaw gateway round-trip via podman exec returns "PONG"

use nightdrive_openclaw_main::{ask_main, GatewayConfig};

#[tokio::test]
#[ignore = "real endpoint — run with `cargo test -p nightdrive-openclaw-main -- --ignored`"]
async fn real_main_round_trip() {
    let cfg = GatewayConfig::from_env().expect("config from env");
    let reply = ask_main(&cfg, "Reply with the single word PONG and nothing else.")
        .await
        .expect("ask_main should succeed");
    assert!(!reply.trim().is_empty(), "reply should be non-empty: {:?}", reply);
    assert!(reply.to_uppercase().contains("PONG"), "expected PONG, got: {:?}", reply);
}
