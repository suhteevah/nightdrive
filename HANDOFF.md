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

## 19. Session 2026-05-12 — Candle scoped, bench-row, storage wired, resume shipped

Worked through HANDOFF §18's "What's Next" punch list in order: 5, 1, 2, 3.

### 19.1 Candle backend scoped (item 5)
`docs/candle-backend-exploration.md` written. TL;DR: **defer**. Upstream
candle has a half-baked MusicGen example (text encoder only, decoder
`prepare_decoder_attention_mask` is `todo!()`, no `generate_continuation`,
no T5, no stereo, no 32 kHz EnCodec wiring). PR #2145 sat unmerged for
~13 months; issue #975 ("AudioGen/MusicGen") is `help wanted` with no
maintainer engagement. Effort to bridge: 3-6 weeks of focused Rust work
tracking a model audiocraft already ships correctly. Performance ceiling
is likely worse, not better. Keep the Python audiocraft sidecar. Re-open
when PR #2145 lands or a third-party crate publishes a working port.

### 19.2 Bench ledger fresh (item 1)
`docs/BENCH_LEDGER.md` now has 10 real rows from the live YouTube uploads
(7 stage rows for track #1 SAO, 1 pipeline_full for track #1, 1 for
track #2 MG, 1 for track #4 with full TWC stack). **Track #2 (MG
continuation) at 1072 s busts the ROADMAP 10-min wall-time gate by 79%**
— documented in the row's `note` column rather than silently massaged.
Cost-of-seamless: continuation re-encodes the prefix through EnCodec each
call. Accepted per §16's "worth it if the seams are gone." The 7-day
stale gate is reset to 1 day old.

### 19.3 Storage wired into pipeline_one (item 2)
`crates/nightdrive-orchestrator/src/main.rs`:

- `run_batch` opens `Db::connect_and_migrate` once before the loop.
- `pipeline_one(cfg, db, track_id, dry_run)` now persists at every stage
  boundary: `Tracks::insert` right after stage 1 spec succeeds, then
  `update_state` to `SpecGenerated` → `CoverRendered` (after the
  parallel 2+3 join) → `AudioMastered` → `VideoEncoded` → `Published`.
- Upload stage inserts an `Uploads` row in `queued` state *before* the
  PUT begins, then `set_youtube_id` flips it to `complete`. A
  mid-upload crash leaves a discoverable trail.
- `run_batch` catch-and-continue: on per-track Err, best-effort
  `Tracks::update_state(Failed)`. "track not found" is tolerated and
  logged (means stage 1 itself failed before insert).
- Note: state machine compresses parallel stages 2+3 directly into
  `CoverRendered`. `AudioRendered` is unreachable from the run-batch
  wiring but kept in the enum for storage compatibility + future
  sequential-rendering paths.

### 19.4 N2.1 resume subcommand (item 3)
`resume` is no longer a `bail!()`. Three new functions in `main.rs`:

- `run_audio_and_cover(cfg, spec, paths)` — extracted from
  `pipeline_one` so resume can call the same parallel audio+cover
  block without duplication.
- `resume_with_db(cfg, db, dry_run)` — inner body that lists every
  non-terminal track (`Pending`, `SpecGenerated`, `AudioRendered`,
  `CoverRendered`, `AudioMastered`, `VideoEncoded`) and dispatches to
  `resume_one` per row.
- `resume_one(cfg, db, row, dry_run)` — deserializes `spec_json` from
  the DB, re-materializes `spec.json` on disk if missing, then runs a
  monotonic dispatch chain: each `needs_*` boolean fires only when the
  row's stored state is at-or-before that stage. Stage transitions
  identical to `pipeline_one`. Per-track failures bubble up to
  `resume_with_db`'s catch-and-continue mark-Failed path.

`tests/witnesses/resume_skips_terminal_tracks.rs` (`// stage: 0`,
witness #8): spawns the actual built `nightdrive-orchestrator` binary
against a tempdir SQLite pre-populated with one `Published`, one
`Failed`, and one `VideoEncoded` track. Strips
`NIGHTDRIVE_YT_*` env vars so the VideoEncoded row's stage-7 upload
fails deterministically at `YoutubeCredentials::from_env()`. Asserts:
exit 0 (catch-and-continue), Published row untouched, Failed row
untouched, VideoEncoded row flipped to Failed. Real binary, real
SQLite, no mocks — passes in 4.01s.

### 19.5 Current audit

```
OK - audit clean (build:0 test:0 stubs:2 witnesses:8)
```

Stubs dropped from 3 to 2: `resume` is now real. Only `status` and
`livestream` remain stubbed in `crates/nightdrive-orchestrator/src/main.rs`
(N1.12-status, N2.4-livestream player).

Witness count climbed from 7 to 8 across stages 0, 1, 3, 7.

### 19.6 What's next (carried forward from §18 with deltas)

Resolved by this session: 1, 2, 3, 5.

Still open:
- **`status` subcommand** (the only remaining N1.12 stub) — print last
  successful batch timestamp, last failed track + reason, count per
  TrackState, livestream service status. Database surface is all there;
  it's purely a presentation layer.
- **N2.2 Track dedup** — orphaned `uploads` rows in `queued` state (the
  pattern §19.3 introduces) aren't yet re-processed by resume. Resume
  only looks at `tracks.state`. Either: (a) extend resume to scan
  `uploads.status='queued'` for re-tries, or (b) keep the current
  semantics and document the operator cleanup recipe.
- **N3.1 wgpu visualizer** — the big multi-week stage-5 unlock.
- **N4.2 Telegram escalation, N4.4 Disk-pressure guard, N4.6
  Prometheus exporter** — S-effort, on the post-MVP punch list.
- **Forecast panel polish** (item 6 carried forward — Matt keeps the
  NWS branding visible as a color guide, deliberately not cropped).
- **cnc P100s arrival ~2026-05-17** still gates N1.5, N1.7, N1.13.

## 20. Session 2026-05-13 — First full album shipped + Tron Vol. 1 staged

### Last Updated
2026-05-13

### Project Status
🟢 **Sunset Drive, Vol. 1 — 12 tracks live on NightDrive channel, scheduled trickle-public Wed 2026-05-14 05:52→08:50 UTC.** YouTube playlist + cover library + bonus cut + Tron Vol. 1 cover library all queued behind.

### What Was Done This Session (the big arc)

This session went from "manual single tracks running on autonomous-VOD scaffolding" to **first full coherent music-theory-architected album published as a YouTube playlist**, plus end-to-end automation that scales the same workflow to subsequent albums.

1. **Discipline stack tasks 1-3 + 5 (from §18 punch list).**
   - Item 5 — candle backend exploration: `docs/candle-backend-exploration.md`. Defer. Upstream candle's MusicGen example is text-encoder-only; PR #2145 sat unmerged 13 months; EnCodec at 24kHz not 32kHz; 3-6 weeks of from-scratch port for likely worse perf. Keep audiocraft.
   - Item 1 — bench ledger: 10 real rows appended to `docs/BENCH_LEDGER.md` from live YouTube wall times. Track #2 MG 1072s flagged as 79% over the 10-min ROADMAP gate (honest, not massaged).
   - Item 2 — storage wired into `pipeline_one`: `Db::connect_and_migrate` once per batch, `Tracks::insert` at stage 1, `update_state` per stage boundary, `Uploads::insert/set_youtube_id` around upload, catch-and-continue marks `Failed` in `run_batch`.
   - Item 3 — N2.1 resume subcommand: `resume_with_db` + `resume_one` + extracted `run_audio_and_cover` helper. Monotonic `needs_*` dispatch chain. Witness `tests/witnesses/resume_skips_terminal_tracks.rs` (#8) passes in 4s.

2. **SDXL cover library expanded.** 2 → 11 random library covers (slots 1-11 from the hand-tuned synthwave prompt list). Established that **low-vram mode (sequential CPU offload + slicing) is the right call on kokonoe** — confirmed with timing data: low-vram at 42-50s/cover beats no-low-vram at 215-312s/cover (latter saturates VRAM at 8/8 GB and spills to shared system memory). Memory: this is permanent on kokonoe.

3. **Album-composer subagent.** `.claude/agents/album-composer.md` — PhD-level studio musician + producer persona. Reads visual theme + track count + audience, designs a coherent album as honest music theory (cycle-of-fifths, modal interchange, motifs that recur at structural pivots, BPM arcs that mean something). Output is a single JSON consumed directly by the orchestrator. Tested across two album genres.

4. **Sunset Drive Vol. 1 — full 12-track album.** Composer-designed: ABA arch over time-of-day, cycle-of-fifths ascent (Am→C→G→Em→Bm), pivot to D major at dual peak (tracks 6-7), chromatic-mediant descent (D→F#m→A→F→Dm→Am) closing the ring. BPM arc 84→112→82. Two motifs threaded across the album: ascending major-7 sunset arp (t1 whole → t5 inverted → t8 fragmented → t12 whole-octave-down-half-tempo) and four-note descending highway-pulse offbeat figure (t3 harmonic support → t6 lead 8-bar refrain → t11 ghosted/filtered).

5. **The Disclosure Day pivot.** Original track 8 was "Afterglow Lane" — melancholy F#m comedown. SDXL cover gen produced an unidentified hovering craft in the sky from the "lavender afterglow" prompt. **Matt: "track 8 must be named disclosure day, this is non-negotiable."** Title flipped + composer notes rewritten (fragmented motif now reads as "the world's familiar tune cracking on the moment of revelation, quieter awe instead of melancholy"). Original Afterglow Lane preserved as **bonus track 13** outside the canonical 12-track album.

6. **Album-batch mode in orchestrator.** New `run-album --slug <slug> [--from-track N] [--to-track N] [--publish-at <RFC3339>] [--dry-run]` subcommand. Reads `docs/albums/<slug>.json`, skips stage 1 (LLM — spec pre-baked) and stage 3 (art — cover pre-rendered to disk, copied into per-track dir). Audio + master + encode + upload run identical to normal pipeline. Spec-from-JSON map handles the lossy JSON-vs-CompositionSpec schema difference (album JSON uses `key`/`key_relationship_to_prior`/`composer_notes` etc; pipeline wants `musical_key`/`youtube`/etc).

7. **Sync-drop publish-at flag** for synchronized 1-shot album drops. Vol. 1 used trickle by Matt's explicit call (`trickle is fine for this`); future albums target a single anchor timestamp via `--publish-at 2026-05-15T18:00:00Z`. Memory locked.

8. **Sunset Vol. 1 audio gen executed.** 12 tracks rendered sequentially via MG-stereo-medium continuation on the existing :8082 sidecar, ~14-18 min wall each, total ~2h 51m. 10 of 12 succeeded clean end-to-end. **Tracks 11 + 12 failed at YT stage 7** in different ways: 11 = chunked PUT transport-layer failure mid-upload (video never accepted); 12 = post-upload `thumbnails.set` returned 429 ("user uploaded too many thumbnails recently") which the old code bubbled as Err → marked the track Failed even though the video was already on YT.

9. **Thumbnail-429 bug fix** — `set_thumbnail_best_effort` helper in `main.rs`. Both 403 (channel unverified) and 429 (rate limit) are now downgraded to warn-and-continue; the video upload itself has succeeded by that point and YT's auto-generated thumbnail is acceptable. Applied at all three call sites (pipeline_one, pipeline_one_album, resume_one). Recovery for tracks 11 + 12 was hand-rolled SQL: track 12 → state=published (video was up), track 11 → state=video_encoded + delete orphan queued upload row, then `resume` re-attempted just stage 7 and landed `oxdlesFx-cI`.

10. **YouTube playlist live.** `https://youtube.com/playlist?list=PLc304hwLOBm_-REZSBQvRlhwTpq0bFiLA`. `scratch/create_album_playlist.py` reads `.env` for OAuth, refreshes access token, calls `playlists.insert` then `playlistItems.insert` 12× in canonical order. Description trimmed to title + narrative_arc + hashtags — the structured `overall_form` content triggered YT's anti-spam playlist heuristic with HTTP 400 "Invalid playlist snippet" (bisected against the live API; documented in the script). Per-minute quota also hit during bisect — defer further playlist work by a few min.

11. **Wallpaper pipeline shipped, then deprecated, then replaced.** `sidecar/wallpaper_pack.py` implemented the reflect-pad img2img outpaint approach (~40 min on all 24 covers). Output was **bad** — reflect-pad seeded the edges with mirrored content (cloned cars, double suns, cloned UFOs at low denoise strength). Matt: "some of the outpaints look meh, we should def avoid outpaint and just generate the covers at the correct ratio and crop to our needs." **Memory locked**: future albums gen at all 3 SDXL training-bucket resolutions natively (1024² + 1344×768 + 768×1344). `sidecar/generate_album_covers_native.py` implements this. Re-ran for Sunset Vol. 1 — 26 fresh native-aspect wallpapers replace the bad outpaints.

12. **Tron Drive, Vol. 1 plan + covers.** Spawned album-composer for the second album. 12 tracks, **all minor keys**, Möbius-strip ring form (entry → dissolution → exit on opposite face, ends in A minor like opener but FM-bell octave-up with derez tail). Modal logic instead of fifths (Phrygian, Locrian, Aeolian, Dorian rotation through the dissolution arc). BPM 96-112 (tight mechanical range vs Sunset's wide 82-112). Two motifs: PWM grid-pulse arp (filtered → unfiltered → glitch-stuttered → FM bell derez) and Phrygian bII derez-chord bracketing the dissolution arc. 36 covers rendered at all 3 native aspects (~26 min wall).

13. **Encoder TWC polish.** Two long-pending polish items shipped:
    - **Blue filler behind radar killed.** Sampled the NWS GIF — pale-cyan water fill is exactly `#C2EAF0` (59% of pixels). New filter chain: `format=rgba, chromakey=color=0xC2EAF0:similarity=0.12:blend=0.04, negate, scale=-1:480`. Surgical: water → alpha=0 → dark navy inset shows through; precip cyans untouched (different saturation/hue).
    - **Timestamp next to city name.** City header now: `5-DAY FORECAST · MIAMI · 14:30 UTC` using `forecast_data.fetched_at`. Width math: longest case "FORT LAUDERDALE" + timestamp = ~880px which fits the 920px right-panel space at fontsize 36.

### Tracks shipped this session (NightDrive channel)

```
01. First Light Off the Pier       SCpD4doyaWY   Am  84   opener
02. Coast Road                     u-SfzJUi460   C   88
03. Palm Shadows                   iQGHBqPyPpw   G   92
04. Magenta Mile                   ZFsC-IVkWHQ   Em  96
05. Half Sun                       CHqZyIq__xo   Bm 102   bridge-into-peak
06. Apex                           WulWSjAfAm0   D  108   peak 1
07. Vanishing Point                I0rJt6a0nbM   D  112   peak 2 (BPM ceiling)
08. Disclosure Day                 KXnZZ7hqrvg   F#m 106   ← UFO emerged from cover gen
09. Lavender Hour                  _xcjwu8938A   A  100
10. Embers on Chrome               -VHYwyPVi6I   F   94
11. Last Orange Sliver             oxdlesFx-cI   Dm  88
12. Lights Out, Dashboard Glow     d6Lq1psbFY8   Am  82   closer (ring close)

Playlist: PLc304hwLOBm_-REZSBQvRlhwTpq0bFiLA
```

### Current State

**Working:**
- Album-batch mode: `run-album --slug <slug>` end-to-end works (audio + master + encode + upload + state transitions + catch-and-continue).
- Sync-drop publish-at flag ready for Tron + future albums (`--publish-at 2026-05-15T18:00:00Z`).
- Thumbnail set is best-effort: 403 (unverified) and 429 (rate-limited) downgraded to warn, video upload succeeds either way.
- Native-aspect cover gen (`generate_album_covers_native.py --slug <slug>`) produces 3 covers per track in ~150s/track total.
- YouTube playlist creator works against the v3 API; description must stay narrative-only (the structured `overall_form` content trips YT's anti-spam heuristic).
- Encoder filter graph: water-blue chromakey + city header timestamp applied. Release binary rebuilt 2026-05-13.

**Pre-existing tracks + content:**
- Sunset Drive Vol. 1: 12 published, scheduled trickle-public Wed 05:52→08:50 UTC. (These rendered with the OLD encoder filter graph — no chromakey, no city timestamp. If you want them re-encoded retroactively, see "What's Next.")
- Tron Drive Vol. 1: covers ready, audio gen NOT started.
- Bonus track 13 (Afterglow Lane): cover exists, audio gen never run. Standalone single, no priority.

**Broken / known issues:**
- The bad outpainted wallpapers in `assets/wallpapers/sunset-drive-vol-1/` are still on disk. The fresh native-aspect versions live under `assets/covers/albums/sunset-drive-vol-1/track-NN-{desktop,phone}.png`. A small "publish_wallpapers" cleanup step needs to copy the good ones to the public `assets/wallpapers/` location and delete the outpaints. Not done.
- Track 8 wallpaper variants (desktop + phone) **don't have the UFO** — the 1024² album cover does. Different seeds per aspect → SDXL produced different scenes from the same `cover_prompt`. Either accept the duality (canonical Disclosure Day cover has craft; wallpapers are "broader establishing shots") or update track 8's `cover_prompt` to explicitly name the hovering craft and re-gen the two non-1024² variants.
- The audit gate (`scripts/audit.ps1`) hasn't been run since the album-batch + encoder polish landed. Should be re-run.
- The `wallpaper_pack.py` script is deprecated but still on disk. Per memory it should not be used; consider deleting.

### Blocking Issues

None immediate. Pending decisions:
1. **Auto-chain Tron audio gen, or hold for explicit go?** ~3-3.5h MG sequential commit; the MG sidecar was killed for the SDXL work and needs to be restarted first.
2. **UFO-in-wallpaper retrofit for track 8?** Optional; the duality argument is solid.
3. **Re-render the 12 Sunset Vol. 1 tracks** to apply the new encoder filter (chromakey + timestamp) before they flip public? Tomorrow 05:52 UTC. Cost: re-encode + re-upload ~20 min for 12 tracks, plus YT video replacement logistics (delete old + re-upload as new + replace in playlist). Probably not worth it — first album ships with the old look, Tron is the first to show the polish.

### What's Next (prioritized)

1. **Matt's call on Tron audio gen.** When given, restart MG sidecar (`uvicorn sidecar.musicgen_server:app --host 127.0.0.1 --port 8082 --workers 1`), kick off `run-album --slug tron-drive-vol-1` (with `--publish-at` if synchronized drop wanted). ~3-3.5h wall.
2. **Publish-wallpapers cleanup step** — small script to copy `assets/covers/albums/<slug>/track-NN-{desktop,phone}.png` → `assets/wallpapers/<slug>/` and delete the bad outpaints. ~20 lines of Python.
3. **`status` subcommand** — last of the N1.12 stubs. Print: last successful batch timestamp, last failed track + reason, count per TrackState, livestream service status. Pure presentation layer; the data is in storage.
4. **Tokyo Cyberpunk Vol. 1** — third planned album. Album-composer can run any time (no GPU); generates the JSON plan ready for cover gen + audio.
5. **N2.2 Track dedup** — orphan `uploads` rows in `queued` state (the pattern that bit tracks 11 + 12) aren't currently re-processed by resume. Either extend resume to scan `uploads.status='queued'` or document the operator cleanup recipe.
6. **Bonus track 13 audio gen** — standalone single, low priority. ~17 min for one track when there's a slot.

### Notes for Next Session

- The release binary at `target/release/nightdrive-orchestrator.exe` has the new chromakey + city timestamp. Sunset Vol. 1's already-uploaded 12 videos used the OLD binary.
- MG sidecar is currently DOWN. Restart with: `& "J:\pledgeandcrowns\tools\synthwave-gen\.venv\Scripts\python.exe" -m uvicorn sidecar.musicgen_server:app --host 127.0.0.1 --port 8082 --workers 1` — ~16s model load, ~3.4 GB VRAM idle.
- **Don't run wallpaper_pack.py.** Deprecated. Use `generate_album_covers_native.py --slug <slug> --low-vram` for any wallpaper retrofit OR generate at all 3 aspects in the first album cover pass.
- **kokonoe's GPU is more efficient in low-vram mode than no-low-vram.** Counter-intuitive but documented with timing: low-vram CPU-offload at ~42-50s/cover beats no-low-vram at ~215-312s/cover because the latter saturates 8/8 GB and spills to shared system memory. Always pass `--low-vram` on kokonoe SDXL gens.
- **YT playlist API anti-spam heuristic**: descriptions with structured "Form: <text>" content + tracklist with key signatures trip HTTP 400 "Invalid playlist snippet." Keep playlist descriptions narrative-only.
- **Sunset Vol. 1's bonus track 13** has a cover at `assets/covers/albums/sunset-drive-vol-1/track-13.png` (fresh native-aspect, no UFO) and `track-13-{desktop,phone}.png`. Its audio_gen has never run; the orchestrator's `run-album` would render it if `--to-track 13` is passed. Not in the playlist by design.
- **`docs/albums/<slug>.json` is canonical** — both `sunset-drive-vol-1.json` and `tron-drive-vol-1.json` carry the full music-theory rationale, key relationships, BPM logic, motif tracking, narrative arc, per-track composer notes. Read these before designing any follow-up volume to maintain stylistic differentiation.
- **`.claude/agents/album-composer.md`** is the persona. For Tokyo Cyberpunk, dispatch with similar setup as the Tron run (read persona, read prior album JSONs for differentiation, design, write JSON, summarize under 300 words).
- **Spring (Teespring) is the picked merch platform** when monetization unlocks. YouTube Merch Shelf integration is the deciding factor; Amazon-owned for trust. Pair with Printful + Gumroad for higher-quality direct-to-fan sales. Wallpaper-pack work IS the print-file prep — same upscaled covers.
- **audit gate** (`powershell -ExecutionPolicy Bypass -File scripts/audit.ps1`) hasn't been run since the album-batch + encoder polish changes landed. Run it before claiming "clean" externally.

---

## 21. Session 2026-05-16 — Tron Drive Vol. 1 shipped (sync drop set)

### Last Updated
2026-05-16

### Project Status
🟢 Second full album rendered + uploaded clean. Sync-drop public flip armed for Fri 2026-05-15 15:00 UTC (8 AM PT). Channel now carries two albums (Sunset Drive Vol. 1 + Tron Drive Vol. 1).

### What Was Done This Session

1. **Two track-title renames on `docs/albums/tron-drive-vol-1.json`** to dodge double-collision risk:
   - Track 7 `"Recognizer"` → `"Scan Subroutine"`
   - Track 12 `"Derez (User Exits)"` → `"User Exits"`
   - Both originals were simultaneously (a) named dialogue/objects in Tron 1982 AND (b) literal Daft Punk track titles on the Tron Legacy soundtrack. Single-hit is fine (unavoidable in a tribute album); double-hit is takedown territory on a monetized channel. Rule saved as `feedback_album_title_danger_zone.md` + linked in `MEMORY.md`. Composer-internal motif names (`derez chord`, `recognizer subroutine` in the prose) left in place — those never reach the public.

2. **MG sidecar bring-up.** Started `sidecar/musicgen_server.py` on `127.0.0.1:8082` via the synthwave-gen venv python, ~16s model load, idle VRAM 5.58 GB on the 3070 Ti. Logs: `scratch/mg-sidecar-tron.log`, `scratch/mg-sidecar-tron.err.log`.

3. **`run-album --slug tron-drive-vol-1 --publish-at 2026-05-15T15:00:00Z`** kicked off. Sync drop chosen per `feedback_sync_drop_for_future_albums.md` (Vol. 1 trickle was the one-time exception). Anchor = Fri 8 AM PT (UTC-7 PDT). The orchestrator validated `--publish-at` was ≥1h in the future and stamped that exact RFC3339 timestamp on every track's `scheduled_publish_at`.

4. **Wall time:** start 13:18:21Z, finish 16:09:43Z = **~2h 51m** for all 12 tracks (matches Sunset Vol. 1's ~2h 51m exactly — MG-stereo-medium continuation pacing is stable). 0 ERROR lines in the log. stderr empty.

5. **Result:** 12/12 tracks rendered + mastered + encoded + uploaded to the NightDrive channel, all scheduled `private → public` at the anchor timestamp. Auto-publish at the anchor still rides YT's scheduler — the videos are uploaded `private` with `publishAt` set, YT flips them at the moment.

6. **Thumbnail 429s on tracks 11 + 12.** Same YT per-channel `~100/day` thumbnail-upload ceiling that bit Sunset Vol. 1. Both tracks fell back to YT's auto-generated thumbnail per the `set_thumbnail_best_effort` helper (downgrade 429 to warn-and-continue). Video upload itself succeeded for both — only the custom thumbnail upload was suppressed. **Retry recipe:** `nightdrive-cli thumbnails retry-failed` once the 24h window clears (~tomorrow). Both tracks will get their custom Tron covers swapped in then.

7. **MG sidecar killed post-run.** Was camping the full 8/8 GB VRAM (the model + activation cache ceilings into the headroom once gen completes). PID 6180 stopped, port 8082 free.

### Tracks shipped this session (NightDrive channel — Tron Drive Vol. 1)

```
01. On The Grid (From Outside)              Am(104)
02. Perimeter Trace                         Em(108)
03. Light Trail                             Bm(110)
04. Enter The Lattice                       F#m Phrygian (112)
05. Disassembly                             C#m Dorian (100)
06. Memory Cache                            G#m Locrian-shaded (96)   ← BPM floor
07. Scan Subroutine                         Dm Aeolian (98)            ← renamed from "Recognizer"
08. Recompile (Colder Shape)                Am Phrygian-shaded (102)   ← structural midpoint
09. Light-Cycle Sprint                      Em (108)
10. I/O Tower                               Bm (110)
11. Breach                                  Am (112)                   ← thumbnail 429
12. User Exits                              Am (100)                   ← renamed from "Derez (User Exits)"; thumbnail 429
```

Sync drop: **2026-05-15T15:00:00Z** (Fri 8 AM PT). YouTube IDs available in `var/nightdrive/nightdrive.sqlite` (table `uploads`) — query when needed.

### Current State

**Working:**
- Tron Drive Vol. 1 fully on YT, sync-flip armed.
- MG sidecar lifecycle (start → render album → kill) clean.
- `--publish-at` flag end-to-end validated against the live YT API.
- Title-collision rule documented + memory'd so album-composer doesn't re-suggest movie-quote+DP-track double-hits on future albums.

**Pending (non-blocking):**
- Tracks 11 + 12 custom thumbnails not yet on YT (auto-thumb fallback active). Retry with `nightdrive-cli thumbnails retry-failed` once the 24h thumbnail quota clears.
- Sunset Vol. 1 → Tron Vol. 1 differentiation now mostly visual + textual (cool palette, tighter BPM, no major keys, Möbius-strip form). Worth a chat in the YT description templates if we want the listener to feel the contrast deliberately.

**Broken / known issues:**
- Carried forward from §20: the bad outpainted wallpapers in `assets/wallpapers/sunset-drive-vol-1/` still on disk, publish-wallpapers cleanup script not written.
- Carried forward: `scripts/audit.ps1` not run since §20's encoder polish + this session's tron drop. Run it before the next external claim.

### Blocking Issues

None. Pending decisions:
1. **Playlist creation for Tron Vol. 1.** Same `scratch/create_album_playlist.py` pattern works; needs a slug arg added (or duplicate + s/sunset-drive-vol-1/tron-drive-vol-1/g). Description must stay narrative-only per §20 #10 (structured `Form:`/key-signature content trips YT's playlist anti-spam heuristic). 5 min of work.
2. **Wallpaper retrofit + publish.** Same as §20's carry-forward. Tron's 36 covers at 3 aspects are in `assets/covers/albums/tron-drive-vol-1/`; the `assets/wallpapers/tron-drive-vol-1/` public bucket doesn't exist yet.

### What's Next (prioritized)

1. **`nightdrive-cli thumbnails retry-failed`** for tracks 11 + 12 once the 24h YT thumbnail window clears (~2026-05-17 13:00Z). Two API calls.
2. **Playlist for Tron Vol. 1** — copy `create_album_playlist.py` → parameterise on slug, run it. URL goes into the channel's playlist tab.
3. **Publish-wallpapers cleanup script** (deferred from §20). ~20 LOC Python, hits both Sunset + Tron buckets.
4. **`status` subcommand** (deferred from §20). Last successful batch + last failure + per-state counts + livestream service status.
5. **Tokyo Cyberpunk Vol. 1** — third planned album. Album-composer can run any time; CLAUDE.md's "240min catalog before livestream" memory says we want ≥60 tracks before flipping on the livestream channel + real NWS data. Two albums = 24 tracks ≈ ~106 min. Three more albums ≈ 60 tracks ≈ 4 hours.
6. **N2.2 dedup of orphan `uploads.status='queued'` rows** (deferred from §20).
7. **Bonus track 13 (Afterglow Lane)** audio gen (deferred from §20).

### Notes for Next Session

- **Album title hygiene** (new rule): cross-reference any tribute-album track title against (a) the source film's dialogue/named objects AND (b) the canonical soundtrack album. Reject double-hits. Saved as `feedback_album_title_danger_zone.md`. The Tron run caught two — `Derez`/`Recognizer` — both Daft Punk track titles AND film terms. Future Vol. 2+ runs need a sweep step before the JSON is finalized.
- **MG sidecar VRAM ceiling**: the audiocraft model on a 3070 Ti starts at ~3.4 GB idle, climbs to 5.58 GB once a render starts, and post-album can sit at 8/8 GB until the process is killed. Always kill it after `run-album` finishes (it doesn't release on idle).
- **Sync drop validated end-to-end.** The orchestrator stamped `publishAt = 2026-05-15T15:00:00Z` on all 12 tracks; YT accepted it (videos uploaded `private` with `publishAt` field). The "≥1h in future" guard caught nothing here (anchor was ~46h out) but the path is exercised.
- **MG sidecar restart command** unchanged from §20 notes:
  ```
  & "J:\pledgeandcrowns\tools\synthwave-gen\.venv\Scripts\python.exe" -m uvicorn sidecar.musicgen_server:app --host 127.0.0.1 --port 8082 --workers 1
  ```
- **PDT conversion for sync drop**: PT in May = UTC-7 (PDT). 8 AM PT = 15:00 UTC. The orchestrator wants RFC3339 with `Z` (or explicit offset).
- **Title-rename safety**: the title field in `docs/albums/<slug>.json` is the only user-visible source. The composer notes / motif names elsewhere in the JSON are internal-only and never surface to YT or the playlist. Safe to keep "derez chord" / "recognizer subroutine" in the prose for music-theory continuity.

---

## 22. Session 2026-05-16 — Audio-gen rewire foundation (ACE-Step + stems + export)

### Last Updated
2026-05-16

### Project Status
🟡 **Rewire scaffold landed; sidecar bring-up + A/B pending.** Code path for
ACE-Step 1.5 (MIT-licensed local song-gen, single-shot full-track, no
30s seams) is in place end-to-end. Cargo workspace green, all unit
tests pass, audit clean at `OK build:0 test:0 stubs:2 witnesses:11`
(stages 0,1,2,3,4,7). MG continuation engine remains the default until
Matt A/Bs the first ACE-Step track and flips `[audio_gen].engine`.

### What Was Done This Session

1. **Deep dive on the audio-gen problem.** Findings in
   `scratch/audio_gen_deepdive_2026-05-16.md` (~4k words):
   - MG seams ≠ random; root causes are (a) same prompt sent for every
     segment so the model never knows when to evolve, (b) EnCodec
     prefix round-trip per continuation, (c) production-character drift
     between calls
   - 2026 local model menu: ACE-Step 1.5 (MIT, <4 GB VRAM, single-shot
     4-min) is the clean replacement; MBD is the cheap in-place upgrade
     for MG; DiffRhythm 2 / YuE deferred (instrumental-only mode not
     ready / heavy hardware respectively)
   - Spotify path: just FLAC + DistroKid; no new pipeline work needed
     beyond the export bundler
   - Editability path: Demucs `htdemucs_ft` stems → basic-pitch / MT3
     MIDI transcription (Phase 3+, optional)

2. **ACE-Step sidecar + install playbook** (Phase 1 — primary engine swap):
   - `sidecar/acestep_server.py` — FastAPI wrapper around ACE-Step 1.5
     handler-based API. POST /generate { caption, lyrics, duration,
     bpm, key, seed, guidance_scale, inference_steps } → audio/wav.
     Single-shot full-song generation, no segment chain. Auto-detects
     Pascal `sm_60` and forces `ACESTEP_LM_BACKEND=pt`. Includes
     fallback path for older `ACEStepPipeline` API if the handler
     import shape shifts.
   - `scripts/install_acestep.ps1` — idempotent installer: installs
     `uv`, clones `ace-step/ACE-Step-1.5` to `J:\acestep` (or
     `$env:NIGHTDRIVE_ACESTEP_ROOT`), runs `uv sync`, pre-downloads
     model weights (~10 GB), prints the sidecar run command.
   - **Not yet executed** — Matt runs `scripts/install_acestep.ps1`
     when he wants to bring it up. Sidecar will be on `:8083`.

3. **Rust client wiring** (`crates/nightdrive-audio-gen`):
   - New `pub mod prompt` with deterministic `format_ace_step_caption`,
     `format_ace_step_lyrics`, `format_musicgen_section_prompt`, and
     `section_for_time` helpers. Translates a `CompositionSpec` →
     engine-native prompts. **Pure Rust, no LLM call** — the "prompt
     engineer" role from the deep-dive is a stateless module, not an
     agent.
   - `AceStepClient` implementing `AudioGenerator` trait, single-shot
     POST → write WAV directly to `paths.raw_audio_wav()`. Headers
     `X-Nightdrive-Gen-Wall-Seconds`, `X-Nightdrive-Inference-Steps`
     surfaced for observability.
   - `client_for(cfg)` factory dispatches on `engine = "ace_step"`
     alongside existing `"stable_audio"` and `"musicgen"`. Older
     configs without `engine` default to stable_audio (unchanged).
   - `AudioGenConfig` gains an `inference_steps: u32` (default 32)
     field — `8` for turbo variants.
   - 7 new unit tests in `prompt::tests`, all passing.

4. **Arranger Claude subagent** — `.claude/agents/arranger.md`. Optional
   layer between `album-composer` and the audio-gen engines. Enriches
   sparse per-track `sections[].instrumentation` strings into vivid,
   model-friendly section hints (spatial detail, processing references,
   continuity prose). On-demand only — not pipeline-wired. Designed to
   not step on the composer's cross-track decisions (key/BPM/role
   stays untouched).

5. **`nightdrive-stems` crate (NEW)** — Demucs CLI wrapper.
   - `StemSeparator` trait + `DemucsCli` impl, shells out to `demucs
     -n htdemucs_ft -o <stems_dir> --device <cuda|cpu> [--shifts N]
     <master.flac>`, normalizes the model-nested output layout to
     canonical `<track_root>/stems/{drums,bass,vocals,other}.wav`.
   - Lightweight vocal-presence QC: warns if `vocals.wav` is
     suspiciously large for an instrumental track (>10 % of
     master.flac size).
   - Added to workspace `Cargo.toml` members + workspace deps.

6. **`nightdrive-cli` new subcommands**:
   - `nightdrive-cli stems generate --album <slug> [--track N]` — runs
     demucs on every track in an album JSON, finds artifact dirs by
     matching `spec.json.title` against the album's
     `tracks[*].title`. Skips tracks without `master.flac` or that
     already have `stems/`.
   - `nightdrive-cli export album --slug <slug> [--out PATH]
     [--include-stems]` — bundles FLAC + cover + optional stems into
     `exports/<slug>/<NN> - <Title>.flac`, writes `README.txt`.
     Spotify/DistroKid upload-ready.

7. **Three new witness tests** (all real-endpoint, no mocks per
   `tests/witnesses/README.md`):
   - `ace_step_real_sidecar.rs` (stage 2) — skips when
     `NIGHTDRIVE_ACESTEP_URL` unset; calls real sidecar with a 20s
     duration target, asserts WAV signature + duration ±20 %.
   - `stems_real_demucs.rs` (stage 4) — skips when `demucs` not on
     PATH; runs Demucs on a real shipped `master.flac` (or
     env-overridable fixture), asserts 4 stems exist + vocals.wav not
     implausibly large.
   - `cli_export_album.rs` (stage 0) — stages fake album JSON +
     spec.json + master.flac in a tempdir, runs the actual
     `nightdrive-cli` binary, asserts the export bundle.
     **End-to-end witness against the real built binary; passes.**

### Current State

**Working (Cargo-green + audit-clean):**
- ACE-Step Rust client + sidecar + prompt module — code path complete
- Stems crate (CLI shell-out) with `nightdrive-cli stems generate`
- Export bundler with `nightdrive-cli export album`
- 11 witnesses across stages 0, 1, 2, 3, 4, 7
- `cargo test --workspace` passes; release binaries built

**Not yet done (deferred Phase 0 items from the deep dive):**
- **Per-section MG prompts** in `MusicGenClient::render` — `prompt::
  format_musicgen_section_prompt` exists but `MusicGenClient` still
  sends `spec.musicgen_prompt` for every segment. Wiring it through is
  ~30 LOC if/when we keep MG around for legacy renders.
- **MBD (Multi-Band Diffusion)** on the MG sidecar — drop-in quality
  boost for the MG path; deferred since we're moving primary to
  ACE-Step.
- **Continuation prefix bump 5s → 8s** — config knob already exists,
  just hasn't been bumped in the live nightdrive.toml.

**Not yet integrated:**
- Stems generation is *operator-triggered* (`nightdrive-cli stems
  generate`); not auto-called by `pipeline_one_album`. Adding it as a
  stage 4.5 hook is a small follow-up.
- The `arranger` subagent is on-demand; not invoked automatically by
  `run-album`. By design — the composer's output is good enough most
  of the time.
- Live `[audio_gen].engine` is still `"musicgen"` in the runtime config.
  Switch happens after Matt's first A/B listen against ACE-Step.

### Blocking Issues

None. The remaining work is **operator-side install + first run**:

1. **Run `scripts/install_acestep.ps1`** to install ACE-Step into
   `J:\acestep` + download the ~10 GB of model weights. One-time, ~15-30
   min including download.
2. **Start the sidecar** on port 8083:
   ```powershell
   $env:NIGHTDRIVE_ACESTEP_ROOT = "J:\acestep"
   $env:NIGHTDRIVE_ACESTEP_CONFIG = "acestep-v15-turbo"
   & "J:\acestep\.venv\Scripts\python.exe" -m uvicorn sidecar.acestep_server:app --host 127.0.0.1 --port 8083 --workers 1
   ```
3. **A/B test** — render one Tokyo Cyberpunk Vol. 1 track via ACE-Step
   (point `[audio_gen].base_url` + `engine = "ace_step"` at the new
   sidecar) and compare against an MG render of the same track JSON.
   Matt's ear decides.

### What's Next (post bring-up)

1. **Bench-runner**: append a row for the rewire (the audit's `5 days
   old` last-bench is right at the gate — fresh row needed before any
   external claim about ACE-Step performance).
2. **Wire stems generation into `pipeline_one_album`** (stage 4.5 hook
   after mastering) so every new album auto-produces stems.
3. **Phase 0 carryback if MG stays in rotation**: section-aware MG
   prompts + MBD + 8s prefix.
4. **Tokyo Cyberpunk Vol. 1** — first ACE-Step album, clean signal on
   whether the engine swap is heard.
5. **Bonus track 13 audio gen** (carried from §20) — could be the
   ACE-Step debut single since it's standalone.
6. **Carried**: thumbnails retry-failed, Tron Vol. 1 playlist, wallpaper
   cleanup, `status` subcommand, dedup.

### Notes for Next Session

- **`docs/albums/<slug>.json` shape**: the export-album CLI deserializes
  a minimal subset (`album_slug`, `title`, `tracks[].track_number +
  title`) so older album JSONs missing newer optional fields don't
  break export.
- **Title-match indexing**: `build_title_index()` in
  `nightdrive-cli/src/main.rs` walks `paths.tracks_dir`, parses every
  `spec.json`, builds `title → root` map. O(N) per album-export call.
  Good enough for ~hundreds of tracks; revisit if catalog blows up.
- **Cargo workspace gained `nightdrive-stems`** — 1 new member crate +
  1 new workspace dep. Cargo.toml `[workspace.dependencies]` updated.
- **`AudioGenConfig::inference_steps`** new field, default 32. Pre-existing
  TOML configs without this field will deserialize fine (serde default
  kicks in). Only matters when `engine = "ace_step"`.
- **The `arranger` subagent is intentionally minimal** — only touches
  `sections[*].instrumentation` strings. Doesn't change titles,
  BPM, key, motifs, narrative arc. If a composition decision needs
  changing, that's still `album-composer`'s job.
- **ACE-Step license is MIT** — clean for the monetized NightDrive
  channel. Once we flip the engine, the `feedback_musicgen_commercial_risk_accepted`
  memory becomes historical context rather than active license posture.
  Don't delete the memory file yet; the MG tracks already published
  still ride that risk until the licenses retroactively expire (they
  don't — but they're past the cease-and-desist window per Matt's
  read).

---

## 23. Session 2026-05-16 (continued) — ACE-Step installed; kokonoe 8 GB hits hard wall

### Last Updated
2026-05-16

### Project Status
🟡 **ACE-Step 1.5 installed clean (~10 GB weights on disk, deps green,
sidecar boots, /health 200). Cannot generate on kokonoe 8 GB.** Smoke
test deferred to cnc P100s (~2026-05-17).

### What Was Done This Session (continued from §22)

1. **Ran `scripts/install_acestep.ps1`** (twice — first run died on a
   PowerShell encoding bug, em-dash characters were read as
   `â€"` by PS 5.1 because the Write tool emits UTF-8 without BOM and
   the system codepage isn't UTF-8). Patched the script to ASCII-only
   on the second run. Install completed end-to-end:
   - uv 0.11.14 installed
   - `git clone ace-step/ACE-Step-1.5` to `J:\acestep`
   - `uv sync` installed 123 packages including torch 2.7.1+cu128
   - ACE-Step model weights downloaded from HuggingFace into
     `J:\acestep\checkpoints` — **9.4 GB across 57 files** in 4 subdirs:
     - `acestep-v15-turbo/` (5 files, 4.46 GB — DiT decoder)
     - `acestep-5Hz-lm-1.7B/` (9 files, 3.50 GB — 5 Hz LM head)
     - `Qwen3-Embedding-0.6B/` (9 files, 1.12 GB — text encoder)
     - `vae/` (2 files, 0.31 GB — audio VAE)

2. **Install-script side-issue:** the smoke-test step at the end of the
   installer calls `AceStepHandler.initialize_service(device="cuda:0")`
   to verify the handler loads. That loads ~5 GB into VRAM as a
   verification step, which I described in chat as "pre-download
   weights" without flagging the VRAM cost. Matt's call to "make sure
   you dont leave anything in vram" caught it. Smoke-test process
   killed cleanly post-verification. **Memory saved: be explicit about
   every VRAM/GPU-touching step in user-facing descriptions.**

3. **Brought up the actual sidecar on :8083 in two configs:**
   - Full mode (DiT + 5Hz LM + Qwen3 embedding): /health reported
     `vram_used_gb: 8.0/8.0`, generation rejected with
     `Insufficient free VRAM: need ~0.8 GB, only 0.1 GB available` per
     ACE-Step's pre-flight check.
   - DiT-only mode (`NIGHTDRIVE_ACESTEP_DIT_ONLY=1` env var added to
     `sidecar/acestep_server.py`) + `PYTORCH_CUDA_ALLOC_CONF=
     expandable_segments:True,max_split_size_mb:128`: same VRAM
     ceiling — `0.4 GB free` after the allocator grew into unreserved
     blocks, still short of the 0.8 GB activation buffer requirement.
   - Tried duration=10s (the schema min) since ACE-Step's error message
     hints at "reduce duration" — but the pre-flight buffer is a
     fixed ~0.8 GB regardless of duration below 30s.

4. **The math, honestly:**
   - Windows + apps baseline: 2.1 GB
   - ACE-Step turbo DiT (fp16): ~4.5 GB
   - Qwen3-Embedding-0.6B: ~1.0 GB
   - VAE: ~0.3 GB
   - ACE-Step activation pre-flight: 0.8 GB
   - **Total: ~8.7 GB demanded on an 8 GB card.**
   - Even DiT-only (skipping 5Hz LM head's ~1.5 GB) doesn't close the
     gap because the embedding + VAE + activation buffer still puts us
     ~0.4 GB over.

5. **Sidecar killed, VRAM verified clean.** nvidia-smi
   `--query-compute-apps` shows zero python/uv processes on the GPU.
   The 2.7 GB baseline is Chrome / Discord / Ollama / Edge WebView2 /
   Photos / system processes — all Matt's, none from this session.

6. **Witness test `ace_step_real_sidecar` ran but FAILED** because gen
   never started. The test code itself is correct — it surfaces
   ACE-Step's pre-flight error through the AudioGen error variant
   cleanly. Re-runs will pass once we're on cnc P100 (16 GB) where
   neither the DiT load nor the activation buffer is a constraint.

7. **Deep-dive doc moved** from `scratch/` (gitignored ephemera) to
   `docs/audio_gen_deepdive_2026-05-16.md` so it's part of the
   project knowledge tree.

8. **gitignore additions:** `**/__pycache__/` + `*.pyc` (sidecar
   bytecode caches now exist after the first sidecar boot).

### Current State

**Working:**
- ACE-Step 1.5 fully installed at `J:\acestep` — uv venv at
  `J:\acestep\.venv\Scripts\python.exe`, weights at
  `J:\acestep\checkpoints/{acestep-v15-turbo,acestep-5Hz-lm-1.7B,
  Qwen3-Embedding-0.6B,vae}/`.
- `sidecar/acestep_server.py` boots clean (~30-60s model load), exposes
  GET /health + POST /generate. Handles `NIGHTDRIVE_ACESTEP_DIT_ONLY=1`
  env var to skip LM init.
- `config/nightdrive-acestep.toml` ready to drop in via
  `NIGHTDRIVE_CONFIG` env var or `--config` flag — `engine =
  "ace_step"`, `base_url = "http://127.0.0.1:8083"`,
  `inference_steps = 8` for turbo.
- Rust workspace audit-clean; AceStepClient unit-tested via 7 prompt
  module tests, request schema validated end-to-end (422 on under-min
  duration, 500 with structured detail on VRAM rejection).

**Blocked on hardware:**
- Phase C (witness test) and Phase D (full pipeline_one with
  engine=ace_step) both require ≥10 GB VRAM headroom for an 8s+ render.
  cnc P100 (16 GB) is the right hardware.

**Not started this session:**
- Stems pipeline integration into `pipeline_one_album` (still
  operator-triggered via `nightdrive-cli stems generate`)
- Phase 0 wins (per-section MG prompts wiring, MBD on MG sidecar) —
  still deferred since we're betting on ACE-Step

### Blocking Issues

1. **kokonoe 8 GB VRAM is structurally insufficient for ACE-Step
   turbo.** Not a bug, just hardware reality. Move sidecar deployment
   to cnc P100s when they land (~2026-05-17 per memory file
   `project_cnc_p100_arrival`).

### What's Next (in order)

1. **Wait for cnc P100s** to arrive. Per `project_cnc_p100_arrival`
   memory, expected ~2026-05-17. 3 × P100 16 GB each = 48 GB total
   for the audio-gen + art workload.
2. **Deploy `sidecar/acestep_server.py` on cnc-server** with
   `ACESTEP_LM_BACKEND=pt` env var (Pascal sm_60 has no vLLM
   support — ACE-Step auto-falls-back to PyTorch but explicit is
   faster). The sidecar's existing `auto` mode handles this too.
3. **Update `config/nightdrive-acestep.toml`** to point
   `[audio_gen].base_url` at the cnc Tailscale endpoint
   (`http://cnc-server.tailb85819.ts.net:8083`).
4. **Re-run Phase C witness** with full LM mode (no
   NIGHTDRIVE_ACESTEP_DIT_ONLY) — proves the integration on intended
   hardware.
5. **Re-run Phase D full pipeline** — `nightdrive-orchestrator
   run-batch --count 1 --dry-run` with NIGHTDRIVE_CONFIG=ace_step toml.
   A/B against an MG-rendered track for ear-quality comparison.
6. **Lock the engine flip** by promoting
   `config/nightdrive-acestep.toml` → `config/nightdrive.toml` if the
   ear test passes. Future albums (Tokyo Cyberpunk Vol. 1+) render via
   ACE-Step.

### Notes for Next Session

- **The `audit.ps1` gate has not been re-run** since the smoke-test
  session. It was clean before (build:0 test:0 stubs:2 witnesses:11)
  but the witness test in `ace_step_real_sidecar.rs` will SKIP cleanly
  unless `NIGHTDRIVE_ACESTEP_URL` is set in the audit's environment —
  the witness's env-not-set early-return path is the correct behavior
  for an offline audit.
- **PowerShell file encoding gotcha confirmed**: when writing .ps1
  files via Claude's `Write` tool, stick to ASCII characters. PS 5.1
  reads files in system codepage (Windows-1252 on US-Windows), not
  UTF-8. Em-dashes (`—`), smart quotes, etc. parse as garbage. Plain
  hyphens + `--` work fine.
- **ACE-Step turbo model in pre-flight ignores cfg_scale**: log notes
  "Turbo model detected: overriding guidance_scale 7.0 -> 1.0 (turbo
  does not use CFG)." Don't waste time tuning guidance for the turbo
  variant. Base variant (`acestep-v15`) respects cfg, but it's larger
  and won't fit on kokonoe either.
- **`vram_used_gb: 8.0/8.0` in /health is a known PyTorch caching-
  allocator quirk** — `torch.cuda.mem_get_info()` returns the OS-level
  free memory, which reflects everything PyTorch's allocator has
  pre-reserved as committed-but-unused. nvidia-smi shows the same.
  Both are "true" in different senses; for actual usable headroom, the
  ACE-Step pre-flight check (`_vram_preflight_check`) is the
  authoritative number.
- **DiT-only mode is a usable fallback** on tight VRAM. The lyrics
  field still gets passed but conditioning is weaker (caption-only
  pathway). Section-level structure quality will suffer; full
  LM-conditioned mode is the target on cnc.
- **Ollama on :11434 was UP** during the session — qwen2.5:7b-instruct
  + 7 others registered. If we run Phase D on cnc later, qwen2.5 stays
  on kokonoe :11434 (its native home); ACE-Step lives on cnc :8083.
  Orchestrator on arch-controller dispatches both over Tailscale per
  the HANDOFF §3 fleet table.
- **VRAM was verified clean at session end** — no python sidecars
  running, no GPU compute processes from this session. Matt's
  baseline ~2.7 GB is Chrome/Discord/Ollama/system. Free to shut down
  or keep using the machine without restart.

---

## 24. Session 2026-05-18 — ACE-Step alive on cnc P100 (sm_60 wall solved)

### Outcome

🟢 **ACE-Step 1.5 turbo runs in full-LM mode on cnc-server's Tesla P100
16 GB (Pascal sm_60), 8.00× realtime on the GPU.** First end-to-end
generation outside kokonoe.

### Hardware reality (vs prior session memory)

Memory file said "3 × P100 16 GB = 48 GB total" — stale. Actual:

| Slot | Card | Bus | PCI ID | VRAM |
|---|---|---|---|---|
| GPU 0 | P100-PCIE-12GB | 01:00.0 | `10de:15f7` | 12 GB |
| GPU 1 | P100-PCIE-16GB | 02:00.0 | `10de:15f8` | 16 GB |
| GPU 2 | — | — | — | **waiting on a PCIe riser** |

Drivers: 580.126.09 / CUDA 13.0. Both cards idle pre-test. cnc is
openSUSE Leap Micro 6.2 — transactional/read-only root; host package
install via `transactional-update pkg install`, not `zypper`. `/opt` is
writable.

### The sm_60 wall + fix

ACE-Step's `pyproject.toml` pins `torch==2.10.0+cu128` on Linux. That
wheel has **no sm_60 binaries** — torch officially dropped Pascal from
cu128 builds around 2.7-2.8. Smoke-time symptom: VAE load died with
`CUDA error: no kernel image is available for execution on the device`
even though the handler init returned "OK" (ACE-Step swallows the
exception in its loader).

`torch.cuda.get_arch_list()` proved it: pinned wheel only has
`['sm_70','sm_75','sm_80','sm_86','sm_90','sm_100','sm_120']`. Torch
itself prints the warning loud: *"Tesla P100 with CUDA capability sm_60
is not compatible with the current PyTorch installation."*

**Fix:** override the torch pin to `torch==2.7.1+cu118` (the version
ACE-Step pins on its Windows path, plus cu118's wider arch list). One
command in the existing venv:

```
ssh cnc-server "cd /opt/acestep && \
  CUDA_VISIBLE_DEVICES=1 uv pip install --force-reinstall \
    --index-url https://download.pytorch.org/whl/cu118 \
    'torch==2.7.1+cu118' 'torchvision==0.22.1+cu118' \
    'torchaudio==2.7.1+cu118'"
```

Resulting `get_arch_list()` includes `sm_60` (plus `sm_37`, `sm_50`,
all the way to `sm_90`). ACE-Step source-level compat with 2.7.1 is
already validated by upstream's own Windows pin — no API breakage.
One non-fatal warning: `torchao: Skipping import of cpp extensions
due to incompatible torch version 2.7.1+cu118 for torchao version
0.16.0`. torchao falls back to pure-Python; doesn't affect inference.

Candle was ruled out — candle can build on Pascal via wiki patches
(`J:\llm-wiki\patterns\candle-p100-pascal-compat.md`), but candle has
no ACE-Step implementation; ACE-Step's DiT + 5Hz LM + VAE would need
weeks of porting work to land on candle. Not on the path.

### Benchmark — full-LM ACE-Step on the 16 GB P100

| Duration | Sidecar time | Wall (curl.exe client) | GPU realtime ratio | Pre-norm peak |
|---|---|---|---|---|
| 10 s | 3.7 s | ~4.0 s | 2.70× | 0.9141 |
| 200 s | **25.0 s** | **25.67 s** | **8.00×** | 1.0000 (clipped → normalized to 0.8913) |

Linear fit: `t_gpu ≈ 0.107 × duration_s + 2.6 s`. Tiled VAE
auto-engaged at 3.7 GB free (chunk_size=128, latents [1, 64, 5000]).
**Network transfer is noise** — 38 MB pulls in ~0.4 s over the LAN
(Tailscale direct, not DERP-relayed); the wall is ~99% GPU compute.

Projected per real song:
- 180 s track: ~21 s GPU + ~0.4 s transfer = **~22 s wall**
- 300 s track: ~35 s GPU + ~0.6 s transfer = **~36 s wall**
- 360 s track: ~41 s GPU + ~0.7 s transfer = **~42 s wall**

For comparison: Tron Vol. 1 ran ~14 min/track on MusicGen-on-kokonoe
(chained 30 s segments). ACE-Step single-shot on cnc P100 ≈ **~20×
faster** per track, plus license is MIT (no CC-BY-NC strike risk).

**Client-side gotcha:** the first 200 s render in this session
clocked 58.5 s wall. That was PowerShell `Invoke-WebRequest -OutFile`
buffering the full 38 MB response in memory before flushing to disk
(known PS 5.1 issue). Switching the client to `curl.exe` (built into
Windows 10) cut wall time to 25.67 s — virtually all GPU. For the
Rust client side (`AceStepClient` in `nightdrive-audio-gen`), reqwest
streams `Response::bytes_stream()` directly to a file with no buffering
overhead — already correct. Only the ad-hoc PS probes were affected.

### Install layout on cnc

Mirrors the kokonoe `J:\acestep\` layout at `/opt/acestep/`:

| Path | What |
|---|---|
| `/opt/acestep/` | Cloned `ace-step/ACE-Step-1.5` |
| `/opt/acestep/.venv/bin/python` | uv-managed venv (Python 3.12.12) |
| `/opt/acestep/.venv/.../torch` | **2.7.1+cu118** (overridden from upstream 2.10.0+cu128) |
| `/opt/acestep/checkpoints/{acestep-v15-turbo, acestep-5Hz-lm-1.7B, Qwen3-Embedding-0.6B, vae}/` | ~10 GB weights |
| `/opt/nightdrive/sidecar/acestep_server.py` | nightdrive sidecar code, scp'd from `J:\nightdrive\sidecar\` |
| `/var/log/nightdrive/sidecar.log` | runtime log |

Helper artifacts staged in repo (not yet installed on cnc as systemd):

- `scripts/install_acestep.sh` — Linux port of the PS1 installer
  (idempotent, sets `UV_HTTP_TIMEOUT=300` to avoid the default-30s
  fonttools timeout that hit on first attempt)
- `scripts/nightdrive-acestep.service` — systemd unit, `Type=simple`,
  pins `CUDA_VISIBLE_DEVICES=1`, restarts on failure
- `config/nightdrive-acestep-cnc.toml` — orchestrator config variant
  with `[audio_gen].base_url = http://cnc-server.tailb85819.ts.net:8083`

### Sidecar boot (current, manual)

```
ssh cnc-server "cd /opt/nightdrive && \
  CUDA_VISIBLE_DEVICES=1 \
  NIGHTDRIVE_ACESTEP_ROOT=/opt/acestep \
  NIGHTDRIVE_ACESTEP_CONFIG=acestep-v15-turbo \
  ACESTEP_LM_BACKEND=pt \
  nohup /opt/acestep/.venv/bin/python -m uvicorn \
    sidecar.acestep_server:app --host 0.0.0.0 --port 8083 --workers 1 \
    > /var/log/nightdrive/sidecar.log 2>&1 &"
```

`/health` output:
```
{
  "ok": true, "model": "acestep-v15-turbo",
  "lm_model": "acestep-5Hz-lm-0.6B", "lm_backend": "pt",
  "device": "cuda:0", "sample_rate": 48000, "channels": 2,
  "supports_structured_lyrics": true,
  "vram_used_gb": 12.07, "vram_total_gb": 15.89
}
```

### Split-GPU VAE follow-up (same session, ~1 h later)

After the single-card baseline was validated, explored pipeline-parallel VAE
placement across the 12 GB + 16 GB P100 pair to see how much the N4.11
roadmap item is worth in practice. Outcome: **~20 % wall-time win on a
360 s render, plus a constant ~3.5 GB headroom unlock on the DiT card.**
The N4.11 placeholder is partly redeemed by this work — full tensor-
parallel sharding is still a future item, but the VAE-on-different-GPU
piece is now production.

**The patch stack** (three changes, all in this repo):

1. `scripts/patches/acestep-vae-device-aware-decode.patch` —
   one-line `.to(self.vae.dtype)` → `.to(device=<vae_device>, dtype=<...>)`
   in ACE-Step's `generate_music_decode.py`. Idempotent on single-card
   (cast is a no-op when VAE shares the latent's device). Apply once on
   any cnc redeploy.
2. `sidecar/acestep_server.py` — reads `NIGHTDRIVE_ACESTEP_VAE_DEVICE`
   env. After `dit_handler.initialize_service`, moves
   `dit_handler.vae` to that device + logs per-device VRAM. Unset =
   legacy single-device path.
3. `scripts/nightdrive-acestep.service` — split-GPU env is the default:
   `CUDA_VISIBLE_DEVICES=1,0`, `NIGHTDRIVE_ACESTEP_DEVICE=cuda:0`,
   `NIGHTDRIVE_ACESTEP_VAE_DEVICE=cuda:1`,
   `ACESTEP_VAE_DECODE_CHUNK_SIZE=1024`.

**Chunk-size A/B grid (360 s render, seed=137, full LM, 8 turbo steps):**

| Config | Wall | Server | VAE decode | RT | # chunks |
|---|---|---|---|---|---|
| Single-card (chunk=128 auto) | 52.5 s | 51.7 s | ~24 s | 6.96× | 70 |
| Split (chunk=128) | 54.7 s | 54.0 s | ~23 s | 6.67× | 70 |
| Split (chunk=512) | 45.3 s | 44.5 s | 16.2 s | 8.09× | 18 |
| **Split (chunk=1024) [prod]** | **42.8 s** | **42.0 s** | **13.8 s** | **8.57×** | **9** |
| Split (chunk=2048) | 42.1 s | 41.4 s | 12.9 s | 8.69× | 5 |

The auto-tuner picks `chunk_size=128` based on `self.device`'s free
VRAM (~4 GB on the DiT card) — wrong card. Manually setting
`ACESTEP_VAE_DECODE_CHUNK_SIZE=1024` lets the VAE on its dedicated
12 GB card use chunks 8× bigger, amortizing per-chunk overhead.
`2048` plateaus the win (~0.7 s further) but eats more activation
buffer — `1024` is the robust production setting.

**Things that didn't work, with why:**

- **Single-chunk (`use_tiled_decode=False`)** — OOM at 360 s. The VAE's
  `conv_t1` ConvTranspose1d needs an 8.24 GiB activation buffer for the
  full 9000-latent input. Even on a 12 GB card with 11 GB free at
  decode start, the upsampling stage doesn't fit single-pass.
- **`use_tiled_decode=False` via direct kwarg to
  `acestep.inference.generate_music`** — the top-level function
  doesn't take it; the kwarg lives on the handler-method one layer
  deeper. Worked around with a `functools.wraps`'d monkey-patch on
  `dit_handler.generate_music`, then reverted once we confirmed the
  bigger-chunk path was the actual win.
- **Calling the threshold helper with the VAE's device** would let
  `_get_auto_decode_chunk_size` auto-pick the right tier, but the helper
  is a method on the handler that queries `self.device` directly.
  Patching it would mean a second source edit; the env-var override
  (which ACE-Step already supports) was cleaner.

### What's next (in order)

1. **Land the systemd unit on cnc** — `transactional-update` not
   needed for the unit file (it goes in `/etc/systemd/system/` which
   is on the writable subvolume on Leap Micro). `daemon-reload` +
   `enable --now`. Sidecar auto-restarts on failure, survives reboot.
2. **A/B listen** — Matt evaluates the 200 s smoke
   (`scratch/cnc-smoke-200s.wav`) vs a prior MusicGen-rendered track.
   If quality is acceptable, flip the engine.
3. **Promote `config/nightdrive-acestep-cnc.toml` → `config/nightdrive.toml`**
   only after the A/B verdict.
4. **Phase D witness re-run** — Phase C had skipped on kokonoe (env
   var not set in audit env). With cnc up, re-run
   `cargo test --test ace_step_real_sidecar`, point
   `NIGHTDRIVE_ACESTEP_URL=http://cnc-server.tailb85819.ts.net:8083`,
   confirm it passes end-to-end.
5. **Phase E full pipeline** — `nightdrive-orchestrator run-batch
   --count 1 --dry-run` with the cnc config, confirm `pipeline_one`
   no longer warns on Stage 2 audio_gen.
6. **N4.11 (deferred)** — pipeline-parallel ACE-Step across the
   12+16 GB pair, only if XL variant or larger renders become
   interesting. Today's turbo workload fits the 16 GB card.

### Notes for next session

- **3rd P100 still pending a PCIe riser.** When it lands, re-run the
  fleet table in `cnc-p100-arrival` memory and decide whether to
  fanout (parallel renders per card) or pool (N4.11).
- **`torch==2.7.1+cu118` is the magic pin** — do NOT let any later
  `uv sync` or `pip install -U` revert it to ACE-Step's 2.10 default.
  If we ever build a Dockerfile or a fresh provisioner for the
  sidecar, the torch override has to be the LAST install step.
- **Pre-stage from fleet before upstream** (lesson burned in today,
  see `feedback_prestage_from_fleet_before_upstream` memory). The
  ~10 GB ACE-Step checkpoints already existed on kokonoe; I should
  have rsync'd them over Tailscale during the driver-install hold
  instead of letting cnc re-pull from HuggingFace.
- **Default 30s `UV_HTTP_TIMEOUT` will burn you** on slow HF/PyPI
  bursts when several large wheels race concurrently. Set
  `UV_HTTP_TIMEOUT=300` for any uv sync on cnc.
- **bash + lean-ctx wrapper conflict observed** — `curl ... | python
  -m json.tool` came back with `C:UsersMatt.cargobinlean-ctx.exe:
  command not found` (the wrapper stripped path slashes). PowerShell
  pipeline worked clean. Use PowerShell for HTTP probes from kokonoe
  side; bash on cnc-side is fine.

---

## 25. Session 2026-05-19 — Split-GPU VAE + Vol. 2 kickoff (Akira-coded)

### Outcome (status: 🟡 in progress — album pipeline staged, composer rate-limited mid-run)

ACE-Step on cnc P100 graduated from "smoke passes on one card" to a
tuned split-GPU production config (~20% wall-time win on a 360 s render),
plus the SDXL cache moved from kokonoe to cnc over LAN so the next
album's covers can render without touching the kokonoe GPU (which is
locked: matt-voice is training there). Started Vol. 2 album work
(Akira-coded Neo-Tokyo, sync-drop ~2026-05-20 01:30 UTC) but the
album-composer subagent hit a transient Anthropic rate-limit before
emitting the JSON. Resume by either re-dispatching album-composer or
using SendMessage on agentId `a42880847a9a3dc2b`.

### What got done

1. **Split-GPU VAE for ACE-Step on the P100 pair** (full A/B grid in §24,
   in-place edits above). Production env baked into
   `scripts/nightdrive-acestep.service`:
   `CUDA_VISIBLE_DEVICES=1,0`, `NIGHTDRIVE_ACESTEP_DEVICE=cuda:0`,
   `NIGHTDRIVE_ACESTEP_VAE_DEVICE=cuda:1`,
   `ACESTEP_VAE_DECODE_CHUNK_SIZE=1024`. The recommended chunk_size is
   1024 (2048 plateaus the win but eats more activation budget).
2. **Upstream patch saved at**
   `scripts/patches/acestep-vae-device-aware-decode.patch` — one-line
   change to ACE-Step's `generate_music_decode.py` routing latents to
   the VAE's device. Idempotent on single-card. Apply on any cnc
   redeploy of ACE-Step.
3. **Tailscale path confirmed direct-LAN** (`direct 192.168.168.100:...`,
   not DERP relay). No reason to bypass — sub-ms RTT, gigabit
   throughput for the WAV download phase.
4. **PowerShell IWR speed trap exposed**: `Invoke-WebRequest -OutFile`
   buffers the full response in PS 5.1 memory. The first 200 s
   render's 58.5 s wall was ~33 s of buffering. Switched all HTTP
   probes to `curl.exe` (built into Win 10). See
   `feedback_powershell_iwr_buffers_large_downloads` memory.
5. **SDXL cache prestaged on cnc** at
   `/root/.cache/huggingface/hub/models--stabilityai--stable-diffusion-xl-base-1.0`
   — 6.62 GB scp'd from kokonoe in 123.7 s over Tailscale's direct-LAN
   path (54.8 MB/s effective). NOT a HF re-pull. Followed the
   `prestage-from-fleet-before-upstream` rule Matt reinforced this
   session.
6. **N4.11 roadmap placeholder** added for the pipeline-parallel VAE
   item (partly redeemed by this session's work; full tensor-parallel
   sharding still a future item).
7. **Memory updates**:
   - new `project_split_gpu_vae_acestep.md`
   - new `feedback_powershell_iwr_buffers_large_downloads.md`
   - new `feedback_prestage_from_fleet_before_upstream.md`
   - new `project_p100_torch_sm60_blocked.md` (then status-updated to
     SOLVED once torch override worked)
   - updated `cnc-p100-arrival.md` to flip the misleading "pin to
     GPU 1" instruction (both cards now hold ACE-Step in prod)
   - updated `MEMORY.md` index

### Current state of the pipeline

| Stage | Status | Notes |
|---|---|---|
| Audio gen | 🟢 Production | ACE-Step on cnc, split-GPU, ~42 s wall per 360 s track |
| Mastering | 🟢 Working | ffmpeg loudnorm two-pass on orchestrator host |
| Covers | 🟡 Staged | SDXL weights on cnc; no sidecar.py yet; existing `sidecar/generate_album_covers_native.py` could run against the cache via the ACE-Step venv (needs diffusers verify) |
| Visualizer | 🟢 Working | Album mode uses ffmpeg `showwaves` overlay baked into stage 6 (CPU, no GPU) per `pipeline_one_album` |
| Final encode | 🟢 Working | ffmpeg libx264 + AAC |
| Upload | 🟢 Working | Single-shot YT Data API v3; chunked-resume still TODO but not blocking |

Audit (last run this session): build:0 test:0 stubs:2 (livestream TODOs
in main.rs, not album-mode blockers) witnesses:11 schema:clean
**bench:STALE 8 d** — only failure, non-blocking for shipping. The
bench-runner agent should be invoked at the start of the next session
to refresh the ledger now that perf-relevant code changed (ACE-Step
on cnc, split-GPU VAE).

### Blocking issues

- **Album-composer subagent rate-limited mid-dispatch.** Resume via
  `SendMessage to: a42880847a9a3dc2b` or just re-dispatch the
  album-composer with the same brief (theme: Akira-coded Neo-Tokyo,
  12 tracks, BPM 104-120, sync-drop, 3-aspect cover prompts, ACE-Step
  prompt format). Brief is in the prior turn of this session's
  transcript.
- **kokonoe GPU OFF LIMITS** until matt-voice finishes training. Affects:
  any visualizer wgpu work, any SDXL-on-kokonoe path, any concurrent
  cover gen on kokonoe. Workaround: covers go to cnc (SDXL cache ready);
  album-mode visualizer is showwaves (CPU); wgpu visualizer waits.

### What's next (in order, for resume)

1. **Re-dispatch the album-composer subagent** with the Akira brief.
   Expected output: `docs/albums/neo-tokyo-drive-vol-1.json` (matching
   the Tron Vol. 1 schema exactly: 12 tracks, recurring motifs, full
   per-track sections + musicgen_prompt + cover_prompt +
   composer_notes).
2. **Decide cover-gen path on cnc**: either (a) call the existing
   `sidecar/generate_album_covers_native.py` directly via the ACE-Step
   venv (likely works since ACE-Step bundles diffusers), or (b) write
   a proper long-running `sidecar/sdxl_server.py` mirroring
   `acestep_server.py` for repeat use. (a) is faster for one album;
   (b) is the right architecture. Recommend (a) for Vol. 2, do (b)
   alongside Vol. 3.
3. **Pre-render 36 covers** (12 tracks × 3 aspects: 1024², 1344×768,
   768×1344) into `assets/covers/albums/neo-tokyo-drive-vol-1/`.
   Path convention is set by `generate_album_covers_native.py`.
4. **Verify the sidecar is up with prod config** — currently running
   with `ACESTEP_VAE_DECODE_CHUNK_SIZE=2048` from the A/B test. Either
   restart it with the systemd unit (which now defaults to 1024) or
   confirm 2048 is what we want shipped.
5. **Run the album**:
   ```
   $env:NIGHTDRIVE_CONFIG = "config/nightdrive-acestep-cnc.toml"
   .\target\release\nightdrive-orchestrator.exe run-album `
       --slug neo-tokyo-drive-vol-1 `
       --publish-at 2026-05-20T01:30:00Z
   ```
   Estimated wall: 12 × (~42 s audio + ~30 s master + ~20 s encode +
   ~20 s upload) ≈ **~22-25 min** for the album, plus the cover
   pre-step (~15-30 min depending on which SDXL path).

### Notes for next session

- **Sidecar state on cnc**: running PID 371531 with chunk_size=2048
  (from the A/B test, not the prod chunk_size=1024). Same VAE timing
  in practice (~0.7 s difference); the 1024 default in the systemd
  unit is the prod recommendation but the running process is fine to
  ship with as-is. `systemctl daemon-reload && systemctl restart
  nightdrive-acestep` will roll it to the canonical config if/when the
  unit lands on cnc.
- **The systemd unit `scripts/nightdrive-acestep.service` is staged
  but not installed on cnc.** Install with:
  `sudo install -m 0644 scripts/nightdrive-acestep.service
  /etc/systemd/system/ && sudo systemctl daemon-reload &&
  sudo systemctl enable --now nightdrive-acestep.service`.
  (Reminder: Leap Micro `/etc/systemd/system/` is on the writable
  subvolume — no `transactional-update` needed.)
- **bash via the Bash tool is mangling paths via lean-ctx wrapper**
  for some operations (e.g. `git status` came back with
  `C:UsersMatt.cargobinlean-ctx.exe: command not found`). PowerShell
  works clean. Default to PowerShell for any client-side ops on
  kokonoe; ssh-into-cnc bash is fine.
- **The auto-uploader (`github-uploader-buildout`) auto-commits the
  working tree.** Don't manually `git add/commit` — the tool handles
  it. Each "Initial commit - uploaded via github-uploader-buildout"
  in the log is a buildout snapshot.
- **Album-composer agentId from this session**: `a42880847a9a3dc2b`.
  If still resumable next session, use `SendMessage` instead of a
  fresh `Agent` dispatch (preserves the brief context).

---

## 26. Session 2026-05-19 (continued) — Vol. 2 ship pass: 10/12 uploaded, 11+12 cron-deferred

### Outcome (status: 🟡 awaiting Pacific quota reset for 11/12 retry)

Neo-Tokyo Drive, Vol. 1 went private end-to-end on the NightDrive YouTube
channel. Sync-drop anchor was pushed from 2026-05-20T01:30Z → **2026-05-20T12:00:00Z**
because YouTube's per-channel daily upload cap clamped on tracks 11+12.

### What got done

1. **Album spec composed** by the album-composer subagent →
   `docs/albums/neo-tokyo-drive-vol-1.json` (12 tracks, BPM 104-120,
   home tonic D minor, FM bell + analog brass palette, vertical-descent
   narrative arc through Neo-Tokyo).
2. **Covers rendered on cnc** (NOT kokonoe — matt-voice was training
   on that GPU). SDXL cache pre-staged from kokonoe via `scp` over the
   LAN (6.62 GB / 124 s / ≈55 MB/s). 36 PNGs (12 × {1024², 1344×768,
   768×1344}) at `assets/covers/albums/neo-tokyo-drive-vol-1/`. Wall
   ~23 min on the 16 GB P100 (no `--low-vram` needed).
3. **Pipeline ran end-to-end audio→master→encode** for all 12 tracks
   (35.8 min wall). Stage 7 upload failed all 12 with `invalid_grant`
   — refresh token expired.
4. **OAuth re-bootstrapped via Chrome MCP** for
   `mmichels88@gmail.com`. Trap: bootstrap.rs timeout was 5 min but
   navigating Google's multi-step consent took longer than that on the
   first try; bumped timeout to 30 min in source, rebuilt, retried.
   See [[powershell-iwr-buffers-large-downloads]] companion lesson
   (similar — assume client-side timing is the bottleneck, not the
   API).
5. **Patched `pipeline_one_album` to skip-on-state** via file-existence
   checks: `raw_audio_wav` / `master_flac` / `final_mp4` presence
   skips stages 2 / 4 / 6 respectively. Survives DB state drift /
   Failed-marker overwrites.
6. **Patched `Tracks::insert` to `INSERT OR IGNORE`** so re-runs don't
   blow up on the `tracks.id` UNIQUE constraint when the row already
   exists from a prior partial run.
7. **Re-ran orchestrator**: 10/12 tracks uploaded clean in 257.1 s wall
   (~25 s/track upload + thumbnail). Tracks 11 + 12 failed with
   `uploadLimitExceeded` (`domain: youtube.video`) — YouTube's
   per-channel daily upload cap, not API quota.
8. **All 10 already-uploaded videos re-anchored** via `videos.update`
   (PUT /youtube/v3/videos?part=status) from
   `publishAt=2026-05-20T01:30Z` → `2026-05-20T12:00:00Z`. Privacy
   stays Private until the new anchor.
9. **Cron `455a6596` scheduled** one-shot at `27 0 20 5 *` local
   (= 2026-05-20T07:27Z, 27 min after Pacific midnight quota reset)
   to re-fire the orchestrator with `--from-track 11 --publish-at
   2026-05-20T12:00:00Z`. Harness reports session-only despite
   `durable: true`.
10. **Telegram heads-up** sent to Matt with the 10 video_ids + manual
    fallback command in case the session dies before 07:27Z fires.

### Final video_id list (Neo-Tokyo Drive, Vol. 1)

| # | Title | YT Video ID | publishAt |
|---|---|---|---|
| 01 | Ignition Deck | `YLmBMrYm6Hk` | 2026-05-20T12:00Z |
| 02 | Onramp Above the City | `ZwSdlwaE47s` | 2026-05-20T12:00Z |
| 03 | Vertical Signage | `ZilNGntSXGg` | 2026-05-20T12:00Z |
| 04 | Cut-In | `0WsM78t7kqw` | 2026-05-20T12:00Z |
| 05 | Arcade Strobe Wall | `EokwjZGFjMk` | 2026-05-20T12:00Z |
| 06 | Night Market Run | `Ca6ZzmTVtRw` | 2026-05-20T12:00Z |
| 07 | Under the Overpass | `f9JuXeRRmKs` | 2026-05-20T12:00Z |
| 08 | Service Ramp Down | `1yLQY3VwGJc` | 2026-05-20T12:00Z |
| 09 | Flooded Maintenance Line | `b2_v_1G6Zxg` | 2026-05-20T12:00Z |
| 10 | Reactor Hall | `XtxiLuX6DTo` | 2026-05-20T12:00Z |
| 11 | Freight Elevator | (pending 07:27Z retry) | — |
| 12 | Ground Floor, Pre-Dawn | (pending 07:27Z retry) | — |

### Blocking issues

- **2 of 12 tracks still need upload** — cron `455a6596` scheduled to
  retry at 07:27Z. Fallback: manual `nightdrive-orchestrator run-album
  --slug neo-tokyo-drive-vol-1 --from-track 11 --publish-at
  2026-05-20T12:00:00Z` if session dies first.
- **YouTube channel daily upload cap is the binding constraint** on
  album-mode batching. NightDrive channel hit it at ~10/day with all
  10 uploads in a ~4-min window. Future albums of >10 tracks need to
  span 2+ Pacific calendar days OR get the channel into a higher
  verification tier.

### Notes for next session

- **The auto-uploader (`github-uploader-buildout`) auto-commits.** Don't
  manually `git add/commit/push`. See `reference_github_uploader_auto_commits`.
- **Refresh token in `.env` is fresh** as of 2026-05-19. Backup at
  `.env.bak.20260519`. The new token is for `mmichels88@gmail.com` —
  confirmed by Matt during the Chrome MCP flow.
- **The `scratch/` dir on kokonoe** has the 4 smoke WAV files
  (10s, 200s, 360s single-GPU, 360s split-GPU chunk=512) plus the
  yt-auth.log + .err files. Safe to clean up; nothing depends on them.
- **All 36 covers** are at `J:\nightdrive\assets\covers\albums\neo-tokyo-drive-vol-1\`
  (the orchestrator-host copies) AND
  `cnc:/opt/nightdrive/assets/covers/albums/neo-tokyo-drive-vol-1/`
  (cnc copies, original render location). Either works as source-of-truth.
- **Build numbers updated** (timeout 5→30 min on bootstrap.rs +
  skip-on-state in pipeline_one_album + INSERT OR IGNORE in Tracks::insert).
  Three discrete edits, one rebuild each — all clean.

---

## 27. Session 2026-05-20 — Vol. 2 sync-drop pushed AGAIN + Vol. 3 in flight

### Outcome (status: 🟡 holding for 16:03Z cap-clear retry; Vol. 3 audio pending)

Two threads of work running in parallel today:

1. **Vol. 2 (Neo-Tokyo) upload retry hit the SAME `uploadLimitExceeded`
   at 07:27 UTC.** Diagnosis revised: the YouTube channel daily cap is
   a **rolling 24h window from first cap-hit**, not a Pacific-midnight
   calendar reset. First hit was 2026-05-19T15:36Z, so the window
   clears ~2026-05-20T15:36Z.
2. **Vol. 3 (Atompunk Cold War) cover gen kicked off on cnc** while
   waiting on Vol. 2. Same SDXL pattern as Vol. 2 — 36 PNGs (12 × 3
   aspects), ACE-Step sidecar killed first to free the 16 GB card.

### Vol. 2 — third anchor push

- **Sync-drop anchor**: 01:30Z → 12:00Z → **2026-05-21T00:00:00Z**.
- All 10 already-uploaded videos re-anchored via
  `videos.update?part=status` for the third time (~1 s wall for the
  whole batch).
- **Cron `f8816c1d`** scheduled at `3 9 20 5 *` (09:03 PDT today =
  16:03 UTC, 27 min after rolling-24h window clears). Will fire
  the orchestrator with `--from-track 11 --publish-at 2026-05-21T00:00:00Z`.
- Matt picked the +24h conservative anchor over a tighter 17:00Z
  retry because the cap reset model is opaque — no API to query when
  it actually clears, so giving 8h+ buffer between retry attempt and
  sync-drop avoids a possible 4th push.

### Vol. 3 — composer + cover render

- **Theme**: Atompunk Cold War (1958-1968). Tang-orange + steel-grey
  + atomic-teal palette, Theremin + muted brass + vibraphone +
  upright bass + brushed drums. BPM 84-98 (slowest album yet). Home
  tonic **C minor** — new harmonic neighborhood vs the A/D minor
  pattern.
- **Album JSON**: `docs/albums/atompunk-drive-vol-1.json` — 12 tracks,
  24-hour cycle narrative (dawn drill siren → bunker midday → near-
  launch crisis → night sign-off), cycle-of-fifths-ascending ladder
  Cm→G#m for morning/bunker arc, cycle-of-fifths-descending Ebm→Cm
  for night descent. Drill siren motif bookends the album as Theremin
  lullaby at half-tempo.
- **Tracks**: 1. Drill Siren, 0600 / 2. Foil Curtain Morning /
  3. Salt Flats Commute / 4. Stations, Console Six /
  5. Telemetry, Range Window 2 / 6. Wall Clock, 1217 /
  7. Contact on the Doppler / 8. Twenty-Second Holds /
  9. All Stand Down / 10. Salt Flats After Sundown /
  11. Sign-Off, Test Pattern Hum / 12. Porch Light, Midnight.
- **Cover gen**: in flight on cnc as of 2026-05-20T07:24Z, ~23 min
  wall expected for 36 PNGs.
- **3 future-album themes banked** from the same picker:
  VHS Bootleg Horror, Hong Kong Rooftop Noir, Arctic Ice Station.
  See `project_future_album_theme_bank` memory.

### Vol. 2 retry RESOLVED — 12/12 uploaded

Cron `f8816c1d` fired at 16:03Z (~27 min after the rolling-24h cap
cleared ~15:36Z). Both tracks uploaded in **54.3 s wall**:

- Track 11 **Freight Elevator** → `mtEra-1Fdok`
- Track 12 **Ground Floor, Pre-Dawn** → `7XptVg8BjVc`

**All 12 of Vol. 2 are now uploaded private + anchored to
2026-05-21T00:00:00Z.** Sync-drop will fire ~7.5 h from this writing.

### Final video_id list — Neo-Tokyo Drive, Vol. 1

| # | Title | YT Video ID | publishAt |
|---|---|---|---|
| 01 | Ignition Deck | `YLmBMrYm6Hk` | 2026-05-21T00:00Z |
| 02 | Onramp Above the City | `ZwSdlwaE47s` | 2026-05-21T00:00Z |
| 03 | Vertical Signage | `ZilNGntSXGg` | 2026-05-21T00:00Z |
| 04 | Cut-In | `0WsM78t7kqw` | 2026-05-21T00:00Z |
| 05 | Arcade Strobe Wall | `EokwjZGFjMk` | 2026-05-21T00:00Z |
| 06 | Night Market Run | `Ca6ZzmTVtRw` | 2026-05-21T00:00Z |
| 07 | Under the Overpass | `f9JuXeRRmKs` | 2026-05-21T00:00Z |
| 08 | Service Ramp Down | `1yLQY3VwGJc` | 2026-05-21T00:00Z |
| 09 | Flooded Maintenance Line | `b2_v_1G6Zxg` | 2026-05-21T00:00Z |
| 10 | Reactor Hall | `XtxiLuX6DTo` | 2026-05-21T00:00Z |
| 11 | Freight Elevator | `mtEra-1Fdok` | 2026-05-21T00:00Z |
| 12 | Ground Floor, Pre-Dawn | `7XptVg8BjVc` | 2026-05-21T00:00Z |

### What's next (in order)

1. **Wait on Vol. 3 cover gen** to finish (still running in the
   background as of 16:04Z). Pull track 1's 3 aspects back to kokonoe,
   send to Matt to confirm the atompunk aesthetic landed before booting
   ACE-Step for the audio pass.
2. **Boot ACE-Step sidecar** on cnc (prod split-GPU config) once
   covers are done.
3. **Run orchestrator `run-album --slug atompunk-drive-vol-1
   --dry-run`** — stops before stage 7 upload, leaves 12 final.mp4s
   on disk. Audio + master + encode only.
4. **Plan Vol. 3 upload**: cannot upload Vol. 3 today (Vol. 2 ate
   today's cap window — 12 tracks in ~24 h). Earliest Vol. 3 first
   upload is 2026-05-21T16:04Z + 24 h ≈ 2026-05-22T16:04Z (rolling
   window from Vol. 2's LAST upload). Could schedule Vol. 3
   sync-drop for Sun 2026-05-24T00:00Z or later, with cron-based
   upload starting ~2026-05-22T18:00Z.

### Notes for next session

- **The YT channel daily upload cap is rolling 24h, NOT calendar-day.**
  Burned this lesson [[yt-channel-daily-upload-cap]] — first version
  of that memory said "resets at Pacific midnight" which was wrong.
  The memory has been corrected.
- **Vol. 3 upload should wait at least 24h after Vol. 2's last upload
  succeeds.** With Vol. 2 retry hopefully landing at ~16:03Z today,
  Vol. 3's first upload should be no earlier than 2026-05-21T16:03Z
  to avoid stacking caps.
- **Sidecar state on cnc**: dead, will be re-booted after covers finish.
- **All Vol. 3 audio/master/encode artifacts will land at
  `var/nightdrive/tracks/nd-atompunk-drive-vol-1-NNN/`** when the
  orchestrator runs.

---

**Single-source-of-truth:** this file. Update it when decisions change.
