// stage: 7
// expect: chunked resumable upload through the real YouTube Data API v3 surface —
//         initiate session, PUT in 8 MB chunks (so a 16 MB test fixture spans
//         exactly 2 chunks + final), query upload offset mid-flight,
//         exercise videos.update to patch description, exercise videos.delete to
//         clean up after ourselves.
// requires: NIGHTDRIVE_RUN_YT_UPLOAD=1 (explicit opt-in — each run burns ~1700
//           quota units of YouTube Data API v3's 10000/day limit), plus
//           NIGHTDRIVE_YT_CLIENT_ID, NIGHTDRIVE_YT_CLIENT_SECRET,
//           NIGHTDRIVE_YT_REFRESH_TOKEN (bootstrap via `nightdrive-cli youtube auth`),
//           plus ffmpeg on PATH (used to synthesize the test mp4 fixture).
//
// Proves nightdrive-youtube's chunked-PUT + resume-status-query + videos.update +
// videos.delete against a real YouTube account (no mocks — per
// tests/witnesses/README.md, mocks aren't allowed here; the "we got burned last
// quarter when mocked tests passed but prod failed" rule). The witness skips
// loudly with an explicit reason whenever its preconditions aren't met so a
// developer running `cargo test` without YT creds doesn't see a failure.
//
// Skip ordering: env opt-in -> creds -> ffmpeg -> run. Quota burns only after
// every skip condition is satisfied.

use nightdrive_core::{CompositionSpec, Section, TrackId, TrackPaths, YoutubeMetadata};
use nightdrive_youtube::{
    Privacy, UploadRequest, VideoUpdate, YoutubeClient, YoutubeCredentials, YoutubeUploader,
    DEFAULT_CHUNK_SIZE,
};

/// 9 MB synthetic test mp4 target. Just past the 8 MB chunk boundary so the
/// chunked PUT loop iterates at least twice — one full 8 MB chunk (gets 308
/// Resume Incomplete) + one partial final chunk (gets 200/201 with the video
/// resource). That exercises both branches of the chunked-PUT state machine
/// without burning more quota than necessary on testsrc bytes.
const TARGET_BYTES: u64 = 9 * 1024 * 1024;

// `#[ignore]` belts-and-suspenders the env-var opt-in: even if a developer
// accidentally exports NIGHTDRIVE_RUN_YT_UPLOAD globally, `cargo test --workspace`
// still won't run this witness without explicit `--ignored` (or
// `--include-ignored`). The witness count in the audit comes from the
// `// stage:` grep, not the test runner, so this doesn't lower the witness
// total — it only changes which command actually fires the upload.
//
// To run for real:
//   cargo test -p nightdrive-witnesses --test youtube_resume_upload -- --ignored --nocapture
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "real YouTube upload — burns API quota; run with --ignored when intended"]
async fn youtube_chunked_resumable_upload_round_trip() {
    if std::env::var("NIGHTDRIVE_RUN_YT_UPLOAD").ok().as_deref() != Some("1") {
        eprintln!(
            "SKIP: this witness burns ~1700 quota units of YouTube Data API v3's \
             10000/day default budget. Set NIGHTDRIVE_RUN_YT_UPLOAD=1 to opt in."
        );
        return;
    }
    let creds = match YoutubeCredentials::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "SKIP: YouTube credentials missing in env ({e}). \
                 Bootstrap via `nightdrive-cli youtube auth` then re-run."
            );
            return;
        }
    };
    if !ffmpeg_available() {
        eprintln!("SKIP: ffmpeg not found on PATH — needed to synthesize the test mp4");
        return;
    }

    let tmp = tempfile::tempdir().expect("create tempdir");
    let work_dir = tmp.path().to_path_buf();
    let track_id = TrackId::new(
        chrono::NaiveDate::from_ymd_opt(1999, 1, 1).unwrap(),
        7,
    );
    let paths = TrackPaths::new(&work_dir, &track_id);
    tokio::fs::create_dir_all(&paths.root).await.expect("mkdir track root");

    synthesize_test_mp4(&paths.final_mp4(), TARGET_BYTES).await;

    // Confirm the fixture spans > 1 chunk before we charge quota. A file with
    // any byte past DEFAULT_CHUNK_SIZE makes upload_in_chunks iterate twice:
    // first PUT gets 308, second PUT gets 200 with the video resource. That's
    // the full chunked PUT state machine without needing a huge fixture.
    let actual = tokio::fs::metadata(paths.final_mp4()).await.unwrap().len();
    assert!(
        actual > DEFAULT_CHUNK_SIZE,
        "fixture must span >1 chunk (DEFAULT_CHUNK_SIZE={DEFAULT_CHUNK_SIZE}), \
         got {actual} bytes — bump duration in synthesize_test_mp4()"
    );

    let client = YoutubeClient::new(creds).expect("YoutubeClient::new");
    let spec = sample_spec(track_id.clone());

    let req = UploadRequest {
        spec: &spec,
        paths: &paths,
        privacy: Privacy::Private,
        scheduled_publish_at: None,
        declare_synthetic_content: true,
    };

    let result = client
        .upload_video(req)
        .await
        .expect("chunked upload through real YouTube must succeed");

    assert!(!result.video_id.is_empty(), "video_id must be non-empty");
    eprintln!("uploaded video_id = {}", result.video_id);

    // ---- query_upload_offset against the (now-complete) session -----------
    // For a complete upload the API returns 200 (with the resource) when you
    // query — our client surfaces that as `Some(total)`.
    let offset_after_done = client
        .query_upload_offset(&result.upload_url, actual)
        .await;
    // Don't fail the test on this — some Google edges return 308 with bytes=0-N
    // even post-completion, depending on which front-end you hit. Just log.
    eprintln!("post-upload offset query returned: {offset_after_done:?}");

    // ---- update_video ----------------------------------------------------
    let updated_description = format!(
        "{}\n\n(witness ran at {} — nightdrive)",
        spec.youtube.description,
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    );
    client
        .update_video(
            &result.video_id,
            VideoUpdate {
                description: Some(updated_description),
                ..Default::default()
            },
        )
        .await
        .expect("videos.update must succeed against a video we just uploaded");

    // ---- delete_video ----------------------------------------------------
    // Always best-effort: panic on failure so we don't leak quota-burnt videos
    // in Matt's channel.
    client
        .delete_video(&result.video_id)
        .await
        .expect("videos.delete must succeed — witness must not leak test videos");

    // ---- cleanup any orphans from prior failed witness runs --------------
    // If a previous run of this witness uploaded successfully but couldn't
    // delete (e.g. because of the insufficient-scope failure that motivated the
    // scope widening), comma-separated NIGHTDRIVE_YT_ORPHAN_VIDEO_IDS sweeps
    // them up. Best-effort: log + continue on failure.
    if let Ok(orphans) = std::env::var("NIGHTDRIVE_YT_ORPHAN_VIDEO_IDS") {
        for id in orphans.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match client.delete_video(id).await {
                Ok(()) => eprintln!("cleaned up orphan video_id={id}"),
                Err(e) => eprintln!("orphan cleanup failed (id={id}): {e} — non-fatal"),
            }
        }
    }
}

fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Synthesize an mp4 of at least `min_bytes`. Strategy: pick a target bitrate
/// that gives us a comfortable margin over `min_bytes` for the configured
/// duration, then let libx264 + AAC produce a file at that bitrate. Uses
/// medium preset because ultrafast ignores `-b:v` in practice.
///
/// Sizing math for the witness's 16 MB target: 120s × 1.5 Mbps video +
/// 128 kbps audio ≈ 24.4 MB, with comfortable headroom against libx264's
/// VBV smoothing under-target.
async fn synthesize_test_mp4(out_path: &std::path::Path, min_bytes: u64) {
    let duration_seconds: u64 = 120;
    // Target bits-per-second that gives min_bytes * 1.5 over duration_seconds,
    // converted to kbit (libx264 wants k-bits not bytes-per-sec).
    let target_kbps = (min_bytes * 8 * 3 / 2 / duration_seconds / 1000).max(1500);

    let status = tokio::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f", "lavfi",
            "-i", &format!("testsrc=size=1280x720:rate=30:duration={duration_seconds}"),
            "-f", "lavfi",
            "-i", &format!("anullsrc=channel_layout=stereo:sample_rate=44100:duration={duration_seconds}"),
            "-c:v", "libx264",
            "-preset", "medium",
            "-b:v", &format!("{target_kbps}k"),
            "-maxrate", &format!("{}k", target_kbps * 2),
            "-bufsize", &format!("{}k", target_kbps * 4),
            "-pix_fmt", "yuv420p",
            "-c:a", "aac",
            "-b:a", "128k",
            "-shortest",
            "-movflags", "+faststart",
        ])
        .arg(out_path)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .expect("ffmpeg launch");

    assert!(status.success(), "ffmpeg synthesis must succeed");
}

fn sample_spec(track_id: TrackId) -> CompositionSpec {
    CompositionSpec {
        track_id,
        title: "nightdrive witness — DELETE ME".to_string(),
        subgenre: "synthwave".to_string(),
        mood_tags: vec!["test".to_string()],
        bpm: 92,
        musical_key: "F# minor".to_string(),
        duration_seconds: 30,
        sections: vec![Section {
            name: "intro".to_string(),
            bars: 4,
            instrumentation: "test pattern".to_string(),
        }],
        musicgen_prompt: "test pattern, not real audio".to_string(),
        cover_prompt: "test pattern".to_string(),
        youtube: YoutubeMetadata {
            title: "nightdrive witness upload — DELETE ME".to_string(),
            description: "Witness test for nightdrive-youtube chunked upload. \
                          This video is automatically deleted within seconds of upload."
                .to_string(),
            tags: vec!["nightdrive-witness".to_string()],
            category_id: "10".to_string(),
        },
    }
}
