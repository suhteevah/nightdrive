# nightdrive — Project Instructions

> **Scope:** project-specific rules for the `nightdrive` repo. Inherits everything in `~/.claude/CLAUDE.md` (global). Read `HANDOFF.md` first in every session — it's the single source of truth for vision, decisions, and build order.

---

## DO NOT REINVENT (read this first, before adding any new code)

Matt's wiki at `J:\llm-wiki\` is the index of working stuff that already
exists across his fleet. **Before writing a new module here, search it.**
Cross-project reuse is the rule, not the exception. Canonical "reinvented
wheels" docs:

- `J:\llm-wiki\projects\Pixiedust\Reinvented Wheels.md`
- `J:\llm-wiki\projects\ClaudioOS\Reinvented Wheels.md`

**Three matches that change nightdrive's plan today:**

### 1. Audio generation — `J:\pledgeandcrowns\tools\synthwave-gen\` already works

`HANDOFF.md` plans a `musicgen-server.py` + Rust `nightdrive-audio-gen`
client. **Reuse `pledgeandcrowns/tools/synthwave-gen/` instead** — it
already does almost exactly what nightdrive needs:

- Stable Audio Open 1.0 (HF `stabilityai/stable-audio-open-1.0`) via
  `diffusers`, fp16 on CUDA
- Manifest-driven (`manifest.json`): per-track prompt + duration_s + seed
  + shared negative prompt + default steps/cfg
- Graceful gated-repo error message that walks the user through `HF_TOKEN`
  / `huggingface-cli login`
- **T5 prompt-truncation preflight** — text encoder caps at 128 units
  and silently truncates the TAIL (where "no vocals, no thrumming bass"
  directives live). The script prints `[synthwave-gen][WARN] track
  'X' prompt is N units, TAIL WILL BE DROPPED`. Steal this verbatim —
  the same trap will bite MusicGen prompt eng if we ignore it.
- Multi-GPU subprocess fanout (`fanout.py`): one `generate.py`
  subprocess per CUDA card, scoped via `CUDA_VISIBLE_DEVICES=<n>`,
  manifest split round-robin. Simpler than `torch.distributed`,
  fault-tolerant per card, no cross-device sharding needed.
- Per-card VRAM tracking, deterministic by seed, pinned reqs.

**HANDOFF.md says MusicGen-large primary, Stable Audio Open
"experimental." That's stale.** Stable Audio Open is the path Matt has
working code for. Default to it. MusicGen stays as a future
`AudioGenerator` trait impl if seam-y 30s chaining proves problematic.

The pledgeandcrowns version generates short tracks (≤47s — Stable Audio
hard limit). Nightdrive needs 3–6 minute tracks, which means we still
have to chain. Build the chaining/crossfade logic in
`nightdrive-audio-gen` but call the existing `generate.py` (or its FastAPI
descendant) for each segment.

Lesson page: `J:\llm-wiki\projects\pledge-and-crown.md` (audio score
section) and `J:\llm-wiki\incidents\2026-05-09-llm-cant-compose-known-pieces.md`
(the "Stable Audio truncates at 128 tokens" composition example).

### 2. The "supermicro 8× Tesla P40 192GB VRAM" box doesn't exist

`HANDOFF.md` §3 maps inference work to `supermicro.tailb85819.ts.net`.
Per `J:\llm-wiki\fleet\Fleet Overview.md` (2026-04-19) the actual GPU
pool is:

| Box        | GPU(s) (today)               | VRAM    | Tailscale name                      |
|------------|------------------------------|---------|--------------------------------------|
| Kokonoe    | RTX 3070 Ti                  | 8 GB    | `kokonoe.tailb85819.ts.net`          |
| Satibook   | RTX 3050                     | 6 GB    | (Win laptop)                         |
| CNC-Server | GTX 980                      | 4 GB    | `cnc-server` (192.168.168.100)       |
| hp-victus  | RTX 3050                     | —       | dev / fallback                       |

**Inbound: 3× Tesla P100 land in cnc-server by ~2026-05-17.** That
flips cnc from "tiny GTX 980 RPC worker" to **the audio-gen + SDXL
muscle box** (≈48 GB additional VRAM, ~52 GB total). Plan the
orchestrator-host split accordingly:

- **cnc-server (Linux, post-P100):** Stable Audio Open + SDXL inference,
  one model copy per card via `CUDA_VISIBLE_DEVICES`. Use the
  `synthwave-gen/fanout.py` round-robin pattern verbatim — that script
  was literally designed for "the inbound 3× P100 rig" (see
  `pledge-and-crown.md` line 37). nightdrive-orchestrator systemd
  timer also lives here (per the "scheduled jobs go on cnc" rule below).
- **Kokonoe:** wgpu visualizer renders only (Windows, headless wgpu
  on the 3070 Ti). Stays out of the inference loop once cnc has the
  P100s — nothing else on kokonoe.
- **arch-controller:** 24/7 OBS RTMP origin for the livestream,
  reading the cnc-rendered MP4s over Syncthing/Tailscale.

**Pascal (sm_60) caveats** — see
`J:\llm-wiki\patterns\candle-p100-pascal-compat.md`:
- P100 has **no fp16 acceleration**. The synthwave-gen `generate.py`
  hardcodes `torch.float16` to fit the 3070 Ti's 8 GB; on a P100 16 GB,
  drop the dtype override (or pass through fp32) — slightly more
  numerically stable, ~60-90s/track instead of ~30-60s.
- candle (if we ever use it for inference) needs the three sm_60
  patches documented in that pattern file.

The supermicro listing in
`J:\llm-wiki\experiments\4-gpu-server-listings-2026-04-24.md`
is **future research toward buying** a 4-GPU chassis — not a deployed
box and not on the near-term path. **Treat HANDOFF §3's hardware table
as aspirational.** Until cnc has the P100s, do single-card
bring-up on Kokonoe (~6-7 GB peak Stable Audio Open at fp16 fits 8 GB).

### 3. Scheduled jobs go on cnc, NOT kokonoe

`HANDOFF.md` says the orchestrator runs on `arch-controller`. Good.
**Don't be tempted to put the cron timer on kokonoe.** Per
`J:\llm-wiki\patterns\scheduled-jobs-on-cnc-not-kokonoe.md`: Windows
Task Scheduler launching `powershell.exe` from `svchost\Schedule`
hangs silently for minutes on kokonoe (Defender/AMSI per-launch
reputation work). Verified 2026-05-01 in
`incidents/2026-05-01-task-scheduler-powershell-amsi-hang.md`. Default
recurring jobs to **systemd timers on cnc** with output distributed via
Syncthing. The nightdrive systemd unit/timer files in this repo target
linux already — keep them on `arch-controller` or `cnc-server`, never
kokonoe.

### Other reusable bits worth knowing

- **Profile C `claude.ai` session-cookie auth** for any Anthropic call:
  `J:\llm-wiki\projects\Pixiedust\Reinvented Wheels.md` documents the
  baremetal-claude → brander-ci-agent → kalshi-trader-v7 port chain.
  If nightdrive ever needs Claude (not just Ollama), use Profile C, not
  the OAuth Bearer (`sk-ant-oat01-*`) path — the latter is a zombie that
  works in docs and Claude Code but **429s immediately on programmatic
  use**. This lesson has been re-learned 3+ times across projects.
- **`crates/api-client` (ClaudioOS)** — bare-metal Anthropic Messages API
  client (POST + SSE streaming + tool use) if we ever want a no-deps
  reference for hand-rolled HTTP/SSE patterns.
- **Ollama `/api/chat` JSON-mode** — `lib.rs` (the staged
  `nightdrive-llm` crate) already implements this correctly. The
  pattern is well-trodden across Matt's projects. Don't switch to a
  framework client.
- **`tracing` + structured fields, no `println!`** is universal. See
  `J:\llm-wiki\patterns\verbose-logging-everywhere.md`.
- **Wraith MCP > Playwright** for any browser automation we add later
  (e.g. YouTube OAuth bootstrap). See
  `J:\llm-wiki\patterns\prefer-wraith-over-playwright.md`.

### How to use this section

Before opening a new file in `crates/<name>/src/`:

1. `grep -ril "<capability>" J:\llm-wiki --include="*.md"` — does the
   wiki already mention working code for it?
2. If a Reinvented Wheels page lists it as shipped in another project:
   read that code FIRST. Decide port vs. depend vs. cross-call.
3. If you write something new that another project might want, **update
   the relevant Reinvented Wheels page in the same session.** Don't
   defer — undocumented lessons get re-learned (per `feedback_wheel_capture`).

---

## Discipline stack

Five on-demand subagents under `.claude/agents/` keep nightdrive honest:

| Agent                | Use when…                                                                          |
|----------------------|-------------------------------------------------------------------------------------|
| `bench-runner`       | After any commit touching audio-gen / mastering / encoder / visuals crates         |
| `honesty-auditor`    | Before any external claim (Telegram, README, HANDOFF.md edit, marketing copy)      |
| `roadmap-tracker`    | At session start; before claiming progress to a stakeholder; after a feature lands |
| `witness-test-author`| When a roadmap item ships and needs its `tests/witnesses/<n>.rs` file              |
| `coverage-matrix`    | Before PR descriptions; as input to `roadmap-tracker` reports                       |

Pattern ported from `J:\aether\.claude\agents\` (validated). On-demand
dispatch only — no auto-fire on commit, no `.claude/state/` cursors.
Findings return in-band; the caller decides what to persist.

**Anchor files the agents operate on:**

- `docs/ROADMAP.md` — phases N1-N5, ~50 numbered items, each with a witness criterion. `roadmap-tracker` reads.
- `docs/BENCH_LEDGER.md` — append-only perf history; hardware-caption-resets on hardware change. `bench-runner` writes.
- `bench/pipeline_one_track/run_all.ps1` — umbrella bench runner. `bench-runner` invokes.
- `tests/witnesses/*.rs` — `// stage: N` tagged real-endpoint integration tests; **no mocks** (per `tests/witnesses/README.md`). `witness-test-author` writes; `coverage-matrix` and `roadmap-tracker` read.
- `scripts/audit.ps1` — 7-section gate ([build/test/stubs/witnesses/bench-freshness/schema-drift/summary]). `honesty-auditor` and `roadmap-tracker` quote section counts verbatim.

**Run the gate locally before any claim:**

```powershell
powershell -ExecutionPolicy Bypass -File scripts/audit.ps1
```

Final line is either `OK - audit clean (build:0 test:N stubs:M witnesses:K)` or `FAIL - <list>`. The two known day-one drift items (HANDOFF.md §7 schema vs migration; HANDOFF.md §3 supermicro hardware claim) should each show up — fixing them is a Sprint A task in `docs/ROADMAP.md`.

**Spec + plan:**
- `docs/superpowers/specs/2026-05-10-discipline-stack-design.md` — design
- `docs/superpowers/plans/2026-05-10-discipline-stack.md` — implementation plan

---

## What this is

Autonomous synthwave generation & publishing pipeline. One `cron` tick →
LLM composition spec → MusicGen audio → SDXL cover → mastered FLAC →
wgpu visualizer → ffmpeg mux → YouTube upload. Same pipeline feeds a
24/7 livestream. Zero human-in-the-loop.

Two products from one codebase: daily VOD uploads + 24/7 RTMP livestream.
Goal is YouTube monetization (1k subs + 4k watch-hours), then compounding
mid-roll revenue on the never-ending stream.

## Status: SCAFFOLD

**No code is "running" yet.** The repo right now is a flat staging area:
the files at the root are templates that belong inside the 11-crate Cargo
workspace described in `HANDOFF.md` §4. They have not been moved yet.

Current file → eventual location:

| File at repo root                  | Belongs at                                                 |
|------------------------------------|------------------------------------------------------------|
| `Cargo.toml`                       | `Cargo.toml` (already correct, defines workspace members)  |
| `config.rs`                        | `crates/nightdrive-core/src/config.rs`                     |
| `lib.rs`                           | `crates/nightdrive-llm/src/lib.rs`                         |
| `main.rs`                          | `crates/nightdrive-orchestrator/src/main.rs`               |
| `index.html`                       | `visualizer/index.html`                                    |
| `musicgen-server.py`               | deployed to `supermicro:/opt/musicgen/`, systemd-managed   |
| `nightdrive-nightly.{service,timer}` | `scripts/` then `/etc/systemd/system/` on the orchestrator host |
| `20260510000000_init.sql`          | `crates/nightdrive-storage/migrations/`                    |
| `nightdrive.toml.example`          | `config/nightdrive.toml.example`                           |
| `mnt/user-data/outputs/nightdrive/crates/nightdrive-core/src/lib.rs`     | `crates/nightdrive-core/src/lib.rs` |
| `mnt/user-data/outputs/nightdrive/crates/nightdrive-youtube/src/lib.rs`  | `crates/nightdrive-youtube/src/lib.rs` |

`mnt/user-data/outputs/...` is leftover sandbox path — the real source
lives under `crates/`. Don't add new content under `mnt/`.

## First action when picking this up

1. Read `HANDOFF.md` end-to-end (don't skim).
2. Decide whether to (a) reshuffle the staged files into the proper
   `crates/<name>/src/lib.rs` layout, or (b) keep iterating on the
   templates in place. Default = **reshuffle first**, then `cargo check
   --workspace` for the green baseline before touching anything else.
3. Pick the next crate from `HANDOFF.md` §9 (Bootstrap order). Each
   crate's `src/lib.rs` has a `// TODO(nightdrive):` marker.

## Architecture (see HANDOFF.md §2 for the diagram)

```
cron → orchestrator → LLM (qwen2.5:7b on kokonoe via OpenClaw/Ollama)
                   → [MusicGen-large on supermicro | SDXL on supermicro]   (parallel, GPU)
                   → [ffmpeg loudnorm master      | wgpu visualizer render] (parallel)
                   → ffmpeg compose
                   → [YouTube Data API v3 VOD     | OBS RTMP livestream]
```

The orchestrator does **no inference**. It dispatches HTTP work over
Tailscale to the muscle boxes and stitches results.

## Hardware fleet

| Box              | Tailscale name                          | Role                                           |
|------------------|------------------------------------------|------------------------------------------------|
| supermicro       | `supermicro.tailb85819.ts.net`           | MusicGen :8080 + SDXL :8081 (8× Tesla P40)     |
| main-pc / kokonoe| `kokonoe.tailb85819.ts.net`              | Ollama :11434 (LLM) + wgpu visualizer renders  |
| arch-controller  | `arch-controller.tailb85819.ts.net`      | Orchestrator host + OBS RTMP origin            |
| hp-victus        | —                                        | Fallback / dev box                             |

Orchestrator runs on `arch-controller` under systemd
(`nightdrive-nightly.timer` → 22:00 daily).

## Crate map (11 members)

```
nightdrive-core           shared types (TrackId, CompositionSpec, TrackState, TrackPaths,
                          NightdriveError) + config + tracing init. EVERY crate depends on this.
nightdrive-llm            OpenclawLlm (Ollama JSON-mode chat) → CompositionSpec
nightdrive-audio-gen      HTTP client → musicgen-server.py, segment-chained generation
nightdrive-audio-master   ffmpeg loudnorm two-pass, EQ, fades → master.flac + master.mp3
nightdrive-art            SDXL/Flux HTTP client → cover.png (1024²)
nightdrive-visuals        wgpu headless renderer → scene.mp4 (1080p30, audio-reactive)
nightdrive-encoder        ffmpeg final mux: scene + master + intro/outro → final.mp4
nightdrive-youtube        Hand-rolled YT Data API v3: refresh→access token, resumable
                          insert, thumbnails.set. Avoid google-youtube3 (80+ deps).
nightdrive-storage        sqlx + sqlite: tracks, uploads, livestream_rotation_log
nightdrive-orchestrator   binary: run-batch | livestream | resume | status
nightdrive-cli            binary: manual triggers, db migrate, youtube auth, replays
```

Plus `visualizer/index.html` — Three.js retrowave scene, dropped into OBS
Browser Source for the livestream channel. Reactive via WebSocket; falls
back to a synthetic BPM envelope so it still looks alive offline.

## Hard rules for this codebase

- **Rust 2024 edition, MSRV 1.85.** Per workspace `Cargo.toml`. Don't downgrade.
- **`tracing` everywhere, no `println!`.** Every external call (Ollama,
  ffmpeg, YouTube, file IO) wrapped in `#[instrument]` with structured
  fields. Matches the global "verbose logging everywhere" rule.
- **Errors:** `thiserror`-defined `NightdriveError` for domain failures;
  `anyhow` for unexpected infra issues. Each crate maps its errors to
  the matching `NightdriveError` variant (`Llm`, `AudioGen`, etc).
- **One failure ≠ batch abort.** `run_batch` continues to the next track
  on per-track errors. See `main.rs::run_batch`.
- **Validate LLM output.** `nightdrive-llm::validate_spec` enforces BPM
  80–118, duration 180–360s, non-empty title/sections/tags. The model is
  creative — keep it honest. Don't relax these without a reason.
- **Hand-rolled YouTube client only.** Don't suggest pulling in
  `google-youtube3`. Five endpoints; we hand-roll.
- **Audio segments cap at 30s.** MusicGen-large limit. Chain with
  1-bar crossfades via `rubato` (Rust-native — no Python audio stitching).
- **AI disclosure honest.** `declare_synthetic_content = true` in
  `[youtube]` config. Don't try to game algorithm by hiding it.
- **YouTube CI is banned globally.** All builds local. Don't suggest
  GitHub Actions for anything.

## Config + secrets

- Runtime config: `/etc/nightdrive/nightdrive.toml` (template:
  `nightdrive.toml.example`). Loaded by `AppConfig::load()` in
  `nightdrive-core::config`. Resolution order:
  `$NIGHTDRIVE_CONFIG` → `/etc/nightdrive/nightdrive.toml` →
  `./config/nightdrive.toml` → `./nightdrive.toml`.
- Secrets in `.env` (template: `.env.example`). Never commit `.env`.
  Required:
  `NIGHTDRIVE_YT_CLIENT_ID`, `NIGHTDRIVE_YT_CLIENT_SECRET`,
  `NIGHTDRIVE_YT_REFRESH_TOKEN`, `NIGHTDRIVE_YT_STREAM_KEY`.
- Bootstrap the YT refresh token once via
  `nightdrive-cli youtube auth` (opens browser, OAuth Desktop flow).

## Storage layout (per track)

`/var/lib/nightdrive/tracks/<track_id>/`:
```
spec.json        cover.png         scene.mp4
raw.wav          thumbnail.jpg     final.mp4
master.flac      master.mp3
```
Path helpers in `nightdrive_core::TrackPaths`. Don't open-code the
filenames elsewhere — go through the helpers.

`TrackId` format: `nd-YYYYMMDD-NNN` (e.g. `nd-20260510-001`).

## DB schema (SQLite, sqlx)

Migration: `20260510000000_init.sql`. Tables: `tracks`, `uploads`,
`livestream_rotation_log`. State machine values for `tracks.state`:
`pending|spec_generated|audio_rendered|cover_rendered|audio_mastered|video_encoded|published|failed`
(matches `nightdrive_core::TrackState`).

The schema in `HANDOFF.md` §7 and the actual migration file have
**slightly different column sets** (the migration uses `seed`,
`visualizer_path`, `duration_secs`; the §7 doc uses `subgenre`, `bpm`,
`musical_key`, `audio_path`, `cover_path`, `video_path`,
`last_streamed_at`). The migration file is authoritative — fix the
drift the next time the spec is touched.

## Commands (when there's actually code)

```bash
# build
cargo check --workspace
cargo build --release --workspace

# db
./target/release/nightdrive-cli db migrate

# one-shot test
./target/release/nightdrive-orchestrator run-batch --count 1 --dry-run

# install nightly batch
sudo cp scripts/nightdrive-nightly.{service,timer} /etc/systemd/system/
sudo systemctl enable --now nightdrive-nightly.timer

# install livestream supervisor
sudo cp scripts/nightdrive-livestream.service /etc/systemd/system/
sudo systemctl enable --now nightdrive-livestream

# watch
journalctl -u nightdrive-nightly.service -f
./target/release/nightdrive-cli tracks list
./target/release/nightdrive-cli uploads list
./target/release/nightdrive-cli stream status

# musicgen server (on supermicro)
uvicorn musicgen-server:app --host 0.0.0.0 --port 8080 --workers 1
```

## Stub inventory (don't claim shipped)

These are explicitly stubbed in `main.rs::pipeline_one` and must be
implemented before the pipeline can produce a track end-to-end:

- Stage 2 audio_gen: spawned task only logs `warn!("not yet implemented")`
- Stage 3 art: same
- Stage 4 master: TODO comment, no call
- Stage 5 visualizer: TODO comment, no call
- Stage 6 final encode: TODO comment, no call
- `livestream`, `resume`, `status` subcommands: TODO bodies

The YouTube upload stage IS implemented (single-shot PUT, no chunked
resume yet — see `nightdrive-youtube/src/lib.rs:228` for the chunked
TODO).

When summarizing progress, list which of these are still stubs. Do not
say "the pipeline runs" while any of them are `warn!` placeholders.

## Resume protocol

Per global CLAUDE.md handoff rules:
1. Read `HANDOFF.md` first.
2. Trust prior session findings — don't re-diagnose.
3. Pick the next crate from `HANDOFF.md` §9.
4. Update `HANDOFF.md` §13 (or add a new section) at end of session
   with current state + blockers + next step.
