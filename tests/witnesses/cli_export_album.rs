// stage: 0
// expect: `nightdrive-cli export album --slug <slug>` reads a real album JSON
//         + walks a real tempdir-staged track tree + writes
//         `exports/<slug>/<NN> - <Title>.flac` for every track that has a
//         master.flac, copies covers, drops a README.txt. No DB required.
// requires: the nightdrive-cli binary built (debug or release). Witness
//          locates it via CARGO_MANIFEST_DIR/../../target/{debug|release}/.
//
// Proves the end-to-end export bundle shape without needing demucs, an album
// in production, or any prior tracks on disk. Pure file-shuffle witness —
// the orchestrator and audio-gen aren't involved.

use std::path::Path;
use tokio::process::Command;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_export_album_bundles_flac_and_cover() {
    let Some(bin) = find_cli_binary() else {
        eprintln!(
            "SKIP: nightdrive-cli binary not found at \
             target/{{debug,release}}/nightdrive-cli(.exe). Run `cargo build` \
             at the workspace root before running this witness."
        );
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let workspace = tmp.path();

    // Stage a fake album JSON at docs/albums/witness-album.json relative to
    // the workspace. The CLI's `export album` reads relative to CWD, so we
    // run it with `workspace` as cwd.
    let album_slug = "witness-export-album";
    let album_json_dir = workspace.join("docs").join("albums");
    tokio::fs::create_dir_all(&album_json_dir).await.unwrap();
    let album_json = serde_json::json!({
        "album_slug": album_slug,
        "title": "Witness Album",
        "theme": "test",
        "track_count": 2,
        "tracks": [
            {
                "track_number": 1,
                "title": "First Witness Track",
                "role": "opener",
                "key": "A minor",
                "bpm": 88,
                "duration_seconds": 240,
                "mood_tags": [],
                "sections": [],
                "musicgen_prompt": "synthwave",
                "cover_prompt": "synthwave cover",
                "key_relationship_to_prior": "—",
                "tempo_relationship_to_prior": "—",
                "composer_notes": ""
            },
            {
                "track_number": 2,
                "title": "Second Witness / Track",
                "role": "closer",
                "key": "C major",
                "bpm": 100,
                "duration_seconds": 240,
                "mood_tags": [],
                "sections": [],
                "musicgen_prompt": "synthwave",
                "cover_prompt": "synthwave cover",
                "key_relationship_to_prior": "—",
                "tempo_relationship_to_prior": "—",
                "composer_notes": ""
            }
        ]
    });
    tokio::fs::write(
        album_json_dir.join(format!("{album_slug}.json")),
        serde_json::to_vec_pretty(&album_json).unwrap(),
    )
    .await
    .unwrap();

    // Stage two fake nightdrive tracks under <workspace>/var/tracks/.
    let tracks_dir = workspace.join("var").join("tracks");
    tokio::fs::create_dir_all(&tracks_dir).await.unwrap();

    for (track_id, title) in [
        ("nd-19990101-001", "First Witness Track"),
        ("nd-19990101-002", "Second Witness / Track"),
    ] {
        let trk_root = tracks_dir.join(track_id);
        tokio::fs::create_dir_all(&trk_root).await.unwrap();
        // Spec.json with the title that the CLI's title-index uses to bind
        // album-JSON tracks to on-disk artifact dirs.
        let spec = serde_json::json!({
            "track_id": track_id,
            "title": title,
            "subgenre": "synthwave",
            "mood_tags": [],
            "bpm": 88,
            "musical_key": "A minor",
            "duration_seconds": 240,
            "sections": [],
            "musicgen_prompt": "",
            "cover_prompt": "",
            "youtube": {"title": title, "description": "", "tags": [], "category_id": "10"}
        });
        tokio::fs::write(
            trk_root.join("spec.json"),
            serde_json::to_vec(&spec).unwrap(),
        )
        .await
        .unwrap();
        // Fake master.flac — just bytes; we're not parsing audio.
        tokio::fs::write(trk_root.join("master.flac"), b"FAKE_FLAC_BYTES_FOR_WITNESS")
            .await
            .unwrap();
        // PNG signature + minimal payload so cover-copy step runs.
        let png_signature = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n'];
        let mut cover = png_signature.to_vec();
        cover.extend_from_slice(b"witness-cover-payload");
        tokio::fs::write(trk_root.join("cover.png"), &cover).await.unwrap();
    }

    // Write a minimal nightdrive.toml pointing tracks_dir at our staged dir.
    let toml_text = format!(
        r#"
[paths]
work_dir = "{workspace}"
sqlite_db = "{workspace}/nightdrive.sqlite"
tracks_dir = "{workspace}/var/tracks"

[openclaw]
base_url = "http://127.0.0.1:11434"
model = "qwen2.5:7b-instruct"

[audio_gen]
base_url = "http://127.0.0.1:8083"
model = "acestep-v15-turbo"
sample_rate = 48000
channels = 2
segment_seconds = 30
overlap_seconds = 0
engine = "ace_step"

[art]
base_url = "http://127.0.0.1:8081"
model = "sdxl"
width = 1024
height = 1024
steps = 20
cfg_scale = 7.0
negative_prompt = ""

[mastering]
target_lufs = -14.0
true_peak_db = -1.0
loudness_range = 7.0
fade_in_seconds = 2.0
fade_out_seconds = 4.0

[visualizer]
width = 1920
height = 1080
fps = 30
quality_preset = "medium"
seed_from_track_id = true
include_code_scroll = false

[encoder]
ffmpeg_path = "ffmpeg"
video_codec = "libx264"
crf = 18
preset = "medium"
audio_codec = "aac"
audio_bitrate = "320k"
intro_seconds = 0
outro_seconds = 0

[youtube]
default_privacy = "private"
default_category_id = "10"
schedule_offset_hours = 24
publish_window_start_hour = 8
publish_window_end_hour = 22
declare_synthetic_content = true

[livestream]
visualizer_ws_port = 7373
metadata_refresh_seconds = 30
shuffle_buffer_size = 8
min_replay_gap_hours = 12
"#,
        workspace = workspace.display().to_string().replace('\\', "/"),
    );
    let toml_path = workspace.join("nightdrive.toml");
    tokio::fs::write(&toml_path, toml_text).await.unwrap();

    // Run: nightdrive-cli --config <toml> export album --slug <slug>
    let output = Command::new(&bin)
        .current_dir(workspace)
        .arg("--config").arg(&toml_path)
        .arg("export").arg("album")
        .arg("--slug").arg(album_slug)
        .output()
        .await
        .expect("spawn nightdrive-cli");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "nightdrive-cli export album failed (exit {:?}): stdout={stdout} stderr={stderr}",
        output.status.code(),
    );

    let exports_dir = workspace.join("exports").join(album_slug);
    assert!(exports_dir.exists(), "exports dir not created");
    // FLACs with normalized names: "01 - First Witness Track.flac"
    let flac1 = exports_dir.join("01 - First Witness Track.flac");
    let flac2 = exports_dir.join("02 - Second Witness - Track.flac"); // `/` sanitized to `-`
    assert!(flac1.exists(), "missing {}", flac1.display());
    assert!(flac2.exists(), "missing {}", flac2.display());
    assert!(exports_dir.join("covers").join("01 - First Witness Track.png").exists());
    assert!(exports_dir.join("covers").join("02 - Second Witness - Track.png").exists());
    assert!(exports_dir.join("README.txt").exists(), "README.txt not written");
    let readme = tokio::fs::read_to_string(exports_dir.join("README.txt")).await.unwrap();
    assert!(readme.contains("Witness Album"), "README.txt missing album title");
}

fn find_cli_binary() -> Option<std::path::PathBuf> {
    let manifest_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let target_root = manifest_dir.parent().unwrap().parent().unwrap().join("target");
    let exe_name = if cfg!(windows) { "nightdrive-cli.exe" } else { "nightdrive-cli" };
    for profile in ["debug", "release"] {
        let candidate = target_root.join(profile).join(exe_name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

// Tiny helper — used only so the const'd `Path` deps land in scope; otherwise
// rustc warns about the unused use.
#[allow(dead_code)]
fn _path_use(p: &Path) -> bool { p.exists() }
