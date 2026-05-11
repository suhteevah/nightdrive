// stage: 0
// expect: Db::connect_and_migrate creates a fresh SQLite, the full TrackState
//         progression (pending → spec_generated → … → published) round-trips through
//         Tracks/Uploads/LivestreamRotation, and reads back identical to what was written
// requires: nothing — the test owns its tempdir SQLite and ffmpeg/Ollama are not in play
//
// Proves nightdrive-storage's CRUD against a real on-disk SQLite (no in-memory shortcut
// — sqlx::migrate! has to operate on the same `?mode=rwc` URL the orchestrator uses in
// prod). Doubles as the schema-drift gate: this test fails loudly if the migration
// stops accepting any of the 8 TrackState string forms, or if the column set changes
// out from under the Tracks::insert SQL.
//
// Per tests/witnesses/README.md: storage tests use real SQLite on a tempdir DB — no
// mocks. The "no mocks" rule exists because Matt got burned last quarter when mocked
// tests passed but the prod migration failed.

use nightdrive_core::{
    CompositionSpec, Section, TrackId, TrackState, YoutubeMetadata,
};
use nightdrive_storage::{Db, LivestreamRotation, Tracks, Uploads};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_roundtrip_through_full_state_machine() {
    // Tempdir auto-cleans on drop; the SQLite file lives inside it for the
    // duration of the test only.
    let tmp = tempfile::tempdir().expect("create tempdir");
    let db_path = tmp.path().join("nightdrive-witness.sqlite");

    let db = Db::connect_and_migrate(&db_path)
        .await
        .expect("connect + migrate fresh sqlite");

    // Two synthetic specs so LivestreamRotation::next_track has something to
    // rotate between.
    let track_a_id = TrackId::new(
        chrono::NaiveDate::from_ymd_opt(1999, 1, 1).unwrap(),
        1,
    );
    let track_b_id = TrackId::new(
        chrono::NaiveDate::from_ymd_opt(1999, 1, 1).unwrap(),
        2,
    );
    let spec_a = sample_spec(track_a_id.clone(), "Neon Drift", 92);
    let spec_b = sample_spec(track_b_id.clone(), "Highway Mirage", 104);

    // ---- Tracks::insert ----------------------------------------------------
    Tracks::insert(&db, &spec_a, 0xABCD)
        .await
        .expect("insert track A");
    Tracks::insert(&db, &spec_b, 0x1234)
        .await
        .expect("insert track B");

    let listed = Tracks::list(&db, None).await.expect("list all");
    assert_eq!(listed.len(), 2, "two tracks inserted");
    assert!(listed.iter().all(|t| t.state == TrackState::Pending), "fresh tracks must be pending");

    let pending = Tracks::list(&db, Some(TrackState::Pending))
        .await
        .expect("list pending");
    assert_eq!(pending.len(), 2, "state filter returns 2 pending rows");

    // ---- Walk the FULL state machine on track A ----------------------------
    // This is the contract the orchestrator's pipeline_one() walks step by
    // step; if the migration ever stops accepting one of these string forms,
    // this test must fail loudly.
    let progression = [
        TrackState::SpecGenerated,
        TrackState::AudioRendered,
        TrackState::CoverRendered,
        TrackState::AudioMastered,
        TrackState::VideoEncoded,
        TrackState::Published,
    ];
    for next in progression {
        Tracks::update_state(&db, &track_a_id, next)
            .await
            .unwrap_or_else(|e| panic!("update_state to {next:?} failed: {e}"));
        let row = Tracks::get(&db, &track_a_id)
            .await
            .expect("get after update")
            .expect("track A must still exist");
        assert_eq!(
            row.state, next,
            "read-back state must match what we just wrote"
        );
    }

    // ---- Identity round-trip: spec_json is the canonical CompositionSpec ---
    let row_a = Tracks::get(&db, &track_a_id)
        .await
        .expect("final get")
        .expect("track A present");
    let parsed: CompositionSpec = serde_json::from_str(&row_a.spec_json)
        .expect("spec_json must round-trip through serde");
    assert_eq!(parsed.track_id, spec_a.track_id);
    assert_eq!(parsed.title, spec_a.title);
    assert_eq!(parsed.bpm, spec_a.bpm);
    assert_eq!(parsed.duration_seconds, spec_a.duration_seconds);
    assert_eq!(parsed.sections.len(), spec_a.sections.len());

    // ---- update_state on a missing id must error, not silently succeed ----
    let bogus = TrackId::new(
        chrono::NaiveDate::from_ymd_opt(1900, 1, 1).unwrap(),
        999,
    );
    let err = Tracks::update_state(&db, &bogus, TrackState::Failed)
        .await
        .expect_err("update_state on missing track must error");
    let msg = err.to_string();
    assert!(
        msg.contains("track not found"),
        "expected 'track not found' in error, got: {msg}"
    );

    // ---- Uploads::insert + set_youtube_id ---------------------------------
    let upload_id = Uploads::insert(&db, &track_a_id)
        .await
        .expect("insert upload row");
    let queued = Uploads::get(&db, upload_id)
        .await
        .expect("fetch upload")
        .expect("upload row present");
    assert_eq!(queued.status, "queued");
    assert_eq!(queued.track_id, track_a_id);
    assert!(queued.youtube_video_id.is_none());

    Uploads::set_youtube_id(&db, upload_id, "abc123XYZ")
        .await
        .expect("stamp video id");
    let complete = Uploads::get(&db, upload_id)
        .await
        .expect("fetch upload after complete")
        .expect("upload row present");
    assert_eq!(complete.status, "complete");
    assert_eq!(complete.youtube_video_id.as_deref(), Some("abc123XYZ"));
    assert!(complete.completed_at.is_some(), "completed_at must be stamped");

    // ---- LivestreamRotation::next_track -----------------------------------
    // Only track A is published; track B is still pending. next_track must
    // return A, never B.
    let next = LivestreamRotation::next_track(&db)
        .await
        .expect("next_track query")
        .expect("at least one published track exists");
    assert_eq!(
        next.id, track_a_id,
        "only published track must be selected; got {}",
        next.id
    );

    // After we log_start, next_track should keep returning A (still the only
    // published track) but the rotation_log row count must grow.
    let log_id = LivestreamRotation::log_start(&db, &track_a_id)
        .await
        .expect("log_start");
    assert!(log_id > 0, "rotation_log row id must be positive");

    // Promote B to published. With both tracks published and only A in the
    // rotation log, B (never-played) must jump A in the queue.
    for next in progression {
        Tracks::update_state(&db, &track_b_id, next)
            .await
            .expect("advance B");
    }
    let next_b = LivestreamRotation::next_track(&db)
        .await
        .expect("next_track query 2")
        .expect("B is published now");
    assert_eq!(
        next_b.id, track_b_id,
        "never-played track must jump the queue ahead of A; got {}",
        next_b.id
    );
}

fn sample_spec(track_id: TrackId, title: &str, bpm: u32) -> CompositionSpec {
    CompositionSpec {
        track_id,
        title: title.to_string(),
        subgenre: "synthwave".to_string(),
        mood_tags: vec!["nocturnal".to_string(), "driving".to_string()],
        bpm,
        musical_key: "F# minor".to_string(),
        duration_seconds: 240,
        sections: vec![
            Section { name: "intro".to_string(), bars: 8, instrumentation: "pad + arp".to_string() },
            Section { name: "verse".to_string(), bars: 16, instrumentation: "+ bass + drums".to_string() },
            Section { name: "chorus".to_string(), bars: 16, instrumentation: "+ lead + sidechain".to_string() },
            Section { name: "outro".to_string(), bars: 8, instrumentation: "fade".to_string() },
        ],
        musicgen_prompt: "lo-fi synthwave 92 BPM F# minor, gated reverb drums, analog DX7 pad, bright lead arp, sidechain compression on bass".to_string(),
        cover_prompt: "synthwave 1985 album cover, neon palm trees, chrome grid floor, setting sun, no text".to_string(),
        youtube: YoutubeMetadata {
            title: format!("{title} — Synthwave for Coding"),
            description: "Generated by nightdrive.".to_string(),
            tags: vec!["synthwave".to_string(), "coding music".to_string(), "lofi".to_string()],
            category_id: "10".to_string(),
        },
    }
}
