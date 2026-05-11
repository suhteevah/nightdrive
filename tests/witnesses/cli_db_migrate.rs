// stage: 0
// expect: nightdrive-cli binary, run against a tempdir-scoped config, performs
//         `db migrate` cleanly and emits the expected empty-list output from
//         `tracks list` + `uploads list`
// requires: nightdrive-cli binary built (cargo build --bin nightdrive-cli). Skips
//           with a build-the-binary hint if the exe is missing — that's the only
//           thing this witness can't synthesize itself.
//
// Proves the cli orchestrates AppConfig load + Db::connect_and_migrate + the
// Tracks/Uploads list queries end-to-end against a real on-disk SQLite. The
// witness spawns the actual built binary rather than calling its functions
// in-process so we exercise the argv parsing, dotenv loading, and exit-code
// surface that operators will hit on the real orchestrator host.

use std::path::PathBuf;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_db_migrate_then_lists() {
    let exe = find_cli_binary();
    if !exe.exists() {
        eprintln!(
            "SKIP: cli binary not found at {} — run `cargo build --bin nightdrive-cli` first.",
            exe.display()
        );
        return;
    }

    let tmp = tempfile::tempdir().expect("create tempdir");
    let work_dir = tmp.path().to_path_buf();
    let sqlite_path = work_dir.join("nightdrive.sqlite");
    let tracks_dir = work_dir.join("tracks");
    let config_path = work_dir.join("nightdrive.toml");

    // Generate a minimal-but-complete config that satisfies AppConfig's full
    // section schema. Paths are inside the tempdir so the witness doesn't
    // touch /var/lib/nightdrive.
    let toml = generate_test_config(&work_dir, &sqlite_path, &tracks_dir);
    tokio::fs::write(&config_path, toml)
        .await
        .expect("write test nightdrive.toml");

    // ---- db migrate ------------------------------------------------------
    let out = run_cli(&exe, &config_path, &["db", "migrate"]).await;
    assert!(
        out.status.success(),
        "db migrate must succeed; stdout={} stderr={}",
        out.stdout,
        out.stderr,
    );
    assert!(
        out.stdout.contains("OK"),
        "db migrate stdout must contain 'OK'; got: {}",
        out.stdout
    );
    assert!(
        sqlite_path.exists(),
        "sqlite file must exist post-migrate at {}",
        sqlite_path.display()
    );

    // Verify the schema is what we expect by opening the same DB via the
    // storage crate. Re-running connect_and_migrate is idempotent (sqlx's
    // migrate is no-op on already-applied migrations) so this also proves
    // the migration is hermetic across re-runs.
    let db = nightdrive_storage::Db::connect_and_migrate(&sqlite_path)
        .await
        .expect("storage crate must re-open the cli-migrated db cleanly");
    let initial_tracks = nightdrive_storage::Tracks::list(&db, None)
        .await
        .expect("Tracks::list on fresh db");
    assert!(initial_tracks.is_empty(), "fresh migration must yield 0 tracks");
    drop(db);

    // ---- tracks list (empty) ---------------------------------------------
    let out = run_cli(&exe, &config_path, &["tracks", "list"]).await;
    assert!(
        out.status.success(),
        "tracks list must succeed on fresh db; stderr={}",
        out.stderr
    );
    assert!(
        out.stdout.contains("(no tracks yet)"),
        "fresh tracks list must say '(no tracks yet)'; got: {}",
        out.stdout
    );

    // ---- uploads list (empty) --------------------------------------------
    let out = run_cli(&exe, &config_path, &["uploads", "list"]).await;
    assert!(
        out.status.success(),
        "uploads list must succeed on fresh db; stderr={}",
        out.stderr
    );
    assert!(
        out.stdout.contains("(no uploads yet)"),
        "fresh uploads list must say '(no uploads yet)'; got: {}",
        out.stdout
    );
}

/// Walk up from `CARGO_MANIFEST_DIR` (the witnesses crate) to the workspace
/// root, then locate the cli binary under `target/{debug|release}/`.
fn find_cli_binary() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root is two up from tests/witnesses/")
        .to_path_buf();

    let exe_name = if cfg!(windows) {
        "nightdrive-cli.exe"
    } else {
        "nightdrive-cli"
    };

    // Prefer debug — that's what `cargo build` / `cargo test` keep current as a
    // side effect of building deps. The release binary may be stale (built
    // before recent main.rs edits) and would silently exercise the wrong code
    // path. If a real release-only environment runs this witness, fall back.
    let debug = workspace_root
        .join("target")
        .join("debug")
        .join(exe_name);
    if debug.exists() {
        return debug;
    }
    workspace_root.join("target").join("release").join(exe_name)
}

struct CliOutput {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

async fn run_cli(exe: &std::path::Path, config: &std::path::Path, args: &[&str]) -> CliOutput {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("--config").arg(config).args(args);
    // Strip any inherited NIGHTDRIVE_CONFIG so the explicit --config wins
    // deterministically.
    cmd.env_remove("NIGHTDRIVE_CONFIG");
    let output = cmd.output().await.expect("spawn nightdrive-cli");
    CliOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

fn generate_test_config(
    work_dir: &std::path::Path,
    sqlite_path: &std::path::Path,
    tracks_dir: &std::path::Path,
) -> String {
    // toml-escape backslashes for Windows paths.
    let work = work_dir.display().to_string().replace('\\', "\\\\");
    let sqlite = sqlite_path.display().to_string().replace('\\', "\\\\");
    let tracks = tracks_dir.display().to_string().replace('\\', "\\\\");

    format!(
        r#"[paths]
work_dir = "{work}"
sqlite_db = "{sqlite}"
tracks_dir = "{tracks}"

[openclaw]
base_url = "http://127.0.0.1:11434"
model = "qwen2.5:7b-instruct"
temperature = 0.85
max_tokens = 2048
timeout_seconds = 120

[audio_gen]
base_url = "http://127.0.0.1:8080"
model = "stable-audio-open"
sample_rate = 44100
channels = 2
segment_seconds = 28
overlap_seconds = 2
guidance_scale = 3.0

[art]
base_url = "http://127.0.0.1:8081"
model = "sdxl"
width = 1024
height = 1024
steps = 20
cfg_scale = 7.0
negative_prompt = "text, watermark"

[mastering]
target_lufs = -14.0
true_peak_db = -1.0
loudness_range = 11.0
fade_in_seconds = 2.0
fade_out_seconds = 4.0

[visualizer]
width = 1920
height = 1080
fps = 30
quality_preset = "high"
seed_from_track_id = true
include_code_scroll = false

[encoder]
ffmpeg_path = "ffmpeg"
video_codec = "libx264"
crf = 18
preset = "slow"
audio_codec = "aac"
audio_bitrate = "320k"
intro_seconds = 3
outro_seconds = 3

[youtube]
default_privacy = "private"
default_category_id = "10"
schedule_offset_hours = 24
publish_window_start_hour = 19
publish_window_end_hour = 23
declare_synthetic_content = true

[livestream]
visualizer_ws_port = 7373
metadata_refresh_seconds = 1
shuffle_buffer_size = 12
min_replay_gap_hours = 24

[metrics]
prometheus_port = 9091
"#
    )
}
