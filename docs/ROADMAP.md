# nightdrive Roadmap — to a 1-VOD/day + 24/7 livestream pipeline that pays rent

**Provenance:** written 2026-05-10 alongside the discipline-stack
agent spec (`docs/superpowers/specs/2026-05-10-discipline-stack-design.md`).
The 12-item build order in `HANDOFF.md` §9 and the revenue timeline in
§10 are the prose source; this file is the structured, witness-anchored
form `roadmap-tracker` reads.

> **Goal:** one cron tick → one published synthwave VOD on YouTube,
> daily, unattended; in parallel a 24/7 RTMP livestream rotating a
> 200-track catalog. Compounding watch-hours rent. Every external
> product (audio-gen model, art-gen model, mastering chain, visualizer,
> uploader) swappable behind a trait. Zero `println!`. Zero
> `warn!("not yet implemented")` in any shipped code path.

## Effort scale

- **S** = 1 session (≤1 day of focused push)
- **M** = several sessions (≤1 week)
- **L** = multi-week (1–4 weeks)
- **XL** = month-plus arc

History across Matt's projects (Polymarket session, Pixie Dust v7.7, the
Aether self-host bootstrap): "months" arcs ship in days when LLM-aided.
Treat S/M/L/XL as upper bounds with honest median 3-5× faster.

## Cross-cutting rules

1. **Audit first.** Every shipped item ships with a
   `tests/witnesses/<descriptive>.rs` that hits the real endpoint
   (Ollama, Stable Audio Open, ffmpeg, YouTube, OBS) and exits clean.
   Tag the test header with `// stage: N` matching `HANDOFF.md` §5.
   `scripts/audit.ps1` count must monotonically grow.
2. **Honesty scan green.** No new `todo!()` / `unimplemented!()` /
   `warn!("not yet implemented")` in shipped code paths. If a stage
   isn't real yet, the orchestrator returns `Err(NotImplementedYet)`,
   not a silent `warn!` placeholder.
3. **Bench every perf-relevant item.** Audio-gen wall-clock per
   segment, mastering loudnorm pass time, final-encode bytes/sec,
   YouTube upload bytes/sec. Each gets a row in `docs/BENCH_LEDGER.md`.
   Hardware change → reset baseline (kokonoe pre-P100 vs cnc post-P100
   are different rows).
4. **Stable Audio Open is primary, MusicGen secondary.** Corrects
   `HANDOFF.md` §6's "MusicGen primary" claim — the
   `J:\pledgeandcrowns\tools\synthwave-gen\` pipeline already validates
   Stable Audio Open end-to-end. MusicGen stays as a future
   `AudioGenerator` trait impl.
5. **Reuse over rewrite.** Before opening any new file under
   `crates/<name>/src/`, search
   `J:\llm-wiki\projects\*\Reinvented Wheels.md` for prior art. Ack
   the precedent (or its absence) in the commit message.
6. **No GitHub Actions.** `scripts/audit.ps1` is the local gate.
   Build + test green before merge. Per global rule.
7. **Verbose tracing everywhere.** `tracing` + structured fields, no
   `println!`, every external call wrapped in `#[instrument]` with
   entry + exit + error spans. Per
   `J:\llm-wiki\patterns\verbose-logging-everywhere.md`.
8. **Trust the inbound hardware.** cnc P100s land ~2026-05-17. Until
   then, single-card bring-up on kokonoe. After: cnc is the inference
   muscle, kokonoe is wgpu-only, arch-controller is OBS RTMP origin.
   See `CLAUDE.md` "DO NOT REINVENT" §2.

---

# Phase N1 — MVP pipeline (cron tick → first private VOD)

The §9 critical path. End state: one full pipeline run produces a
private YouTube upload with a generated audio track + cover art +
placeholder waveform visualizer + correct metadata. Everything below is
either a Rust crate landing or a deploy gate.

## N1.1 Workspace reshuffle (S)

- Move root-staged files into the `crates/<name>/src/` layout per
  `CLAUDE.md` § "Status: SCAFFOLD".
- Delete `mnt/user-data/outputs/` sandbox leftover (already promoted
  in the table; safe to remove).
- `cargo check --workspace` returns 0 errors.
- **Done criterion:** `scripts/audit.ps1` section [1/7] reports build:0.

## N1.2 nightdrive-core ships (S, depends N1.1)

- `TrackId`, `CompositionSpec`, `TrackState`, `TrackPaths`,
  `NightdriveError`, `NightdriveResult` all compile.
- `config::AppConfig::load()` resolves the toml + env override path.
- `observability::init()` wires tracing-subscriber with JSON output to
  journald.
- **Witness:** `tests/witnesses/core_loads_real_config.rs` —
  `// stage: 0`. Loads `config/nightdrive.toml.example` from disk,
  asserts every section parses to its typed struct.

## N1.3 nightdrive-storage ships (S, depends N1.2)

- `sqlx::migrate!()` runs `20260510000000_init.sql`.
- CRUD: `Tracks::insert`, `Tracks::update_state`, `Tracks::list`,
  `Uploads::insert`, `Uploads::set_youtube_id`,
  `LivestreamRotation::next_track`.
- HANDOFF.md §7 schema drift fixed against the migration in this same
  PR (rename / align columns; pick migration as authoritative).
- **Witness:** `tests/witnesses/storage_roundtrip.rs` — `// stage: 0`.
  Opens an in-tempdir SQLite, migrates, inserts a track, transitions
  states `pending → spec_generated → audio_rendered → … → published`,
  reads back identical.

## N1.4 nightdrive-llm ships (M, depends N1.2)

- `OpenclawLlm::generate_spec()` works against a real Ollama on
  kokonoe at `kokonoe.tailb85819.ts.net:11434`, model
  `qwen2.5:7b-instruct`.
- `validate_spec` enforces BPM 80-118, duration 180-360, non-empty
  tags/title/sections (already in the staged `lib.rs` — keep it).
- Retry on JSON parse failure with the same prompt up to 2× before
  bubbling.
- **Witness:** `tests/witnesses/llm_real_ollama.rs` — `// stage: 1`.
  Hits real Ollama; asserts a parseable `CompositionSpec` is returned;
  marks `#[ignore]` if `OLLAMA_URL` env not set so CI-less local runs
  can opt in.

## N1.5 stable-audio sidecar deploys to cnc (M, depends cnc-P100s)

- Port `J:\pledgeandcrowns\tools\synthwave-gen\generate.py` into a
  FastAPI service (mirroring the staged `musicgen-server.py` shape):
  `POST /generate { prompt, duration_seconds, seed?, prev_audio_b64? } → audio/wav`,
  `GET /health → { ok, model, device, sample_rate }`.
- Reuse the manifest's negative_prompt + 128-token preflight verbatim.
- systemd unit on cnc: `stable-audio-server.service`, single worker,
  binds `0.0.0.0:8080`.
- Pascal sm_60 → drop fp16 override → fp32 inference (~60-90s/track).
- Multi-card via `synthwave-gen/fanout.py` pattern: not required for
  N1, but the API contract MUST allow per-request device pinning so
  fanout can sit in front later.
- **Witness:** `tests/witnesses/audio_sidecar_health.rs` — `// stage: 2`.
  Hits `/health`, asserts model name + sample_rate=44100.

## N1.6 nightdrive-audio-gen ships (M, depends N1.5)

- Trait `AudioGenerator { async fn render(&self, spec: &CompositionSpec, out: &Path) -> Result<()> }`.
- `StableAudioClient` impl: chains N×30s segments per the spec's
  section breakdown, passes `prev_audio_b64` as continuation context
  for each non-first segment, crossfades 1-bar overlap via `rubato`.
- Output: `tracks/<id>/raw.wav` at the model's native sample rate
  (44.1k for SAO 1.0, 32k for MusicGen — write whichever the sidecar
  reports in its health response).
- **Witness:** `tests/witnesses/audio_gen_3min_chain.rs` —
  `// stage: 2`. Renders a real 180s track end-to-end; asserts
  WAV header + duration ±2s + non-zero RMS.

## N1.7 nightdrive-art ships (S, depends N1.2)

- Trait `CoverArtist`. `SdxlClient` impl against a `stable-diffusion-webui`
  or `comfyui` HTTP endpoint on cnc:8081 (or kokonoe pre-P100).
- 1024×1024, negative-prompt locked per
  `nightdrive.toml.example` `[art].negative_prompt`.
- Output: `tracks/<id>/cover.png`.
- **Witness:** `tests/witnesses/art_real_sdxl.rs` — `// stage: 3`.
  Hits real SDXL; asserts PNG header + 1024×1024 dimensions.

## N1.8 nightdrive-audio-master ships (S, depends N1.6)

- `tokio::process::Command` wrapper around ffmpeg. Two-pass
  `loudnorm` to `target_lufs = -14.0`, `true_peak = -1.0`,
  `loudness_range = 11.0`.
- 2s fade-in / 4s fade-out. Output `master.flac` (lossless) +
  `master.mp3` (CBR 320 fallback).
- **Witness:** `tests/witnesses/master_loudnorm.rs` — `// stage: 4`.
  Feeds in a known noisy WAV; asserts the output's measured LUFS is
  within ±0.5 of -14.0 (re-measure with a second ffmpeg loudnorm pass
  in analyze mode).

## N1.9 nightdrive-encoder ships (M, depends N1.5/6/7/8)

- Trait `Encoder`. `FfmpegEncoder` impl: H.264 high@1080p30 + AAC
  320k, MOV faststart container.
- 3s cover intro + 3s outro frames.
- **MVP visualizer placeholder:** ffmpeg `showwaves=mode=cline` +
  cover.png as background. Ugly but ships. Replaced in Phase N3.
- Output: `tracks/<id>/final.mp4`.
- **Witness:** `tests/witnesses/encode_with_showwaves.rs` —
  `// stage: 6`. Asserts MP4 muxes, ffprobe reports correct
  duration + codec + 1080p30.

## N1.10 nightdrive-youtube hardening (M)

- The single-shot PUT in the staged `lib.rs` is fine for MVP.
- Add **chunked resumable upload** (8 MB chunks, Content-Range +
  308-Resume-Incomplete handling) so >50 MB uploads survive
  network blips. The TODO comment at line 228 of the staged file is
  the entry point.
- Wire `videos.update` for the synthetic-content disclosure flag (see
  HANDOFF.md §5 stage 7).
- **Witness:** `tests/witnesses/youtube_resume_upload.rs` —
  `// stage: 7`. Uploads a 60 MB file, kills the connection mid-PUT,
  resumes from the byte offset YouTube reports, asserts final video
  resource has the right size.

## N1.11 nightdrive-orchestrator wired (S, depends N1.4–N1.10)

- Replace every `warn!("not yet implemented")` in `pipeline_one_track`
  with the real call chain.
- Per-track `tokio::join!` of audio+art still parallel.
- DB state transitions update in lockstep with stage progress.
- **Witness:** `tests/witnesses/pipeline_one_dry_run.rs` —
  `// stage: 0`. Runs `pipeline_one_track(..., dry_run=true)` against
  real Ollama + Stable Audio + SDXL + ffmpeg; final state =
  `video_encoded`, no upload attempted; final.mp4 exists at the
  expected path.

## N1.12 nightdrive-cli ships (S)

- Subcommands: `db migrate`, `youtube auth` (one-shot OAuth Desktop
  flow with browser open), `tracks list`, `uploads list`,
  `stream status`.
- **Witness:** `tests/witnesses/cli_db_migrate.rs` — `// stage: 0`.
  Spawns the binary against a tmp DB, asserts migration tables exist.

## N1.13 systemd units installed on arch-controller (S)

- `nightdrive-nightly.{service,timer}` from this repo deploy to
  `/etc/systemd/system/`.
- `EnvironmentFile=/etc/nightdrive/nightdrive.env` populated from
  `.env.example` template + real OAuth refresh token.
- `systemctl enable --now nightdrive-nightly.timer` returns clean.
- **Witness:** `tests/witnesses/systemd_units_lint.rs` —
  `// stage: 0`. Runs `systemd-analyze verify` on both unit files
  (skipped on Windows; integration host runs on the linux box).

## N1.14 First private VOD end-to-end (S, depends all of N1)

- `nightdrive-orchestrator run-batch --count 1` against real
  everything; final track lands in YouTube as `private`.
- Disclosure flag set. Thumbnail set. DB row in `uploads` with
  `youtube_video_id` populated.
- Telegram notification fires on success (per
  `~/.claude/CLAUDE.md` notify section).
- **Witness:** `tests/witnesses/first_vod_e2e.rs` — `// stage: 7`.
  Runs the full pipeline, asserts a private video resource exists in
  YT API for the returned ID, checks privacyStatus="private",
  selfDeclaredMadeForKids=false.

**Phase N1 done:** one full pipeline run produces a real, gated-private
YouTube video from a cron tick. Day 5 from start per HANDOFF.md §10.

---

# Phase N2 — Catalog & livestream (Stage 8, durability)

End state: 30+ tracks in the catalog, 24/7 RTMP stream rotating them,
running unattended for 7 days. The flywheel starts here.

## N2.1 Resume subcommand (M, depends N1.11)

- `nightdrive-orchestrator resume` finds tracks where `state IN
  (spec_generated, audio_rendered, cover_rendered, audio_mastered,
  video_encoded)` and re-runs the pipeline from that stage forward.
- Idempotent: re-running on a `published` track is a no-op.
- **Witness:** `tests/witnesses/resume_from_audio_rendered.rs` —
  `// stage: 0`. Plant a track in `audio_rendered` state with raw.wav
  on disk, run resume, assert it advances to `published`.

## N2.2 Track dedup (S, depends N1.3)

- `nightdrive-llm` checks DB before generating: never re-emit a spec
  with a title within Levenshtein-3 of an existing track's title in
  the last 60 days.
- `nightdrive-orchestrator` short-circuits if `track_id` already
  exists in DB.
- **Witness:** `tests/witnesses/dedup_blocks_repeat_title.rs` —
  `// stage: 1`. Insert track "Neon Drift", attempt to generate
  "Neon Drift on Highway 9", assert spec is rejected with retry.

## N2.3 Audio fingerprint pre-scan (M, depends N1.8)

- After mastering, run `audfprint` (or `chromaprint` via `acoustid`)
  against a corpus of known training-set fingerprints (e.g. the
  Lakh / FMA datasets).
- If match score > threshold, mark track `state = failed,
  failure_reason = "potential content-id collision"` and skip upload.
- **Witness:** `tests/witnesses/fingerprint_blocks_known_track.rs` —
  `// stage: 4`. Feed in a known commercial track, assert the scan
  flags it.

## N2.4 Livestream player (L, depends N1.3)

- New binary subcommand: `nightdrive-orchestrator livestream`.
- Pulls tracks `ORDER BY last_streamed_at ASC NULLS FIRST`,
  respecting `min_replay_gap_hours = 24`.
- Audio playback: `rodio` or direct `libpulse` into a virtual sink
  (`pactl load-module module-null-sink sink_name=nightdrive`).
- On track start: update `last_streamed_at`, append
  `livestream_rotation_log` row.
- **Witness:** `tests/witnesses/livestream_rotates_3_tracks.rs` —
  `// stage: 8`. With 3 tracks in DB, run livestream for 30s, assert
  all 3 `last_streamed_at` advanced and rotation log has 3 rows.

## N2.5 Visualizer WebSocket server (M, depends N2.4)

- Tiny `tokio-tungstenite` server on `:7373`.
- Pushes `{type:"track", title, bpm, key, seed}` on track change.
- Pushes `{type:"level", value: 0..1}` at `metadata_refresh_seconds`
  cadence (FFT amplitude average from the audio thread).
- Pushes `{type:"beat"}` on detected onset.
- **Witness:** `tests/witnesses/ws_streams_track_metadata.rs` —
  `// stage: 8`. Connects via `tungstenite`, asserts the three
  message types arrive in order during one track transition.

## N2.6 OBS integration (M, depends N2.5)

- OBS on arch-controller hosts `visualizer/index.html` as Browser
  Source, pointed at the visualizer WS endpoint from N2.5
  (`$NIGHTDRIVE_VISUALIZER_WS_URL`, served on
  `arch-controller.tailb85819.ts.net:7373/visualizer` over the
  Tailscale mesh — WireGuard provides transport encryption).
- Audio source = the pulseaudio virtual sink from N2.4.
- OBS WebSocket on `:4455` controlled from `nightdrive-orchestrator
  livestream` for stream start/stop and scene swaps.
- RTMP push to `rtmp://a.rtmp.youtube.com/live2/$NIGHTDRIVE_YT_STREAM_KEY`.
- **Witness:** `tests/witnesses/obs_ws_starts_stream.rs` —
  `// stage: 8`. Connects to OBS WS, asserts `StartStream` returns
  successfully and stream state goes `OFFLINE → STARTING → STREAMING`
  within 10s.

## N2.7 Rotation logic + gap enforcement (S, depends N2.4-2.6)

- Pull-N strategy: keep a `shuffle_buffer_size` window in memory; on
  each track pop, refill from DB respecting gap.
- On underflow (catalog too small), loop back to the oldest eligible
  track + log a `tracing::warn!` "catalog small, rotation tightening".
- **Witness:** `tests/witnesses/rotation_respects_gap.rs` —
  `// stage: 8`. With 5 tracks (one streamed 1h ago), assert next pop
  is NOT that track when `min_replay_gap_hours = 24`.

## N2.8 Catalog growth to 30 tracks (S, batch op)

- Backfill script: `nightdrive-orchestrator run-batch --count 30
  --dry-run=false --schedule-offset-hours=0` over a single weekend
  burst on cnc.
- All 30 land in `published` state; 1 visible publicly per day on
  the VOD channel; remaining 29 feed the livestream rotation.
- **Done criterion:** `tracks` table count >= 30 AND
  `livestream_rotation_log` shows ≥ 3 distinct tracks streamed in any
  rolling hour.

## N2.9 Livestream runs 7 days unattended (M, operational milestone)

- Zero manual interventions for 7 consecutive days.
- OBS auto-reconnect on RTMP drop, systemd auto-restart on player
  crash, YouTube re-key on stream-end.
- **Witness:** `tests/witnesses/livestream_uptime_7d.rs` —
  `// stage: 8`. Reads Prometheus counter `nightdrive_livestream_uptime_seconds`,
  asserts ≥ 604_800 since last `systemctl reset-failed`.

**Phase N2 done:** the flywheel is spinning. 1 VOD/day + 24/7 stream
both live. HANDOFF.md §10 "Week 3" milestone hit.

---

# Phase N3 — Visuals v2 (kill the showwaves placeholder)

End state: real audio-reactive synthwave scene baked into VOD MP4s,
seeded per track for visual variety; livestream uses the same scene
via Three.js Browser Source (`visualizer/index.html` already exists).

## N3.1 wgpu headless renderer baseline (L)

- New crate `nightdrive-visuals`. Headless `wgpu` setup: surface →
  off-screen texture → `wgpu::Texture` → readback to CPU → ffmpeg pipe.
- Port the `index.html` Three.js scene into wgpu shaders (sun disc
  shader, grid floor, mountain silhouettes, particle stars).
- 1080p30 baseline render, no audio reactivity yet.
- **Witness:** `tests/witnesses/wgpu_renders_static_frame.rs` —
  `// stage: 5`. Render one frame, hash, assert non-trivial entropy
  and dominant pink/purple palette.

## N3.2 Audio-reactive shader uniforms (M, depends N3.1)

- FFT analysis on the master.flac (low/mid/high band amplitudes per
  frame).
- Uniform updates per frame: `uLevel`, `uBeat`, `uBands[3]`, `uTime`.
- Sun pulse on beat, grid scroll speed proportional to mid-band,
  star twinkle from high-band.
- **Witness:** `tests/witnesses/visuals_react_to_audio.rs` —
  `// stage: 5`. Render 30s of a known track; sample uLevel uniform
  values; assert variance ≥ 0.15 (i.e. it actually moves).

## N3.3 Per-track palette + geometry seeding (S, depends N3.1)

- `seed = hash(track_id) & u32::MAX`.
- `mulberry32`-style RNG drives: mountain silhouette ridge heights,
  sun palette (one of 6 curated synthwave palettes), grid color pair,
  star density.
- **Witness:** `tests/witnesses/visuals_distinct_per_track.rs` —
  `// stage: 5`. Render frame from 20 distinct track_ids; pixel-hash
  the top 1k pixels of each; assert ≥ 80% pairwise distinct.

## N3.4 Code-scroll background (S, optional, depends N3.1)

- For tracks tagged "coding" / "programming" in mood_tags, scroll
  syntax-highlighted Rust/Python/HCL in the lower third at low alpha.
- Source: random snippets from a curated `assets/code_scroll/`
  directory (not from any real codebase — avoid leaking).
- **Witness:** `tests/witnesses/code_scroll_only_when_tagged.rs` —
  `// stage: 5`. Render two tracks (one coding-tagged, one not);
  assert code-scroll region differs only in the tagged one.

## N3.5 Title card + intro/outro frames (S, depends N3.1)

- 3s intro: cover.png centered + title text fade-in.
- 3s outro: "next track in <Ns> · nightdrive · 24/7" + fade-out.
- Replaces the ffmpeg `tpad` workaround in N1.9.
- **Witness:** `tests/witnesses/intro_outro_present.rs` —
  `// stage: 5`. ffprobe reports first 3s and last 3s have specific
  marker pixels (corner watermark planted by the renderer).

## N3.6 Visual diversity bench (S, depends N3.3)

- Standing bench: render n=20 random track_ids, compute
  pixel-hash diversity, fail bench if < 80% pairwise distinct.
- Append result to `BENCH_LEDGER.md` under `stage = visuals_diversity`.
- **Witness:** `tests/witnesses/diversity_bench_passes.rs` —
  `// stage: 5`. Same logic as the bench, runs as a test gate.

**Phase N3 done:** 30+ tracks have shipped with the v2 visualizer; no
viewer feedback that uploads "all look the same."

---

# Phase N4 — Operational hardening

End state: 7 consecutive days of unattended operation across both
VOD batch + livestream, with monitoring + alerting + auto-recovery.

## N4.1 Per-stage retry policy (S)

- `nightdrive_core::retry::with_backoff`: exponential 1s → 2s → 4s,
  max 3 retries per stage, jitter ±20%.
- Wrap every external HTTP call (Ollama, audio sidecar, SDXL,
  YouTube).
- **Witness:** `tests/witnesses/retry_recovers_transient_500.rs` —
  `// stage: 0`. Mock-server (one per witness — exception to no-mocks
  for retry behavior) returns 500 twice then 200; assert client
  recovers within 3 attempts.

## N4.2 Failure escalation to Telegram (S, depends N4.1)

- On terminal stage failure (retries exhausted), shell out to
  `notify-telegram.sh` with track_id + stage + last error message.
- Throttle: max 1 message per stage per hour to avoid spam during
  a sustained outage.
- **Witness:** `tests/witnesses/telegram_fires_on_failure.rs` —
  `// stage: 0`. Force a stage failure, assert the notify shim was
  invoked (intercept via `NIGHTDRIVE_NOTIFY_BIN` override).

## N4.3 OAuth refresh-token resilience (M)

- On 401 from `oauth2.googleapis.com/token`, log + page Matt via
  Telegram with the re-auth command.
- Cache access tokens with 5-minute safety margin before
  `expires_in`.
- **Witness:** `tests/witnesses/oauth_refresh_handles_401.rs` —
  `// stage: 7`. Inject a known-stale refresh_token, assert the
  client returns a clear `Youtube("oauth refresh failed: ...")`
  error rather than panicking.

## N4.4 Disk-pressure guard (S)

- Pre-batch check: abort if `work_dir` partition is > 80% full.
- Per-track cleanup: after `published`, remove `raw.wav` (keep
  `master.flac` per `[storage].keep_published_indefinitely`).
- **Witness:** `tests/witnesses/disk_guard_aborts_at_80pct.rs` —
  `// stage: 0`. Mock `statvfs` (or override path), assert pre-batch
  bails.

## N4.5 GPU contention guard (S)

- Pre-dispatch check: `nvidia-smi --query-gpu=memory.used,utilization.gpu --format=csv,noheader,nounits`.
- If utilization > 50% or free VRAM < required, queue + retry
  with backoff rather than failing.
- **Witness:** `tests/witnesses/gpu_guard_queues_on_contention.rs` —
  `// stage: 2`. Mock the smi output, assert dispatch waits.

## N4.6 Prometheus exporter on :9091 (M)

- `nightdrive_pipeline_stage_duration_seconds{stage}` histograms.
- `nightdrive_pipeline_failures_total{stage,reason}` counter.
- `nightdrive_last_successful_publish_timestamp_seconds` gauge.
- `nightdrive_livestream_uptime_seconds` gauge.
- `nightdrive_catalog_size_total` gauge.
- **Witness:** `tests/witnesses/prometheus_metrics_exported.rs` —
  `// stage: 0`. Hit `/metrics`, assert all five metrics present.

## N4.7 Grafana dashboard (S, depends N4.6)

- Single board on the existing CNC Grafana instance: per-stage
  timing distributions, batch success rate (24h), livestream
  uptime, catalog growth curve, last-failure timestamp.
- Dashboard JSON committed to `ops/grafana/nightdrive.json`.
- **Witness:** `tests/witnesses/grafana_dashboard_lints.rs` —
  `// stage: 0`. Run `dashboard-linter` (or jq schema check) on the
  JSON, assert clean.

## N4.8 Takedown / strike handling (M)

- New CLI: `nightdrive-cli takedown <video_id> --reason "..."`.
- Marks `uploads.status = removed`, sets `tracks.state = failed`
  with `failure_reason = "youtube_takedown: ..."`.
- Removes from livestream rotation immediately (livestream player
  re-checks DB on each track pop).
- **Witness:** `tests/witnesses/takedown_removes_from_rotation.rs` —
  `// stage: 8`. Insert track, mark takedown, assert next rotation
  pop excludes it.

## N4.9 Multi-channel scaffolding (M)

- Config: `[youtube.<channel_id>]` blocks, each with its own
  OAuth credentials.
- Orchestrator routes uploads to a channel based on
  `spec.youtube.channel_id` (LLM picks per track based on subgenre).
- DB: `tracks.channel_id`, `uploads.channel_id`.
- **Witness:** `tests/witnesses/multi_channel_routes_correctly.rs` —
  `// stage: 7`. Two channels configured, two tracks generated,
  assert each lands on the right channel.

## N4.10 7-day unattended operational milestone (depends N4.1-7)

- Zero manual interventions for 7 consecutive days.
- All stage-failure Telegram pings resolved by per-stage retry; no
  human in the loop.
- **Done criterion:** Grafana shows ≥ 7 days since the last `failed`
  state in `tracks` AND livestream uptime gauge ≥ 604_800.

**Phase N4 done:** the system runs itself. Matt's only intervention is
content review (private → public flip per N5.1).

---

# Phase N5 — Revenue milestones

Mostly external (algorithm, viewers, YouTube review). Engineering work
is the small set of items that gates the next milestone.

## N5.1 First public VOD (S, depends N1.14)

- After human visual approval (intro 3s, outro 3s, audio quality),
  flip a track from `private` to `public` via `nightdrive-cli yt
  publish <video_id>`.
- **Done criterion:** one public video on the channel, 0 strikes.

## N5.2 1 VOD/day cadence sustained 14 days (depends N1.14, N4.10)

- 14 consecutive daily uploads, zero failed batches.
- **Done criterion:** `uploads` table shows 14 rows in last 14 days,
  all `status = uploaded` AND public.

## N5.3 24/7 livestream channel public (depends N2.9)

- Stream privacy = public; channel banner / thumbnails set;
  live chat enabled with default moderation.

## N5.4 1,000 subs (external; track via YT Analytics API)

- New CLI: `nightdrive-cli analytics subs` polls YT Analytics
  monthly, writes to `analytics_snapshots` table.
- **Done criterion:** snapshot row shows subs ≥ 1000.
- Per HANDOFF.md §10: realistic Month 2-4.

## N5.5 4,000 watch hours (external)

- Same analytics polling. 50 concurrent × 24h × 30d = 36k
  watch-hours per month at livestream-scale, so this lands fast
  once N5.3 is live and discoverable.
- **Done criterion:** snapshot row shows watch_hours_12mo ≥ 4000.

## N5.6 Monetization approved (external; ~1 month after N5.4 + N5.5)

- Apply via YouTube Studio. Disclosure of AI-generated content
  honest per global rule.
- **Done criterion:** YPP status = monetized; ad revenue gauge in
  Grafana > 0.

## N5.7 First $100 revenue month (external)

- Compounding RPM. Mid-rolls on the livestream are the engine.
- **Done criterion:** monthly `analytics_snapshots.revenue_usd` ≥ 100.

## N5.8 Mid-roll optimization (S, depends N5.6)

- LLM composition spec adds `ad_break_bars: [16, 32, 48]`
  (musical bar boundaries from the section breakdown).
- `nightdrive-encoder` writes ID3 chapter markers at those bar
  timestamps.
- YouTube auto-places mid-rolls at chapter markers; ad placement
  no longer cuts mid-phrase.
- **Witness:** `tests/witnesses/midroll_chapters_present.rs` —
  `// stage: 6`. ffprobe shows chapter count == ad_break_bars.len().

## N5.9 Channel #2 cloned (M, depends N4.9)

- Different branding (e.g. "darksynth nightdrive"), same backend,
  routed via `[youtube.darksynth]` config block.
- LLM prompt branches on channel: subgenre filter at composition
  time.
- **Done criterion:** channel #2 has ≥ 7 public uploads, all from
  the same orchestrator process.

**Phase N5 done:** two channels monetized, > $100/mo, livestream
running 24/7. The compounding-rent thesis from HANDOFF.md §10 is
either validated or refuted with real numbers.

---

# Suggested ordering / parallelism

The graph below shows the dependency edges that matter. Items inside a
phase that don't share an edge can run in parallel.

```
N1.1 reshuffle ─► N1.2 core ─┬─► N1.3 storage ─────────────┐
                             ├─► N1.4 llm                  │
                             ├─► N1.7 art                  │
                             ├─► N1.10 youtube             │
                             └─► N1.12 cli                 │
N1.5 sao sidecar ───► N1.6 audio-gen ─► N1.8 master ─┐    │
                                                      ├──► N1.9 encoder ┐
                                                      │                  │
                                                      └─────────────► N1.11 orchestrator ─► N1.14 first VOD
N1.13 systemd ──────────────────────────────────────────────────────────► (deploy gate)

N2.1 resume ──┐
N2.2 dedup ───┼──► N2.8 catalog growth ─► N2.9 7-day livestream
N2.3 fp scan ─┘
N2.4 player ─► N2.5 ws ─► N2.6 OBS ─► N2.7 rotation ──┘

N3.1 wgpu ─► N3.2 reactive ─► N3.3 seeding ─┬─► N3.6 diversity bench
                                            ├─► N3.4 code-scroll
                                            └─► N3.5 intro/outro

N4.1 retry ─► N4.2 telegram ────────────────────────────────► N4.10 7-day op
N4.3 oauth ─► N4.4 disk ─► N4.5 gpu ─┐
N4.6 metrics ─► N4.7 grafana ─────────┴──────────────────────► (op visibility)
N4.8 takedown                          (independent)
N4.9 multi-channel ──────────────────────────────────────────► N5.9 channel #2

N5.1 first public ─► N5.2 cadence ─► N5.4 subs ┐
                                                ├─► N5.6 monetized ─► N5.7 $100/mo
                                    N5.5 hours ┘                       └─► N5.8 mid-roll opt
```

## Recommended attack order (Sprints)

Alternates between user-visible value and infrastructure. Aether
pattern: each sprint = 1-3 weeks of concentrated work; LLM-aided pace
historically lands "weeks" in days.

1. **Sprint A — MVP** (Days 1-7): N1.1 reshuffle → N1.2 core →
   N1.3 storage → N1.4 llm → N1.7 art → N1.5 sao-sidecar (deploy when
   cnc has P100s, ~2026-05-17) → N1.6 audio-gen → N1.8 master →
   N1.9 encoder (showwaves placeholder) → N1.10 yt hardening →
   N1.11 orchestrator → N1.12 cli → N1.13 systemd → N1.14 first VOD.
2. **Sprint B — durability** (Days 8-14): N2.1 resume → N2.2 dedup →
   N2.3 fingerprint → N4.1 retry → N4.2 telegram. End: 1 VOD/day
   sustained.
3. **Sprint C — livestream** (Weeks 3-4): N2.4 player → N2.5 ws →
   N2.6 OBS → N2.7 rotation → N2.8 catalog 30 → N2.9 7-day
   unattended → N5.1 first public → N5.3 livestream public.
4. **Sprint D — visuals v2** (Month 2 weeks 1-2): N3.1 wgpu →
   N3.2 reactive → N3.3 seeding → N3.5 intro/outro → N3.6 diversity.
   Parallel: N4.3 oauth → N4.4 disk → N4.5 gpu.
5. **Sprint E — observability** (Month 2 weeks 3-4): N4.6 prometheus
   → N4.7 grafana → N4.8 takedown → N4.10 7-day op milestone.
   Parallel: N5.4/5.5 analytics polling.
6. **Sprint F — monetization + scale** (Month 3+): N5.6 monetized →
   N5.7 $100/mo → N5.8 mid-roll opt → N4.9 multi-channel → N5.9
   channel #2.

Total roadmap: **5–7 months of focused execution** to first
monetized month. Aligned with HANDOFF.md §10 timeline (Month 4-7 for
first $100). Calibrated against the historical pattern that
month-scale estimates land in days when LLM-aided.

---

# Bench cadence

Standing benches that run on every milestone. Each appends a row to
`docs/BENCH_LEDGER.md`.

- `bench/pipeline_one_track/run_all.ps1` — one full pipeline run on a
  fixed `track_id` (seed=1010, BPM=92, 240s duration). Times each
  stage + GPU VRAM peak. **Required before any commit touching
  `nightdrive-audio-gen`, `nightdrive-audio-master`, or
  `nightdrive-encoder`.**
- `bench/audio_gen_segment_chain/run_all.ps1` — once N1.6 lands.
  Times N×30s segment generation + crossfade.
- `bench/visuals_render_minute/run_all.ps1` — once N3.1 lands. Times
  60s of headless wgpu rendering at 1080p30.
- `bench/visuals_diversity/run_all.ps1` — once N3.3 lands. Pixel-hash
  diversity across 20 random seeds.
- `bench/youtube_upload_throughput/run_all.ps1` — once N1.10 chunked
  resume lands. Bytes/sec to `private` upload.
- `bench/livestream_uptime/run_all.ps1` — continuous; reads
  Prometheus gauge.

Hardware caption (locked at top of `BENCH_LEDGER.md`): updates on
hardware change. Today: kokonoe RTX 3070 Ti 8GB fp16. Post-2026-05-17:
cnc 3× Tesla P100 16GB fp32. **Hardware change → reset baseline; do
not compare across rows of different hardware.**

---

# Closing

This roadmap is the path from cron-tick-to-published-VOD to a 24/7
monetized two-channel pipeline. It's measurable, ordered, and
witness-anchored. Every item has either a `tests/witnesses/` file
that exits 0 OR a `docs/BENCH_LEDGER.md` row that beats the
comparison.

The discipline-stack agents
(`bench-runner` / `honesty-auditor` / `roadmap-tracker` /
`witness-test-author` / `coverage-matrix`) operate on this document
and the witness tests it produces. Keep the IDs (N1.1, N2.4, etc.)
stable — the agents reference them.

Update this file when scope changes. Never silently retire items —
mark them `~~strikethrough~~` with a short reason so the audit trail
survives.
