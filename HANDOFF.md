# nightdrive — Autonomous Synthwave Generation & Publishing Pipeline

**Project:** `nightdrive`
**Owner:** Matt Gates / Ridge Cell Repair LLC / OpenClaw
**Status:** SCAFFOLD — vision locked, no code written yet
**Last updated:** 2026-05-10

A fully automated pipeline that turns a single `cron` tick on a Linux box into a published YouTube video (or live RTMP stream) of original synthwave / "coding chill / nighttime vibes" music with a custom retrowave visualizer. End to end: composition → audio render → mastering → cover art → animated video → YouTube upload, no human in the loop.

---

## 1. Why this exists

Two revenue plays from one codebase:

1. **VOD channel(s).** 30–60 minute synthwave "coding/bugfixing/late-night-debug" mixes uploaded daily. Lofi Girl proved the format; AI-generated lofi channels (Chillhop AI, etc.) prove the unmanned version works. Monetization gate: 1,000 subs + 4,000 watch hours / 12 months. A 60-minute video with 50 concurrent viewers nets ~50 watch hours per session.
2. **24/7 livestream.** Single channel running a Pixie-Dust-style rotation of generated tracks endlessly. Watch hours rack up fast. Once monetized, mid-rolls on a never-ending stream compound. This is the flywheel.

Same pipeline produces both. Pre-generate a deep catalog (target: 200+ tracks = ~14 hours), upload 1–2 as VOD daily, loop the rest on the livestream channel.

> **YouTube AI-content note:** As of late 2024 YouTube requires creators to disclose "altered or synthetic content" in the upload checklist for "realistic" media. Music generally falls outside the strict-disclosure category, but we'll flag the synthetic-content checkbox conservatively. AI-generated music is allowed on YouTube and is monetizable; we're not skirting policy.

## 2. Architecture (one screen)

```
cron → OpenClaw LLM (composition spec) → [MusicGen | SDXL cover] (parallel, GPU)
     → [audio master | visualizer render] (parallel)
     → ffmpeg compose
     → [youtube VOD upload | RTMP livestream feed]
```

See the diagram in chat for the full picture. Six tiers, two parallel splits.

## 3. Hardware mapping (Matt's fleet)

| Box | Role | Reason |
|---|---|---|
| `supermicro` (8× Tesla P40, 192GB VRAM) | MusicGen + SDXL inference | Plenty of headroom; can run MusicGen-large in parallel with SDXL on separate GPUs |
| `main-pc` (RTX 3070 Ti, "kokonoe") | Visualizer render (wgpu) | Realtime synthwave scene + audio-reactive shader work |
| `arch-controller` (GTX 980) | RTMP origin / OBS host for livestream | Always-on, doesn't need much GPU for an audio-reactive WebGL scene |
| `hp-victus` (RTX 3050) | Fallback / dev box | |
| **Where the orchestrator runs:** | systemd timer on `arch-controller` | Always-on, modest CPU, dispatches work over Tailscale to the muscle |

The orchestrator does NOT do inference. It schedules, monitors, mux-es, uploads. Inference jobs are dispatched as Ollama/MusicGen REST calls to `supermicro` over Tailscale, results pulled back via SSH/rsync.

## 4. Workspace layout (Cargo workspace, 11 crates)

```
nightdrive/
├── Cargo.toml                                # workspace
├── HANDOFF.md                                # this file
├── README.md
├── .env.example
├── config/
│   └── nightdrive.toml.example
├── scripts/
│   ├── cron-nightly.sh                       # systemd timer ExecStart
│   └── livestream-loop.sh                    # 24/7 stream supervisor
├── visualizer/
│   └── index.html                            # browser-source visualizer (OBS)
└── crates/
    ├── nightdrive-core/                      # shared types, tracing setup, errors
    ├── nightdrive-llm/                       # OpenClaw / Ollama client, prompts
    ├── nightdrive-audio-gen/                 # MusicGen REST client + chaining
    ├── nightdrive-audio-master/              # ffmpeg loudnorm, EQ, fades
    ├── nightdrive-art/                       # SDXL / Flux client for covers
    ├── nightdrive-visuals/                   # wgpu retrowave scene renderer
    ├── nightdrive-encoder/                   # ffmpeg wrapper for final mux
    ├── nightdrive-youtube/                   # YouTube Data API v3 client
    ├── nightdrive-storage/                   # SQLite tracks DB, dedup, history
    ├── nightdrive-orchestrator/              # binary: pipeline coordinator
    └── nightdrive-cli/                       # binary: manual triggers, status
```

Every crate uses `tracing` with structured fields. No `println!`. Every external call (Ollama, ffmpeg, YouTube API, file IO) is wrapped in a span. Failures bubble up as `thiserror` domain errors with `anyhow` for unexpected infra issues.

## 5. The pipeline stages in detail

### Stage 0 — Trigger
`systemd` timer fires `nightdrive-orchestrator run-batch --count 1` at 22:00 local. (Cron also works; I prefer systemd for journalctl integration.) On boot, a separate `nightdrive-orchestrator livestream` unit starts and stays running.

### Stage 1 — Composition spec (LLM)
`nightdrive-llm` hits the local OpenClaw orchestrator (Ollama). Single structured-output prompt returns a `CompositionSpec`:

```json
{
  "track_id": "nd-20260510-001",
  "title": "Neon Drift on Highway 9",
  "subgenre": "synthwave",
  "mood_tags": ["nocturnal", "introspective", "driving"],
  "bpm": 92,
  "key": "F# minor",
  "duration_seconds": 240,
  "sections": [
    {"name": "intro", "bars": 8, "instrumentation": "pad + arp"},
    {"name": "verse", "bars": 16, "instrumentation": "+ bass + drums"},
    {"name": "chorus", "bars": 16, "instrumentation": "+ lead + sidechain"},
    {"name": "bridge", "bars": 8, "instrumentation": "stripped"},
    {"name": "outro", "bars": 8, "instrumentation": "fade"}
  ],
  "musicgen_prompt": "lo-fi synthwave 92 BPM F# minor, gated reverb drums, analog DX7 pad, bright lead arp, sidechain compression on bass, nocturnal driving vibe",
  "youtube": {
    "title": "Neon Drift on Highway 9 — Synthwave for Coding [4K]",
    "description": "...",
    "tags": ["synthwave","coding music","lofi","study","programming"],
    "category": "10"
  },
  "cover_prompt": "synthwave 1985 album cover, neon palm trees, chrome grid floor, setting sun, F#m mood, no text"
}
```

Model: `qwen2.5-7b-instruct` (already running on the fleet) is plenty for this. Schema enforced via structured output / json-mode.

### Stage 2 — Audio generation (MusicGen)
`nightdrive-audio-gen` calls a thin MusicGen HTTP wrapper on `supermicro`. MusicGen-large caps at ~30s per generation, so:
- Generate 8–12 contiguous segments using the composition spec's section breakdown
- Each segment gets `prompt + previous_audio` as continuation context
- Stitch segments with 1-bar crossfades using `rubato` (Rust audio resampling)
- Output: `tracks/<id>/raw.wav` at 32kHz stereo

For longer-form / smoother output we'll experiment with **Stable Audio Open** as well (40s clips, better at long-form). Both wrap behind the same `AudioGenerator` trait.

### Stage 3 — Cover art (SDXL / Flux)
`nightdrive-art` calls an SDXL or Flux Schnell HTTP wrapper on a different GPU. 1024×1024 cover, synthwave aesthetic. Negative prompt locked to exclude text artifacts. Saved as `tracks/<id>/cover.png`.

### Stage 4 — Audio mastering
`nightdrive-audio-master` runs an ffmpeg chain:
1. `loudnorm` filter, two-pass, target `-14 LUFS` (YouTube standard)
2. Gentle high-shelf EQ (synthwave bright top)
3. 2s fade-in, 4s fade-out
4. Export `tracks/<id>/master.flac` (lossless intermediate) + `master.mp3` (CBR 320 for fallback)

### Stage 5 — Visualizer
Two render paths:

**A. Pre-rendered VOD (per-track):**
`nightdrive-visuals` is a headless `wgpu` renderer. Inputs: master audio, cover art, track title. Output: 1920×1080 30fps MP4 of the audio-reactive scene. Scene elements (all parametric, seeded from track_id for visual variety):
- Wireframe grid floor with perspective + chromatic aberration
- Setting sun / palm trees / mountain silhouette
- Frequency-band reactive bars (low/mid/high → different visual elements)
- Track title + subtitle overlay
- Subtle scrolling code in background (for "coding/bugfixing" branded uploads)

**B. Live 24/7 stream:**
`visualizer/index.html` — same scene but in WebGL/Three.js, drops into OBS as a Browser Source. A small WebSocket on `arch-controller` pushes track metadata + current FFT spectrum from the running audio player. OBS captures the browser + a `pulseaudio` virtual sink and streams to YouTube via RTMP.

### Stage 6 — Final encode
`nightdrive-encoder` runs `ffmpeg`:
- Inputs: `master.flac` + `scene.mp4` + `cover.png` (for thumbnail)
- Output: H.264 high@1080p30 + AAC 320k, MOV faststart container
- Adds: 3s cover-art intro frame, 3s outro frame
- Output: `tracks/<id>/final.mp4`

### Stage 7 — Publish
`nightdrive-youtube` uses YouTube Data API v3 (OAuth refresh-token flow):
- Resumable upload (`videos.insert` with `uploadType=resumable`)
- Set thumbnail (`thumbnails.set`)
- Set privacy: `private` → human review queue, OR `scheduled` for a publishAt window
- Mark "altered or synthetic content" flag (`videos.update` with `selfDeclaredMadeForKids=false` and `contentDetails.contentRating` flags as appropriate)
- Save uploaded video_id in `nightdrive-storage` SQLite for dedup + analytics

### Stage 8 — Livestream (parallel, always-on)
Separate orchestrator subcommand. Reads from `nightdrive-storage` for tracks ordered by `last_streamed_at ASC`, plays through `pulseaudio` virtual sink, pushes metadata to the visualizer WebSocket, OBS does the RTMP push. Failure recovery: if RTMP drops, OBS auto-reconnects; if the player crashes, systemd restarts it; if YouTube drops the stream, we re-key.

## 6. Tech choices & rationale

| Concern | Choice | Why |
|---|---|---|
| Workspace lang | Rust 2024 | Matt's preference, perf, memory safety for long-running orchestrator |
| Async runtime | `tokio` | Standard |
| HTTP client | `reqwest` | Ollama, MusicGen wrapper, YouTube API |
| YouTube API | Hand-rolled `reqwest` client | `google-youtube3` crate is fine but adds 80+ deps; hand-roll for the ~5 endpoints we use |
| SQLite | `sqlx` | Compile-time checked queries, track history + dedup |
| Audio stitching | `rubato` + `hound` | Rust-native, no Python needed |
| Mastering / mux | `ffmpeg` (subprocess) | Same as everyone, structured args via `tokio::process::Command` |
| Visualizer (VOD) | `wgpu` headless | Pure Rust, runs on `main-pc`'s 3070 Ti |
| Visualizer (Live) | Three.js in OBS Browser Source | Easier to iterate, no AV1/realtime-encode complexity |
| Music model | MusicGen-large (primary) + Stable Audio Open (experimental) | Open weights, run locally on P40s, no per-track API cost |
| Cover art model | SDXL or Flux Schnell | SDXL is proven, Flux Schnell is faster |
| LLM | local `qwen2.5-7b-instruct` via OpenClaw | Zero external API cost, already running |
| Logging | `tracing` + JSON output to journald | Verbose everywhere, parseable for Grafana later |
| Metrics | Prometheus exporter on `:9091` | Hook into existing Grafana stack |
| Secrets | `.env` + `sops` later | YT OAuth refresh token in `.env` for now |

## 7. Data model (SQLite)

Schema source of truth: `crates/nightdrive-storage/migrations/20260510000000_init.sql`.

```sql
CREATE TABLE IF NOT EXISTS tracks (
    id              TEXT PRIMARY KEY,          -- ulid / uuid v7
    title           TEXT NOT NULL,
    bpm             INTEGER NOT NULL,
    key             TEXT NOT NULL,
    seed            INTEGER NOT NULL,
    spec_json       TEXT NOT NULL,             -- raw CompositionSpec
    state           TEXT NOT NULL,             -- nightdrive_core::TrackState: pending|spec_generated|audio_rendered|cover_rendered|audio_mastered|video_encoded|published|failed
    audio_path      TEXT,
    cover_path      TEXT,
    visualizer_path TEXT,                       -- final mp4
    duration_secs   INTEGER,
    created_at      TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_tracks_state      ON tracks(state);
CREATE INDEX IF NOT EXISTS idx_tracks_created_at ON tracks(created_at);

CREATE TABLE IF NOT EXISTS uploads (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id            TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
    youtube_video_id    TEXT,
    upload_url          TEXT,                   -- resumable session
    bytes_uploaded      INTEGER NOT NULL DEFAULT 0,
    status              TEXT NOT NULL,          -- queued|uploading|complete|failed
    error               TEXT,
    started_at          TEXT NOT NULL DEFAULT (datetime('now')),
    completed_at        TEXT
);

CREATE INDEX IF NOT EXISTS idx_uploads_track  ON uploads(track_id);
CREATE INDEX IF NOT EXISTS idx_uploads_status ON uploads(status);

CREATE TABLE IF NOT EXISTS livestream_rotation_log (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    track_id    TEXT NOT NULL REFERENCES tracks(id),
    started_at  TEXT NOT NULL DEFAULT (datetime('now')),
    ended_at    TEXT,
    listeners   INTEGER                        -- snapshot from yt analytics, optional
);

CREATE INDEX IF NOT EXISTS idx_rotation_track ON livestream_rotation_log(track_id);
```

## 8. Config

`config/nightdrive.toml`:

```toml
[paths]
work_dir = "/var/lib/nightdrive"
sqlite_db = "/var/lib/nightdrive/nightdrive.sqlite"

[openclaw]
base_url = "http://kokonoe.tailb85819.ts.net:11434"
model = "qwen2.5-7b-instruct"
temperature = 0.85

[audio_gen]
base_url = "http://supermicro.tailb85819.ts.net:8080"
model = "musicgen-large"
segment_seconds = 28
overlap_seconds = 2

[art]
base_url = "http://supermicro.tailb85819.ts.net:8081"
model = "sdxl"
steps = 30

[mastering]
target_lufs = -14.0
true_peak_db = -1.0

[encoder]
ffmpeg_path = "/usr/bin/ffmpeg"
video_codec = "libx264"
crf = 18
preset = "slow"

[youtube]
oauth_refresh_token_env = "NIGHTDRIVE_YT_REFRESH_TOKEN"
client_id_env = "NIGHTDRIVE_YT_CLIENT_ID"
client_secret_env = "NIGHTDRIVE_YT_CLIENT_SECRET"
default_privacy = "private"                   # flip to "public" once trusted
default_category_id = "10"                    # Music
schedule_offset_hours = 24                    # auto-schedule 24h out

[livestream]
rtmp_url_env = "NIGHTDRIVE_YT_STREAM_KEY"
obs_websocket_url = "wss://arch-controller.tailb85819.ts.net:4455"
visualizer_ws_port = 7373
```

## 9. Bootstrap order (build sequence)

When picking this back up, build the crates in this order — each is independently testable:

1. **`nightdrive-core`** — types, `AppError`, tracing init. ~1 hour.
2. **`nightdrive-storage`** — SQLite schema + migrations + CRUD. ~2 hours.
3. **`nightdrive-llm`** — OpenClaw client, prompt template, integration test against local Ollama. ~3 hours.
4. **`nightdrive-audio-gen`** — Stand up a Python `musicgen-server.py` on `supermicro` (FastAPI + audiocraft), then write the Rust client. ~1 day total.
5. **`nightdrive-audio-master`** — ffmpeg `loudnorm` wrapper, two-pass. ~3 hours.
6. **`nightdrive-art`** — SDXL HTTP client (assumes a `stable-diffusion-webui` or `comfyui` API endpoint is running). ~2 hours.
7. **`nightdrive-encoder`** — final ffmpeg mux. ~3 hours.
8. **`nightdrive-youtube`** — OAuth flow + resumable upload. ~1 day (OAuth is fiddly).
9. **`nightdrive-visuals`** — wgpu visualizer. **This is the time sink.** Start with a static rendered scene, add audio reactivity iteratively. ~3-5 days for v1.
10. **`nightdrive-orchestrator`** — pipeline binary stitching all of the above. ~1 day.
11. **`nightdrive-cli`** — manual triggers, status, replays. ~half day.
12. **`visualizer/index.html`** — Three.js browser-source for livestream. Can be done in parallel with the rest. ~1 day.

**MVP cutoff:** crates 1–8 + 10 + 11 = ~5 days = one VOD-per-day pipeline live. Visuals at this stage can be a static cover art + waveform (ffmpeg `showwaves` filter) — ugly but ships. Then iterate on `nightdrive-visuals` for real synthwave scene + the livestream channel.

## 10. Revenue timeline (honest)

| Milestone | Realistic timing | Notes |
|---|---|---|
| First VOD live, private | Day 5 | MVP shipped, gated manual review |
| First public VOD | Day 7 | Once visuals don't embarrass |
| 1 VOD/day cadence | Week 2 | Pipeline running unattended |
| 24/7 livestream channel up | Week 3 | Once track catalog is ~30 deep |
| 1,000 subs | Month 2-4 | Depends entirely on algorithm luck + content quality |
| 4,000 watch hours | Month 1-3 | The livestream is the engine here; 50 concurrent × 24h × 30d = 36k watch hours |
| Monetization approved | Month 3-6 | YouTube review takes ~1 month after thresholds hit |
| First $100 month | Month 4-7 | Once monetized, RPM on music content is low (~$0.50-2 CPM) but compounds |

**This is not a 30-day revenue play.** This is a **30-day asset-build play** that pays compounding watch-hour rent for years. If 30-day revenue is the gate, this needs to ship alongside Fiverr work and the Brander/OpenClaw client work, not instead of them.

## 11. Risks & open questions

- **MusicGen quality at length.** 30s clips chained may sound seam-y. Mitigation: aggressive crossfading, possibly Stable Audio Open instead, or a hybrid (LLM-generated MIDI rendered through a sample-based synthwave instrument set like Surge XT).
- **YouTube algorithm.** AI-music channels have been getting demonetized in waves. We disclose synthetic content honestly, focus on the listener experience, and don't try to game.
- **Visual variety.** 100 tracks with the same visualizer scene = uploads start to feel samey. Mitigation: seed visual parameters from `track_id` so every video has a slightly different palette, geometry, code-scroll content.
- **Compute cost.** 8× P40 server isn't free to run 24/7. Track wattage and decide whether to spin generation in batches (e.g. generate 14 tracks in one nightly burst, then idle the GPUs) vs always-on.
- **Copyright bots.** YouTube Content ID will scan. If MusicGen accidentally regurgitates training data we'd get a strike. Mitigation: pre-scan with `audfprint` or similar before upload, log + skip any flagged tracks.

## 12. Out of scope (for now)

- Vocals / lyrics (synthwave is largely instrumental anyway; revisit later)
- Multi-channel strategy (one channel first, prove the loop, then clone)
- Spotify / Apple Music distribution (different product, different model — DistroKid integration is a follow-up)
- Stream chat moderation (defer until livestream has actual viewers)

## 13. How to resume work

1. `cd nightdrive`
2. Read this file end to end
3. Check `git log` for last touched crate
4. Run `cargo check --workspace` to confirm green baseline
5. Pick next crate from §9 build order
6. Each crate's `src/lib.rs` has a `// TODO(nightdrive):` marker showing where to start

## 14. Power-outage recovery — 2026-05-10

Came back to nightdrive after a power outage mid-buildout. Status snapshot:

**Repo state (post-outage, kokonoe):**
- All 11 crates scaffolded under `crates/<name>/src/` — the file-reshuffle described in
  CLAUDE.md "Status: SCAFFOLD" is **done**. CLAUDE.md's table of "File at repo root → Belongs
  at" is stale; reshuffle landed pre-outage.
- `scripts/audit.ps1` returns `OK - audit clean (build:0 test:0 stubs:9 witnesses:1)`.
- `cargo check --workspace` clean (0.41s).
- Witnesses: 1 (`tests/witnesses/core_loads_real_config.rs`, `// stage: 0`).
- Stubs (expected, all flagged in code with `bail!("... not yet implemented; see ROADMAP.md N1.x")`):
  - orchestrator stages 2-6 + `resume` + `status` subcommands (8 stubs)
  - youtube chunked-resume upload (1 TODO)

**N1 roadmap (from `roadmap-tracker` post-outage report):**
- DONE: N1.1 (reshuffle), N1.2 (core, witnessed)
- READY on kokonoe alone: **N1.3 storage** (recommended next), N1.4 llm, N1.10 youtube hardening, N1.12 cli, N1.7 art (8 GB VRAM tight)
- GATED on cnc P100s (~2026-05-17): N1.5, N1.6, N1.8, N1.9, N1.11, N1.13, N1.14

**Fleet (post-outage):**
- `kokonoe` UP
- `cnc-server` DOWN (Matt power-cycling)
- `arch-controller`, `supermicro`, `hp-victus` DOWN (not on critical path while in scaffold phase)

**Side-fix in this session:** `~/.bashrc` lean-ctx hook was auto-enabling in non-TTY
subshells, causing every aliased command (cargo/git/rg/…) in Claude Code's Bash tool to
fail with a path-mangled `C:UsersMatt.cargobinlean-ctx.exe: command not found`. Gate
changed from `[ -t 1 ]` to `case "$-" in *i*` (interactive-shell flag check, reliable
since bash initializes with TTY attached even when stdout will later be piped).
`_lc` / `_lc_compress` also `export -f`'d for safety. Fix is permanent for new bash
sessions; current Claude Code session has stale alias state — use `command <cmd>`,
`\<cmd>`, `bash -lc '<cmd>'`, or PowerShell as workaround until restart.

**Next 1-2h of work:** N1.3 storage — sqlx migrate + CRUD + `tests/witnesses/storage_roundtrip.rs`
(`// stage: 0`). Also fixes the HANDOFF §7 schema-drift gate item in the same PR.

**Update — N1.4 (llm) shipped same session 2026-05-10:**
- `crates/nightdrive-llm/src/lib.rs` refactored into `attempt_generate_spec` + 3-attempt
  retry loop; `is_retryable` predicate retries only on parse/validate errors (transport
  failures bubble immediately — don't pound a sick Ollama).
- `tests/witnesses/llm_real_ollama.rs` (`// stage: 1`) hits real Ollama on kokonoe at
  `http://localhost:11434` with `qwen2.5:7b-instruct`. Model-presence probe skips cleanly
  with an instructive message when the configured model isn't pulled. Passes end-to-end
  against a real model load in ~44s.
- `qwen2.5:7b-instruct` (4.7 GB) pulled onto kokonoe's Ollama instance during the
  recovery session. The model wasn't present pre-outage — config + roadmap referenced it
  but no one had pulled it yet.
- Audit moved from `OK build:0 test:0 stubs:9 witnesses:1` → `OK build:0 test:0 stubs:9
  witnesses:2 stages:0,1`.

**Update — N1.3 (storage) shipped same session 2026-05-10:**
- `crates/nightdrive-storage/src/lib.rs`: `Db::connect_and_migrate` (WAL + foreign keys +
  `?mode=rwc`), `Tracks::{insert,update_state,list,get}`, `Uploads::{insert,set_youtube_id,get}`,
  `LivestreamRotation::{next_track,log_start}`. Runtime-typed sqlx queries (no DATABASE_URL
  build dependency); errors mapped to `NightdriveError::Storage`.
- `tests/witnesses/storage_roundtrip.rs` (`// stage: 0`): walks the full TrackState progression
  (pending → spec_generated → audio_rendered → cover_rendered → audio_mastered → video_encoded →
  published) against a real on-disk tempdir SQLite, verifies `spec_json` round-trips through
  serde, verifies `Uploads::set_youtube_id` flips status + stamps `completed_at`, verifies
  `LivestreamRotation::next_track` correctly prefers never-played tracks. 0.07s wall time.
- **Schema-drift gate fixed in same PR:** the `state` column comment in both
  `migrations/20260510000000_init.sql` AND HANDOFF.md §7 was carrying the old 6-state
  vocabulary (`pending|rendering|mastered|encoded|uploaded|failed`). Both updated to the
  authoritative 8-state `nightdrive_core::TrackState` vocabulary. Comment-only change, no
  schema impact, audit confirms `no schema drift`.
- Audit now: `OK build:0 test:0 stubs:9 witnesses:3 stages:0,1`.

**Update — N1.10 (youtube hardening) shipped same session 2026-05-10:**
- `crates/nightdrive-youtube/src/lib.rs` chunked PUT loop: `upload_in_chunks` (8 MB
  chunks via `tokio::fs::File::seek + read_exact`, no whole-file RAM buffering),
  `put_chunk_with_retry` (1 + 2 retries with exponential backoff, query upload offset
  between retries so partially-landed chunks aren't re-sent), `put_chunk` (handles
  200/201, 308 Resume Incomplete with `Range` header parsing, 5xx retry-able),
  `query_upload_offset` (public — PUT with `Content-Range: bytes */N`), `parse_range_next_byte`.
- `update_video` with **fetch-merge semantics**: videos.update has PUT (not PATCH)
  semantics on each `part`, so a partial snippet diff returns 400 invalidTitle unless
  the *full* snippet is included. update_video now fetches the current snippet via
  videos.list?part=snippet, merges the `VideoUpdate` diff on top, then PUTs. status
  doesn't have the same problem (only privacyStatus required) so partial PUT works.
- `delete_video` for witness cleanup.
- **Honest note about altered-content disclosure**: the YouTube Data API v3 surface
  (stable through early 2026) doesn't expose a writable field for the altered-content
  checkbox. The honest path is what `upload_video` already does — append a disclosure
  sentence to the description when `declare_synthetic_content=true`. `update_video`'s
  docstring spells this out so future-you doesn't try to forge a field.
- `crates/nightdrive-youtube/src/bootstrap.rs` + `nightdrive-cli youtube auth` shipped:
  OAuth Desktop flow with a localhost callback listener (binds 127.0.0.1:0 for an
  OS-picked free port, serves a friendly "auth complete" HTML to the browser).
- **OAuth scope bumped from `youtube.upload` to `youtube`**: the narrow scope rejects
  videos.update + videos.delete with `ACCESS_TOKEN_SCOPE_INSUFFICIENT`, which made
  upload-then-cleanup witnesses impossible. The broader scope covers everything we
  need including future N1.13 livestream broadcasts.
- `tests/witnesses/youtube_resume_upload.rs` (`// stage: 7`): synthesizes a 9.3 MB
  test mp4 via ffmpeg testsrc + anullsrc (just past the 8 MB chunk boundary so the
  loop iterates twice — one 308, one final 200), uploads via the chunked path
  against the real NightDrive channel, runs videos.update with a description patch,
  runs videos.delete, sweeps `NIGHTDRIVE_YT_ORPHAN_VIDEO_IDS` for leftover videos
  from earlier failed runs. Marked `#[ignore]` so `cargo test --workspace` never
  fires it incidentally — explicit opt-in via `--ignored` flag is required (witness
  count still grows in the audit because the `// stage:` grep is over file contents,
  not test runner output). Passed end-to-end in 19.66s against real YouTube, the
  NightDrive channel (channelId `UCsS7L4PUedZ-zus3rV3AMDw`) is clean post-run.
- `.gitignore` created (was missing) — `.env` with the refresh token is now properly
  excluded from any future git commits.
- `.env` populated with CLIENT_ID + SECRET + REFRESH_TOKEN bound to the NightDrive
  channel. Refresh token expires only if Matt revokes via Google account permissions
  page or rotates the OAuth client secret.
- Audit: `OK build:0 test:0 stubs:8 witnesses:4 stages:0,1,7`.

**Update — N1.7 (art) shipped same session 2026-05-10:**
- `crates/nightdrive-art/src/lib.rs`: `CoverArtist` trait + `SdxlClient` impl against
  AUTOMATIC1111-compatible `/sdapi/v1/txt2img` endpoint (works against A1111,
  Forge, and most ComfyUI bridges). 1024×1024 fp32-or-fp16 inference. Deterministic
  seed = djb2(track_id) so re-runs of the same track produce stable covers.
- Validation guardrail: PNG signature + IHDR dimensions checked after base64-decoding
  the sidecar's response. If sidecar misconfig returns a 512×512 image (wrong model
  loaded, wrong size in config), the client errors loudly rather than letting a bad
  cover ride through to the final encode + YouTube upload.
- `parse_png_dimensions` is a pure helper exported for the witness; also covered by
  3 inline `#[cfg(test)]` unit tests (cargo test -p nightdrive-art --lib).
- `tests/witnesses/art_real_sdxl.rs` (`// stage: 3`): probes `/sdapi/v1/sd-models`,
  skips with explicit "sidecar not deployed" message when NIGHTDRIVE_ART_URL unset or
  unreachable. When reachable: renders a cover for a known test spec, asserts PNG
  signature + 1024×1024 IHDR dimensions. Will actually fire when the cnc SDXL
  sidecar lands post-P100s.
- Audit: `OK build:0 test:0 stubs:8 witnesses:5 stages:0,1,3,7`.

**Update — N1.12 (cli) shipped same session 2026-05-10:**
- `crates/nightdrive-cli/src/main.rs`: `db migrate`, `tracks list`, `uploads list`,
  `stream status` real implementations. `db migrate` creates the sqlite parent dir
  if missing (operators new to nightdrive haven't pre-created /var/lib/nightdrive).
  `tracks list` and `uploads list` print tab-separated rows for easy piping.
  `stream status` calls `systemctl is-active nightdrive-livestream.service` on
  unix, returns platform-not-supported on windows so dev-side `cargo test` doesn't
  fail spuriously.
- `Uploads::list_recent` added to nightdrive-storage (`ORDER BY started_at DESC LIMIT N`).
- `tests/witnesses/cli_db_migrate.rs` (`// stage: 0`): spawns the actual built
  `nightdrive-cli` binary against a tempdir-scoped nightdrive.toml, walks the full
  db migrate → re-open via storage crate → tracks list → uploads list flow.
  Witness finds the binary via `CARGO_MANIFEST_DIR + ../../target/{debug|release}/`
  with debug preferred (because release builds go stale across edits).
- Audit: `OK build:0 test:0 stubs:8 witnesses:6 stages:0,1,3,7`.

**Update — N4.1 (per-stage retry policy) shipped same session 2026-05-10:**
- `crates/nightdrive-core/src/retry.rs`: `with_backoff(policy, op, should_retry)`
  generic async retry utility. Exponential backoff 1s → 2s → 4s capped at
  `max_backoff` (default 30s), ±20% jitter to break thundering-herd reconnects,
  caller-supplied `should_retry` predicate per-error. Default
  `RetryPolicy { max_attempts: 3, initial_backoff: 1s, max_backoff: 30s, jitter: 0.2 }`.
  Hand-rolled instead of pulling in `tokio-retry` / `backoff` to keep the workspace
  surface small (a `tokio-retry` add ~5 transitive deps for a 60-line utility).
- `crates/nightdrive-core/Cargo.toml`: added tokio as a workspace dep (was already
  used by retry::with_backoff and the inline #[tokio::test] tests).
- 5 inline unit tests cover: success on first try, transient-then-success,
  bubble-on-non-retryable, budget exhaustion, exponential schedule cap.
- `tests/witnesses/retry_recovers_transient_500.rs` (`// stage: 0`) spins up an
  in-process 30-line raw-TCP mock HTTP server returning 500 → 500 → 200, calls
  `with_backoff` via reqwest, asserts the body comes back and exactly 3 attempts
  were made. Documents the mock-server exception per `tests/witnesses/README.md`.
- Follow-up: `nightdrive-llm`'s open-coded retry loop should eventually call
  through `with_backoff` for symmetry — not done in this turn so the existing
  llm witness keeps passing without behavioral changes.
- Audit: `OK build:0 test:0 stubs:8 witnesses:7 stages:0,1,3,7`.

## 15. Pipeline end-to-end — first VOD uploaded 2026-05-11

**FIRST PRIVATE VOD LIVE ON NIGHTDRIVE CHANNEL.**

- Watch: https://youtube.com/watch?v=EGFUlex64L4
- Title: "Nocturnal Lanes (Synthwave for Coding)"
- Duration: 4m 34s · key: G minor · BPM: 96
- End-to-end wall time: **7m 20s** for one full track
- Privacy: private (per `[youtube].default_privacy`)

The wgpu visualizer (N3.1) is still gated; this VOD uses the ROADMAP §10 MVP
placeholder — a deterministic per-track-id gradient cover + ffmpeg `showwaves`
overlay baked into the encoder filter graph. Looks like exactly what the
roadmap promised: "ugly but ships." Once N3.1 lands the encoder swaps the
overlay for a real wgpu-rendered scene at the same orchestrator surface.

**Stages, in order, with wall times from the live run:**

| Stage | Crate | Wall |
|---|---|---|
| 1 spec | `nightdrive-llm` via Ollama qwen2.5:7b-instruct on kokonoe | 74 s |
| 2 audio | SAO sidecar `sidecar/stable_audio_server.py` on kokonoe 3070 Ti — 8× 35s segments chained with equal-power crossfade | 4 min 4 s |
| 3 cover | SDXL unreachable (3070 Ti VRAM-contended with SAO); fell back to the ffmpeg-gradient placeholder in `orchestrator::placeholder_cover` | <1 s |
| 4 master | `nightdrive-audio-master` two-pass ffmpeg loudnorm — measured -12.68 LUFS, normalized to -14.0 LUFS | 21 s |
| 5 visualizer | placeholder (showwaves overlay folded into stage 6) | 0 s |
| 6 encode | `nightdrive-encoder` ffmpeg mux: cover + showwaves overlay + master.flac → 67 MB final.mp4 (H.264 medium CRF 18 + AAC 320k + faststart) | 60 s |
| 7 upload | `nightdrive-youtube` chunked PUT to YouTube Data API v3 | 40 s |

**One non-blocker surfaced and patched:** `thumbnails.set` returned `403 youtube.thumbnail.forbidden — channel needs phone verification`. The pipeline now logs that as a warning and continues; YouTube auto-generates a thumbnail from frame samples so the VOD still has a thumbnail. Once the channel is phone-verified via youtube.com/verify the custom thumbnail upload will work without code changes.

**What got built in the same session to reach this point:**

- `sidecar/stable_audio_server.py` — FastAPI wrapper for SAO 1.0 ported from
  `J:\pledgeandcrowns\tools\synthwave-gen\generate.py` per CLAUDE.md §"DO NOT
  REINVENT" §1. fp16 on the 3070 Ti, ~3.6 GB model footprint, ~25 s wall per
  10 s of audio at 100 steps. Reuses synthwave-gen's T5 token-length pre-flight
  + gated-repo error message verbatim. Runs in the synthwave-gen venv (Python
  3.10.6, torch 2.5.1 + cu121, diffusers 0.31). Sidecar startup ~21 s.
- `crates/nightdrive-audio-gen/src/lib.rs` — `AudioGenerator` trait +
  `StableAudioClient` HTTP impl. Segment count derived from `[audio_gen]`
  `segment_seconds` + `overlap_seconds` so post-crossfade total ≥ `spec.duration_seconds`.
  Equal-power crossfade in `crossfade_into` (cos/sin sum to 1.0 in power, no
  middle-dip from linear blend). 3 inline unit tests for crossfade + djb2.
- `crates/nightdrive-audio-master/src/lib.rs` — `AudioMaster` trait +
  `FfmpegMaster`. Two-pass loudnorm with `print_format=json` measurement
  parse + measured-value pass-2 + fade-in/out applied in the same filter
  graph. ffmpeg-banner duration probe to position the fade-out start (cheaper
  than spawning ffprobe). 2 inline unit tests for parsers.
- `crates/nightdrive-encoder/src/lib.rs` — `FinalEncoder` trait +
  `FfmpegEncoder`. Cover + showwaves overlay + master FLAC → MP4 with `+faststart`
  via `-shortest`. Plus `make_thumbnail()` helper for the JPEG re-encode.
- `crates/nightdrive-orchestrator/src/main.rs` `pipeline_one` — all 5 bail!s
  replaced with real calls. SDXL-or-fallback art logic with a deterministic
  per-track-id gradient placeholder (drawtext-free for Windows ffmpeg ACCESS_VIOLATION
  immunity). Thumbnail set is best-effort (logs 403, continues).
- `config/nightdrive.toml` (gitignored) — kokonoe-specific config: localhost
  endpoints for Ollama + SAO sidecar, J: drive paths, ffmpeg from PATH.

**Audit:** `OK build:0 test:0 stubs:3 witnesses:7 stages:0,1,3,7`. The 3
remaining stubs are `resume`, `status`, `livestream` in
`crates/nightdrive-orchestrator/src/main.rs` — separate roadmap items
(N2.1 Resume, N1.12-status, N2.4 Livestream player).

**N1.14 First private VOD end-to-end — DONE.**

## 16. MusicGen engine landed 2026-05-11 (track #2 uploaded)

Matt's critique of track #1: seam audible every ~34s where one SAO clip ends
and the next begins. SAO has no audio-prior conditioning so segments can only
be blended via crossfade, which masks but doesn't eliminate the timbre shift.

**Decision:** ship a MusicGen engine alongside SAO. MG has native
`generate_continuation(prompt=prev_audio, ...)` which produces a real
extension of the prior audio rather than a separate clip blended in. License
caveat — MG weights are CC-BY-NC; the strike-tail-risk on the monetized
NightDrive channel has been explicitly accepted by Matt (see memory file
`project_musicgen_commercial_risk_accepted.md`).

**Architecture:**
- `sidecar/musicgen_server.py` — FastAPI wrapper around audiocraft's MusicGen.
  Default model `facebook/musicgen-stereo-medium` (~3.4 GB VRAM, native
  stereo at 32 kHz, fits the kokonoe 3070 Ti). Same `POST /generate`
  contract as the SAO sidecar plus a `prev_audio_b64` field for continuation.
- `crates/nightdrive-audio-gen/src/lib.rs` — new `MusicGenClient` impl of
  `AudioGenerator`. Generates segment 1 fresh, then for each subsequent
  segment passes the last `[audio_gen].continuation_prefix_seconds` (default
  5s) of accumulated audio as `prev_audio_b64`, strips the sidecar's
  regenerated prefix from the response, appends only the new audio. Safety
  cap: 30 segments per render (~10 min of audio max).
- `nightdrive_audio_gen::client_for(cfg)` factory dispatches on the new
  `[audio_gen].engine` config field (`"stable_audio"` default, `"musicgen"`
  for the continuation path). Orchestrator's `pipeline_one` calls through
  the factory — same surface, engine choice is config-only.

**Side-by-side bench, both tracks 240s target, kokonoe 3070 Ti:**

| | Track #1 (SAO) | Track #2 (MG continuation) |
|---|---|---|
| video_id | EGFUlex64L4 | FGPUo7oXCI4 |
| title | Nocturnal Lanes | Night Drive Echoes |
| engine | Stable Audio Open 1.0 | MusicGen-stereo-medium |
| segments | 8 (blind crossfade, 35s × 7 + 1s overlap) | 12 (1 fresh + 11 continuations, 25s segments with 5s prefix) |
| sample rate | 44.1 kHz | 32 kHz |
| raw.wav | 48 MB | 31 MB |
| final.mp4 | 67 MB | 57 MB |
| wall time | 7m 20s | 17m 52s |
| thumbnail | auto (pre-verify) | ✅ custom (post-verify) |
| seam every ~34s | yes (config flagged 1s overlap → bumped to 3s for future SAO runs) | n/a — true continuation |

The 2.4× wall-time penalty is the cost of continuation: each call encodes
the prefix audio through EnCodec then decodes prefix + new audio. Worth it
if the seams are gone.

**Windows install gotchas captured in
`reference_audiocraft_windows_install.md`:**
- `pip install audiocraft` fails on Windows because `av` (PyAV) needs a
  prebuilt wheel and audiocraft pins torch==2.1.0 (clashes with diffusers'
  newer torch). The recipe installs `av --only-binary :all:` first, then
  `audiocraft --no-deps`, then audiocraft's runtime deps separately, then
  re-pins torch+torchaudio to 2.5.1+cu121.
- xformers is "required" via a module-level import in
  `audiocraft/modules/transformer.py` but the default
  `_efficient_attention_backend = 'torch'` means xformers ops are never
  called at runtime. Real xformers wheels demand incompatible torch
  versions, so we ship a **stub xformers package** (~25 lines) that
  satisfies the import and delegates `ops.unbind` to `torch.unbind`.

## 17. What's still open

**Hardware-gated** (cnc P100s ~2026-05-17):
- N1.5 deploy SAO sidecar onto cnc (the kokonoe sidecar at sidecar/stable_audio_server.py is the template — just deploy + fp32 on Pascal)
- N1.7 SDXL sidecar (8 GB VRAM contention with SAO on the 3070 Ti; cnc P100s break the tie)
- N1.13 systemd unit files installed on arch-controller

**Kokonoe-ready next:**
- Phone-verify the NightDrive channel at youtube.com/verify → custom thumbnails work
- Storage integration into pipeline_one (persist track row + state transitions per stage)
- N2.1 Resume subcommand (now real, since pipeline_one isn't stubbed any more)
- N3.1 wgpu visualizer (the big multi-week stage-5 unlock — would replace
  showwaves with a real audio-reactive scene)
- N2.2 Track dedup, N4.2 Telegram escalation, N4.4 Disk-pressure guard,
  N4.6 Prometheus exporter — all S-effort
- Cosmetic: the SAO output sometimes has audible 1s crossfade seams. Tweak
  `[audio_gen].overlap_seconds` to 2-3 once we have a bench rig to measure.

## 18. Session 2026-05-11 — Full TWC pipeline + 4 VODs queued

### Last Updated
2026-05-11

### Project Status
🟢 **Pipeline running end-to-end on YouTube.** 4 VODs queued to auto-publish on the NightDrive channel; each successive one is more feature-complete.

### What Was Done This Session (the big arc)

After §16 landed track #2 with MG continuation but no overlays, this session built out the whole video-production stack on top:

1. **OAuth bootstrap → channel verification.** Built `nightdrive-cli youtube auth`, walked Matt through Google Cloud Console setup, got `NIGHTDRIVE_YT_REFRESH_TOKEN` into `.env`. After track #1 hit `403 thumbnail.forbidden`, Matt phone-verified the channel — custom thumbnails now work, also unlocks >15min uploads + livestreaming. See `memory/project_youtube_channel_verified.md`.

2. **Type & VT323 typography pass.** Started with Cascadia Mono ("too soft" per Matt), swapped to VT323 (CRT/VHS pixel font, Google Fonts OFL). Bumped shadow/border, added BPM/key subtitle. Locked in `memory/feedback_vt323_locked.md`.

3. **TWC-style 3-panel layout.** Iterated v1-v6 with Matt on the side panel design: title floats above panels in cover bleed, left = radar inset, right = 5-day forecast with pink HI + cyan LO + per-day glyphs. Panels meet at center seam x=960. Locked in `memory/feedback_twc_3panel_layout_locked.md`.

4. **Real NWS forecast + radar.** Added `nightdrive_encoder::weather` module with parallel NWS `/points → /gridpoints/.../forecast` lookups. KAMX/KOKX/KVTX/KAMX radar GIFs downloaded + composited via ffmpeg `negate` (synthwave-magenta precip blobs, dark basemap). Every track archives full `forecast.json` (raw NWS response + timestamp) per Matt's "time capsule" framing. See `memory/feedback_radar_negate_locked.md`.

5. **Multi-city forecast cycling.** 4 cities per region rotate every 30s on the forecast panel (TWC "Local on the 8s" pattern). Time-gated drawtext layers via `enable='between(mod(t,120),slot_start,slot_end)'`. SE: Miami / Fort Lauderdale / Key West / Naples — each pulls its own NWS gridpoint so temps actually differ per slot. See `memory/feedback_4city_cycling_locked.md`.

6. **SDXL cover library.** Stood up a one-shot SDXL gen script (`sidecar/generate_cover_library.py`), attempted 25 covers but VRAM thrashing made each take 10-12 min instead of expected 30-45s. Killed at 2 covers, deferred. Orchestrator picks library covers via `djb2(track_id) % library_size`, falls back to ffmpeg gradient for unmapped tracks.

7. **MusicGen engine.** Replaced SAO as default audio engine. Audiocraft Windows install was painful (av wheel build, xformers torch conflicts) — solved with `--only-binary :all: av`, `audiocraft --no-deps`, force-reinstall torch 2.5.1+cu121, and a **stub xformers package** (audiocraft's module-level import doesn't actually call xformers at runtime when `_efficient_attention_backend == 'torch'`). Full recipe in `memory/reference_audiocraft_windows_install.md`.

8. **VRAM management lessons.** Killing chrome + discord freed ~2 GB. PyTorch's caching allocator can show "8.0/8.0 GB used" even at idle because it reserves blocks rather than releasing to OS. The real "performance gate" is whether per-segment time stays ~30s (good) or balloons to 8-9 min (thrashing — restart MG sidecar).

### Tracks shipped (NightDrive channel)

| video_id | title | engine | layout | upload time |
|---|---|---|---|---|
| `EGFUlex64L4` | Nocturnal Lanes (Synthwave for Coding) | SAO | gradient cover + showwaves | first VOD |
| `FGPUo7oXCI4` | Night Drive Echoes (Chillsynth for Coding) | MG continuation | gradient cover + showwaves | second |
| `zAEiQ4A-2ig` | Digital Dreams (Synthwave for Coding) | MG | 3-panel + single-city NWS + KAMX radar + VT323 + SDXL cover | third |
| `2NvOEfVbv2c` | Midnight Pulse (Late Night Programming Mix) [Synthwave for Coding] | MG | 3-panel + **4-city rotation** + KAMX radar + VT323 + SDXL cover | fourth |

All scheduled to auto-flip private→public 24h after upload.

### Current State

**Working:**
- `run-batch --count N` end-to-end: LLM → MG audio → mastering → 3-panel encode → YouTube upload with custom thumbnail
- NWS live data pull + KAMX/KOKX/KVTX/KAMX radar GIF download per track
- 4-city forecast cycling on the right panel (30s/city, 120s loop)
- VT323 title + subtitle + CTA overlays with proper shadows
- Cover library fallback chain (SDXL sidecar → library → ffmpeg gradient)
- `forecast.json` archive per track in `paths.root` — historical record of every track's weather snapshot
- `radar.gif` archive per track for the same time-capsule purpose
- Storage CRUD (sqlx, sqlite, witnessed)
- Workspace audit green (build:0 test:0 stubs:3 witnesses:7 stages:0,1,3,7)

**Broken / not yet shipped:**
- Real SDXL sidecar (kokonoe 3070 Ti can't host SDXL + MG concurrently — VRAM-contended). Cover library only has 2 covers from the abandoned full gen.
- Pipeline doesn't persist track rows to SQLite yet — storage is shipped, just not wired into orchestrator
- `nightdrive-orchestrator resume / status / livestream` still bail!() — separate N2.x roadmap items

### Blocking Issues

- **cnc P100s arrival ~2026-05-17** is the unlock for N1.5 (real SAO/MG on cnc), N1.7 (real SDXL inference, multi-tenant), N1.13 (systemd on arch-controller). Until then everything runs on kokonoe + Windows.
- **VRAM headroom on kokonoe**: MG-stereo-medium peaks at ~5 GB during inference, Windows desktop tax is ~1-2 GB, so we're always tight. PyTorch caching allocator can fragment under back-to-back model loads (SAO → MG → SDXL); fix is to kill + restart the sidecar between mode switches.

### What's Next (prioritized)

1. **Bench-runner row.** We've shipped 4 tracks but `docs/BENCH_LEDGER.md` hasn't been updated since 2026-05-10. The 7-day stale gate fires when witnesses ≥ 7 (which we are). Run the bench-runner agent to append a row for the pipeline.
2. **Storage integration into pipeline_one.** Insert track row at stage 1 (after spec generated), update_state at each stage transition. Currently the storage crate is shipped but pipeline_one doesn't call it.
3. **N2.1 resume subcommand.** Now real because pipeline_one is no longer stubbed. Query `tracks WHERE state != 'published'` and re-run from that stage forward.
4. **SDXL library expansion.** Either fix the kokonoe SDXL thrashing (maybe by closing more apps + using `enable_model_cpu_offload`) or wait for cnc P100s and run library gen on the 16 GB cards.
5. **Candle backend exploration.** Matt asked about this — see chat history. Confirmed we haven't actually benchmarked candle vs PyTorch for music generation. Following up means porting `MusicGenClient` to a candle backend and side-by-side benchmarking. The existing `candle-fork` (matt-voice-lora branch) already has Pascal compat patches.
6. **Forecast panel polish:** Crop NWS branding/legend bar from the radar GIF before composite. Currently visible at top + bottom of the radar inset.

### Notes for Next Session

- The `var/nightdrive/tracks/nd-20260511-001/` directory has the artifacts from the most recent run (track #4). Earlier runs overwrote each other because track_id is `nd-{today}-001` and all 4 runs were today. Per-track persistence requires Sequence > 1 or different date — orchestrator's `run-batch` always uses sequence=1.
- `var/` shouldn't be in git tracking — added to `.gitignore` this session but the files were already tracked from the initial commit. Need `git rm --cached var/` in a future session.
- `.env` has live YT OAuth refresh token bound to NightDrive channel (`UCsS7L4PUedZ-zus3rV3AMDw`). Gitignored. Don't commit.
- HF token is at `~/.cache/huggingface/token` (whoami: Suhteevah). audiocraft + diffusers auto-discover it.
- MG sidecar runs on `:8082` (not :8080 — lattice-server holds that). Config field `[audio_gen].base_url = "http://127.0.0.1:8082"`.
- VT323 lives at `assets/fonts/VT323-Regular.ttf` (downloaded from Google Fonts OFL). Committed; the rest of the font discussion is in `memory/feedback_vt323_locked.md`.
- The `xformers` package in the synthwave-gen venv is a **stub** (`{site-packages}/xformers/__init__.py` + `ops.py`). Real xformers wheels demand torch versions that conflict with the rest of the venv. Stub satisfies audiocraft's module-level import; the runtime path uses torch SDPA. Don't `pip install xformers` — it'll wreck the venv.
- Auto-publish schedule is 24h via `[youtube].schedule_offset_hours = 24` + `publishAt` in upload metadata. Tracks flip private → public on YouTube's side; we don't poll.
- Memory directory at `~/.claude/projects/J--nightdrive/memory/` has 12 entries documenting every locked design decision this session. Read the index in `MEMORY.md` before redesigning anything.

---

**Single-source-of-truth:** this file. Update it when decisions change.
