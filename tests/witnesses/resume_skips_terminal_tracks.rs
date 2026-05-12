// stage: 0
// expect: `nightdrive-orchestrator resume`, run against a tempdir-scoped DB
//         pre-populated with one Published, one Failed, and one VideoEncoded
//         track row, leaves the two terminal rows untouched, marks the
//         non-terminal row Failed when its only remaining stage (upload) is
//         denied via stripped credentials, and exits 0.
// requires: nightdrive-orchestrator binary built (cargo build --bin
//           nightdrive-orchestrator). Skips with a build-the-binary hint
//           if the exe is missing.
//
// Proves three things about the resume subcommand:
//   1. The list-non-terminal predicate correctly identifies the row at
//      `video_encoded` and ignores `published` / `failed` (the latter
//      stay at their initial state regardless of what resume tries to do).
//   2. Per-track failures don't abort the resume — the binary exits 0
//      even though the VideoEncoded row's upload stage errored out
//      (NIGHTDRIVE_YT_* env vars are stripped so
//      YoutubeCredentials::from_env() returns Err).
//   3. The catch-and-continue mark-Failed path actually flips the row to
//      `failed`, which is what `resume` itself observes on a follow-up.
//
// Per tests/witnesses/README.md: real SQLite on a tempdir DB, real binary
// spawned as a subprocess, no mocks. The stripped-creds approach exercises
// a deterministic upload-stage failure without needing fake YouTube — the
// error originates in our own [`YoutubeCredentials::from_env`].

use nightdrive_core::{
    CompositionSpec, Section, TrackId, TrackState, YoutubeMetadata,
};
use nightdrive_storage::{Db, Tracks};
use std::path::PathBuf;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_skips_terminal_tracks_and_marks_failed_on_upload_error() {
    let exe = find_orchestrator_binary();
    if !exe.exists() {
        eprintln!(
            "SKIP: orchestrator binary not found at {} — run `cargo build --bin nightdrive-orchestrator` first.",
            exe.display()
        );
        return;
    }

    let tmp = tempfile::tempdir().expect("create tempdir");
    let work_dir = tmp.path().to_path_buf();
    let sqlite_path = work_dir.join("nightdrive.sqlite");
    let tracks_dir = work_dir.join("tracks");
    let config_path = work_dir.join("nightdrive.toml");

    let toml = generate_test_config(&work_dir, &sqlite_path, &tracks_dir);
    tokio::fs::write(&config_path, toml)
        .await
        .expect("write test nightdrive.toml");

    // ---- Pre-populate the DB ---------------------------------------------
    // Open via the storage crate to inherit the migration + WAL setup the
    // orchestrator will use. Three tracks: Published (terminal), Failed
    // (terminal), VideoEncoded (non-terminal, only stage 7 left).
    let db = Db::connect_and_migrate(&sqlite_path)
        .await
        .expect("connect + migrate fresh sqlite");

    let date = chrono::NaiveDate::from_ymd_opt(1999, 12, 31).unwrap();
    let id_published = TrackId::new(date, 1);
    let id_failed = TrackId::new(date, 2);
    let id_videoencoded = TrackId::new(date, 3);

    let spec_published = sample_spec(id_published.clone(), "Already Shipped", 92);
    let spec_failed = sample_spec(id_failed.clone(), "DOA", 100);
    let spec_videoencoded = sample_spec(id_videoencoded.clone(), "Almost There", 96);

    Tracks::insert(&db, &spec_published, 0x1)
        .await
        .expect("insert published track");
    Tracks::update_state(&db, &id_published, TrackState::Published)
        .await
        .expect("set published");

    Tracks::insert(&db, &spec_failed, 0x2)
        .await
        .expect("insert failed track");
    Tracks::update_state(&db, &id_failed, TrackState::Failed)
        .await
        .expect("set failed");

    Tracks::insert(&db, &spec_videoencoded, 0x3)
        .await
        .expect("insert videoencoded track");
    Tracks::update_state(&db, &id_videoencoded, TrackState::VideoEncoded)
        .await
        .expect("set videoencoded");

    // Drop the connection so the orchestrator's process can open it without
    // contending on the SQLite WAL writer.
    drop(db);

    // ---- Spawn nightdrive-orchestrator resume ----------------------------
    // Strip YT credentials so the VideoEncoded row's stage-7 attempt errors
    // out at YoutubeCredentials::from_env() — deterministic without faking
    // the YouTube API itself.
    let out = run_orchestrator(&exe, &config_path, &["resume"]).await;
    assert!(
        out.status.success(),
        "resume must exit 0 even when a per-track resume fails; stdout={} stderr={}",
        out.stdout,
        out.stderr,
    );
    assert!(
        out.stderr.contains("resuming non-terminal tracks") || out.stdout.contains("resuming non-terminal tracks"),
        "expected the 'resuming non-terminal tracks' log line; stdout={} stderr={}",
        out.stdout,
        out.stderr,
    );

    // ---- Verify the post-resume state machine ----------------------------
    let db = Db::connect_and_migrate(&sqlite_path)
        .await
        .expect("re-open after resume");

    let row_published = Tracks::get(&db, &id_published)
        .await
        .expect("get published")
        .expect("published row present");
    assert_eq!(
        row_published.state,
        TrackState::Published,
        "Published row must be untouched; got {:?}",
        row_published.state
    );

    let row_failed = Tracks::get(&db, &id_failed)
        .await
        .expect("get failed")
        .expect("failed row present");
    assert_eq!(
        row_failed.state,
        TrackState::Failed,
        "Failed row must be untouched; got {:?}",
        row_failed.state
    );

    let row_videoencoded = Tracks::get(&db, &id_videoencoded)
        .await
        .expect("get videoencoded")
        .expect("videoencoded row present");
    assert_eq!(
        row_videoencoded.state,
        TrackState::Failed,
        "VideoEncoded row must flip to Failed after upload-stage error; got {:?}",
        row_videoencoded.state
    );
}

fn find_orchestrator_binary() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root is two up from tests/witnesses/")
        .to_path_buf();

    let exe_name = if cfg!(windows) {
        "nightdrive-orchestrator.exe"
    } else {
        "nightdrive-orchestrator"
    };
    let debug = workspace_root.join("target").join("debug").join(exe_name);
    if debug.exists() {
        return debug;
    }
    workspace_root.join("target").join("release").join(exe_name)
}

struct CmdOutput {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

async fn run_orchestrator(
    exe: &std::path::Path,
    config: &std::path::Path,
    args: &[&str],
) -> CmdOutput {
    let mut cmd = tokio::process::Command::new(exe);
    cmd.arg("--config").arg(config).args(args);
    // Strip everything that would let YoutubeCredentials::from_env() succeed
    // and turn this into an actual upload attempt against real YouTube.
    cmd.env_remove("NIGHTDRIVE_CONFIG");
    cmd.env_remove("NIGHTDRIVE_YT_CLIENT_ID");
    cmd.env_remove("NIGHTDRIVE_YT_CLIENT_SECRET");
    cmd.env_remove("NIGHTDRIVE_YT_REFRESH_TOKEN");
    let output = cmd.output().await.expect("spawn nightdrive-orchestrator");
    CmdOutput {
        status: output.status,
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}

fn sample_spec(track_id: TrackId, title: &str, bpm: u32) -> CompositionSpec {
    CompositionSpec {
        track_id,
        title: title.to_string(),
        subgenre: "synthwave".to_string(),
        mood_tags: vec!["nocturnal".to_string()],
        bpm,
        musical_key: "F# minor".to_string(),
        duration_seconds: 240,
        sections: vec![Section {
            name: "intro".to_string(),
            bars: 8,
            instrumentation: "pad".to_string(),
        }],
        musicgen_prompt: format!("synthwave {bpm} BPM F# minor"),
        cover_prompt: "synthwave 1985 album cover".to_string(),
        youtube: YoutubeMetadata {
            title: format!("{title} — Synthwave for Coding"),
            description: "Test fixture.".to_string(),
            tags: vec!["synthwave".to_string()],
            category_id: "10".to_string(),
        },
    }
}

fn generate_test_config(
    work_dir: &std::path::Path,
    sqlite_path: &std::path::Path,
    tracks_dir: &std::path::Path,
) -> String {
    let work = work_dir.display().to_string().replace('\\', "\\\\");
    let sqlite = sqlite_path.display().to_string().replace('\\', "\\\\");
    let tracks = tracks_dir.display().to_string().replace('\\', "\\\\");
    format!(
        r#"[paths]
work_dir = "{work}"
sqlite_db = "{sqlite}"
tracks_dir = "{tracks}"

[openclaw]
base_url = "http://127.0.0.1:1"
model = "qwen2.5:7b-instruct"
temperature = 0.85
max_tokens = 2048
timeout_seconds = 120

[audio_gen]
base_url = "http://127.0.0.1:1"
model = "stable-audio-open"
sample_rate = 44100
channels = 2
segment_seconds = 28
overlap_seconds = 2
guidance_scale = 3.0

[art]
base_url = "http://127.0.0.1:1"
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
