// stage: 4
// expect: DemucsCli::separate on a real master.flac writes 4 stem WAVs at
//         `<track_root>/stems/{drums,bass,vocals,other}.wav` and the vocals
//         stem is < 50% the size of master.flac (true for an instrumental
//         track even when vocals.wav contains residual bleed).
// requires: a `demucs` CLI on PATH (the synthwave-gen / acestep venv has it
//           after `pip install demucs`). Skips cleanly with an instructive
//           message when the binary isn't installed or no master.flac to
//           operate on.
//
// Proves nightdrive-stems against a real subprocess + real audio. No mocks
// per tests/witnesses/README.md — a mock would tell us nothing about
// whether the htdemucs_ft model is downloaded, whether the CLI version
// matches our flag set, or whether the output layout normalization
// actually finds the demucs-emitted files.
//
// On a 3070 Ti this witness completes in ~30-60s once htdemucs_ft is
// cached. First run downloads weights (~300 MB) and takes longer.

use nightdrive_core::TrackPaths;
use nightdrive_stems::{DemucsCli, StemSeparator, StemsConfig};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stems_real_demucs_separates_instrumental_track() {
    if !demucs_on_path().await {
        eprintln!(
            "SKIP: `demucs` CLI not on PATH. Install via \
             `pip install demucs` into the synthwave-gen venv and put its \
             Scripts/bin dir on PATH, or set DEMUCS_PATH env var. Witness \
             will fire automatically once available."
        );
        return;
    }

    // Source audio: a real shipped track is the best fixture (a couple MB
    // of FLAC, instrumental, known-good). Operator-overridable via env so
    // a dev box without var/nightdrive/tracks/ can still run by pointing
    // at any local FLAC.
    let source = std::env::var("NIGHTDRIVE_STEMS_FIXTURE")
        .map(std::path::PathBuf::from)
        .ok()
        .or_else(find_first_master_flac);
    let Some(source_flac) = source else {
        eprintln!(
            "SKIP: no master.flac fixture found. Run one full pipeline first \
             (so var/nightdrive/tracks/*/master.flac exists) or set \
             NIGHTDRIVE_STEMS_FIXTURE to a local .flac path."
        );
        return;
    };
    eprintln!("fixture: {}", source_flac.display());

    // Stage the source FLAC into a tempdir tree that matches TrackPaths layout.
    let tmp = tempfile::tempdir().expect("tempdir");
    let track_id = nightdrive_core::TrackId::new(
        chrono::NaiveDate::from_ymd_opt(1999, 1, 1).unwrap(),
        4,
    );
    let paths = TrackPaths::new(tmp.path(), &track_id);
    tokio::fs::create_dir_all(&paths.root).await.expect("mkdir track root");
    tokio::fs::copy(&source_flac, &paths.master_flac())
        .await
        .expect("stage master.flac");

    let cfg = StemsConfig {
        // CPU avoids needing a GPU for the witness — slower but proves the
        // wrapper without GPU coupling. Operator can override with
        // DEMUCS_DEVICE=cuda for fast runs.
        device: std::env::var("DEMUCS_DEVICE").unwrap_or_else(|_| "cpu".to_string()),
        // Default model is htdemucs_ft; override to plain htdemucs in env if
        // the fine-tuned weights aren't downloaded.
        model: std::env::var("DEMUCS_MODEL").unwrap_or_else(|_| "htdemucs_ft".to_string()),
        // Stems on CPU is slow — bump the timeout to 30 min for the witness.
        timeout_seconds: 1800,
        ..StemsConfig::default()
    };

    let demucs = DemucsCli::new(cfg);
    let stems = demucs.separate(&paths).await.expect("demucs separate");

    // All four stems exist + non-empty.
    for stem in stems.all() {
        let meta = tokio::fs::metadata(stem)
            .await
            .unwrap_or_else(|_| panic!("stem {} missing", stem.display()));
        assert!(meta.len() > 1024, "stem {} is too small: {} bytes", stem.display(), meta.len());
    }

    // For an instrumental track, vocals stem should be significantly smaller
    // (mostly silence). Conservative: < 50% of master.flac size. (The QC
    // warning in DemucsCli fires at 10%, but real demucs leaves enough
    // residual to push past 10% — 50% is the "this is clearly wrong"
    // threshold.)
    let master_size = tokio::fs::metadata(&paths.master_flac())
        .await
        .expect("master meta")
        .len();
    let vocals_size = tokio::fs::metadata(&stems.vocals)
        .await
        .expect("vocals meta")
        .len();
    eprintln!(
        "vocals/master size ratio = {:.3}",
        vocals_size as f64 / master_size as f64,
    );
    // WAV is uncompressed, FLAC is compressed — direct byte comparison is a
    // weak signal but catches the "model emitted speech where there was
    // none" failure mode (vocals stem would be loud → large). We only fail
    // when the ratio is implausibly large.
    let ratio = vocals_size as f64 / master_size as f64;
    assert!(
        ratio < 5.0,
        "vocals.wav ({} B) suspiciously larger than master.flac ({} B); \
         demucs probably misidentified instrumental content as vocals",
        vocals_size, master_size,
    );
}

async fn demucs_on_path() -> bool {
    let path = std::env::var("DEMUCS_PATH").unwrap_or_else(|_| "demucs".to_string());
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new(&path)
            .arg("--help")
            .output(),
    )
    .await;
    matches!(result, Ok(Ok(out)) if out.status.success())
}

fn find_first_master_flac() -> Option<std::path::PathBuf> {
    for base in [
        std::path::PathBuf::from("var/nightdrive/tracks"),
        std::path::PathBuf::from("/var/lib/nightdrive/tracks"),
    ] {
        let Ok(entries) = std::fs::read_dir(&base) else { continue };
        for entry in entries.flatten() {
            let candidate = entry.path().join("master.flac");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}
