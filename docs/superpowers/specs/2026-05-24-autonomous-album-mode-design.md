# Design — Autonomous Album Mode for nightdrive

**Date:** 2026-05-24
**Owner:** Matt Gates
**Status:** approved-pending-review (this doc)

## Problem

Through Vol. 3 (Atompunk / Atiom Punikn) every album drop is hand-fired:
Matt picks a theme, invokes the `album-composer` subagent, runs the
arranger, calls `nightdrive-orchestrator run-album --slug <s>`,
re-anchors `--publish-at` if quotas slip, and retries thumbnails
manually when the per-channel ~100/day cap hits. Five albums shipped
this way; the catalog goal is 240+ minutes (≈60 tracks ≈ 5 albums)
before the livestream flips on, plus continuing growth after.

Constraints:
- **GPU is shared with the openclaw inference fleet on cnc-server's
  P100s** (`openclaw-inference-{embed,scout,workhorse}`). The
  nightly-batch service already evicts those three units in
  `ExecStartPre` and restores in `ExecStopPost`; the pattern is proven
  (2026-05-22). Any new auto-render service must reuse it verbatim —
  no lock files, no new arbitration scheme. Matt has explicitly
  authorized this eviction pattern for the existing nightly service
  and nothing more; new services use the *same* pattern, not a new one.
- **YouTube has three stacked quotas**: 10-unit API quota (cheap), the
  per-project `defaultVideoInsertPerDayPerProject` = 6 video.insert/day
  hidden cap (Pacific reset), and a per-channel rolling-24h upload cap
  (~10/day). A 12-track album drop **cannot fit in one day** under
  defaults — Vol. 3 took two days to upload, Vol. 2 took two anchor
  pushes.
- **Per-channel thumbnail cap** ≈100/day; tracks 11 + 12 of Vol. 3
  shipped with auto-thumbs because the cap hit mid-album. No retry
  exists.
- **Danger-zone titles** ("Derez", "Recognizer" — both Tron soundtrack
  + film dialogue) are a takedown risk. The Tron Vol. 1 incident
  caught this only because Matt was in the loop.

The codebase is ready for automation — every primitive exists; the
sequencing logic doesn't.

## Goal

A self-running album pipeline that:
1. Drops a new 12-track album every 3 days
2. Picks themes from a Matt-curatable backlog; auto-extends the backlog
   weekly by asking openclaw `main` (Opus 4.7 OAuth) for new proposals
3. Honors a 24h Telegram "soak" — proposals auto-promote unless Matt
   NACKs
4. Retries thumbnails on a 6h cadence within rate limits
5. Coordinates GPU access with openclaw by reusing the proven
   eviction-pattern (no new arbitration scheme)
6. Stays auditable: backlog is file-based JSON, all transitions
   logged, Matt-overridable at any point

Approach **C** from the brainstorm: stacked systemd timers, file-based
backlog, 24h auto-promote.

## Architecture

```
                          ┌─────────────────────────────────┐
                          │ docs/album-backlog.json         │
                          │ {                                │
                          │   proposed: [{slug,theme,...}],  │  ←─── theme-propose.timer (weekly)
                          │   approved: [{slug,theme,...}],  │       calls openclaw main, writes proposed[]
                          │   history:  [{slug, dropped_at}] │
                          │ }                                │
                          └─────────────────────────────────┘
                                       ↓
                              album-drop.timer (every 3 days)
                                       ↓
                          1. Promote expired proposals (>24h, no NACK)
                          2. Pop head of approved[]
                          3. Run nightdrive-album-composer
                             → docs/albums/<slug>.json
                          4. Arranger enrichment pass: SKIPPED in V1 (composer
                             output is the audio-gen input directly).
                             Configurable in V2 if seam quality drops.
                          5. Render covers (SDXL)
                          6. (eviction) → ACE-Step renders 12 tracks
                          7. ffmpeg encode → upload → publishAt
                          8. (restore) openclaw-inference back up
                          9. Append to history[]
                                       ↓
                              thumbnail-retry.timer (every 6h)
                                       ↓
                          For each published track w/ no custom thumb:
                            try set_thumbnail; on 429 → log + wait next tick
```

## Components

### 1. New crate: `nightdrive-album-composer`

Rust port of the Claude-side `album-composer` subagent. Standalone
binary + lib.

Input:
- `theme` (string, e.g. "Tokyo Cyberpunk")
- `recurring_motifs[]` (pulled from previous albums in `docs/albums/`)
- `danger_zone` (list of title+composer pairs from
  `docs/album-danger-zone.json` — known soundtracks + films we can't
  echo)
- `track_count` (default 12)

Output: `docs/albums/<slug>.json` matching the existing schema
(album_slug, title, theme, tonic_progression, bpm_arc, narrative_arc,
recurring_motifs, tracks[12]).

LLM backend: openclaw `main` via `podman exec openclaw-gateway openclaw agent` (Opus 4.7 OAuth).
Failure → fall back to LiteLLM Sonnet so a refresh-race doesn't block
album drops.

Danger-zone check: after generation, the composer cross-references
every track title against `docs/album-danger-zone.json`; any double-hit
(film title AND soundtrack title) → reject + re-roll up to 3x → on
3-strike, write proposal with `composer_blocked: true` and Telegram-ping
Matt for manual rename.

### 2. New crate: `nightdrive-openclaw-main`

Thin subprocess wrapper. Single function:

```rust
pub async fn ask_main(prompt: &str, timeout_secs: u64) -> Result<String, Error>
```

**Chosen RPC path (discovered 2026-05-24):** `podman exec openclaw-gateway openclaw agent --agent main --message <prompt> --json`

The gateway exposes **no REST endpoint** for agent messaging. All HTTP
routes under `/api/v1/agents/*` return 404. The gateway protocol is
WebSocket/RPC-only (discovered from `/app/dist/gateway/protocol/index.js`
which exports only `validateSessionsSendParams`, `validateChatSendParams`,
etc. — no HTTP route registrations). The gateway does expose `/health`
(`{"ok":true,"status":"live"}`) and a React SPA at `/docs`, but no
programmatic message API.

The `openclaw agent` CLI subcommand (inside the container) is the
canonical RPC surface. With `--json` it returns a structured payload:
`result.payloads[0].text` = reply text. Confirmed working 2026-05-24:
round-trip ~21s, exit 0, `PONG` reply from `claude-opus-4-7`.

Invocation pattern:
```
sudo podman exec openclaw-gateway \
  openclaw agent --agent main --message <prompt> --json
```

Parse reply: `serde_json` → `response["result"]["payloads"][0]["text"]`.
No auth token needed (runs inside the gateway container as the
already-authenticated session). No `NIGHTDRIVE_OPENCLAW_GATEWAY_TOKEN`
env var required — remove that from `.env.example`.

Read-only by design; no session spawning, no tool surface. Returns the
assistant's text reply.

Used by:
- `nightdrive-album-composer` (per-album JSON gen)
- The theme-propose timer (weekly batch propose)

### 3. CLI extensions on `nightdrive-cli`

```
nightdrive-cli thumbnails retry-failed [--max N] [--dry-run]
nightdrive-cli album backlog list
nightdrive-cli album backlog add <slug> --theme <t> [--approved]
nightdrive-cli album backlog approve <slug>
nightdrive-cli album backlog nack <slug>
nightdrive-cli album backlog remove <slug>
nightdrive-cli album propose [--count N]         # one-shot manual trigger
nightdrive-cli album drop-next [--dry-run]       # what the timer calls
```

`thumbnails retry-failed`: DB query for tracks in `state=published AND
custom_thumbnail_set=false`, batch retry via `set_thumbnail_best_effort`
respecting a `--max` ceiling (default 80 — leaves 20-thumb headroom
for fresh uploads). Stops cleanly on first 429 and exits 0.

`album drop-next`: top of the file flow above. If
`approved[]` is empty after promotion sweep, exits 0 with a Telegram
ping ("backlog empty, theme-propose hasn't run or all proposals were
NACKed"). Idempotent: safe to call manually.

### 4. Three systemd unit pairs in `scripts/`

All reuse the eviction pattern from `nightdrive-nightly.service` lines
16-22 verbatim. No new arbitration.

| Service | Timer | When | What |
|---------|-------|------|------|
| `nightdrive-album-drop.service` | `.timer` | every 3 days at 02:00 PT (= 09:00 UTC, 2h after Pacific quota reset) | `nightdrive-cli album drop-next` |
| `nightdrive-thumbnail-retry.service` | `.timer` | every 6h | `nightdrive-cli thumbnails retry-failed --max 80` (no GPU; no eviction needed) |
| `nightdrive-theme-propose.service` | `.timer` | weekly, Sunday 03:00 PT | `nightdrive-cli album propose --count 3` |

Album-drop fires 09:00 UTC — comfortably after Pacific midnight reset
(07:00 UTC PDT) so the full 6 video.insert/day budget is fresh. A
12-track album still won't fit one day's quota, so the drop is
two-stage: tracks 1-6 day 1, tracks 7-12 day 2, then publishAt fires
on day 3. The album-drop timer handles this by checking
`album_progress` table state at start: if last album is partially
uploaded, resume from `--from-track N+1` instead of popping a new slug.

### 5. Backlog file format

`docs/album-backlog.json`:

```json
{
  "version": 1,
  "proposed": [
    {
      "slug": "tokyo-cyberpunk-vol-1",
      "theme": "Tokyo cyberpunk noir",
      "proposed_at": "2026-05-24T07:30:00Z",
      "promote_at": "2026-05-25T07:30:00Z",
      "proposed_by": "manual" | "openclaw-main"
    }
  ],
  "approved": [
    { "slug": "miami-vice-vol-1", "theme": "Miami Vice / Vapor Coast", "approved_at": "..." }
  ],
  "history": [
    { "slug": "atompunk-drive-vol-1", "dropped_at": "2026-05-21T00:00:00Z" }
  ]
}
```

Promotion rule: when `album-drop` runs, any `proposed[]` entry with
`promote_at <= now` is moved to the tail of `approved[]`. NACK via
Telegram (or `nightdrive-cli album backlog nack <slug>`) deletes from
`proposed[]` before promotion.

### 6. Seed backlog (lands with this spec)

```json
{
  "approved": [
    { "slug": "tokyo-cyberpunk-vol-1",      "theme": "Tokyo cyberpunk noir, neon Shinjuku rain" },
    { "slug": "miami-vice-vol-1",           "theme": "Miami Vice / Vapor Coast — pastel coke-era" },
    { "slug": "blade-runner-2049-vol-1",    "theme": "Blade Runner / LA 2049 — rain, neon Chinatown" },
    { "slug": "berlin-wall-vol-1",          "theme": "Cold War East — Checkpoint Charlie, Trabants, divided city" }
  ]
}
```

These four populate `approved[]` directly (Matt explicitly chose them
in the brainstorm). After they're dropped, the weekly theme-propose
will keep the queue fed.

Danger-zone seeds (`docs/album-danger-zone.json`):
- Blade Runner: avoid "Tears in Rain" (Vangelis soundtrack + film
  dialogue), "Memories of Green" (Vangelis), "Wait for Me"
- Tokyo Cyberpunk: avoid Akira soundtrack titles, Ghost in the Shell
  "Reincarnation" (Kawai), Blade Runner overlap
- Miami Vice: avoid "In the Air Tonight" (Phil Collins), "Crockett's
  Theme" (Hammer)
- Berlin Wall: avoid "Heroes" (Bowie), "Wind of Change" (Scorpions)

Composer cross-references this list at generation time.

## GPU coordination (verbatim from existing pattern)

`nightdrive-album-drop.service` ExecStart wrappers:

```ini
[Service]
Type=oneshot
ExecStartPre=+systemctl stop openclaw-inference-embed openclaw-inference-scout openclaw-inference-workhorse
ExecStartPre=+sleep 3
ExecStartPre=+systemctl start nightdrive-acestep
ExecStartPre=+sleep 10
ExecStart=/opt/nightdrive/bin/nightdrive-cli album drop-next
ExecStopPost=+systemctl stop nightdrive-acestep
ExecStopPost=+systemctl start openclaw-inference-embed openclaw-inference-scout openclaw-inference-workhorse
TimeoutStartSec=6h
```

`thumbnail-retry.service` does NOT touch GPU — no eviction.
`theme-propose.service` does NOT touch GPU (single HTTP call to
gateway) — no eviction.

## Telegram surface

Telegram is **one-way** (uses `notify-telegram.sh` send-only). NACKs
happen via SSH + `nightdrive-cli album backlog nack <slug>`, OR by
editing `docs/album-backlog.json` directly (commit auto-fires via
github-uploader). No bot-listener is built; adding one is out of scope.

| Event | Message |
|-------|---------|
| `theme-propose` ran | `nightdrive: 3 new themes proposed — 24h soak. NACK any via 'nightdrive-cli album backlog nack <slug>' on cnc. Slugs: ...` |
| Auto-promote fired | `nightdrive: <slug> promoted to active backlog (silent 24h).` |
| `album-drop` started | `nightdrive: dropping <slug>. ETA ~3h for render + 2-day upload window.` |
| `album-drop` complete | `nightdrive: <slug> 12/12 done — sync-drop <iso> armed.` |
| Quota hit mid-drop | `nightdrive: <slug> hit <quota>; rescheduling drop-next for <iso>.` |
| Composer danger-zone strike-3 | `nightdrive: <slug> blocked — track titles collide with <hits>. Reply /override <slug> or /skip <slug>.` |
| Thumbnail retry | (silent unless ≥10 retried in one pass, then summary) |

Reuses the `notify-telegram.sh` script.

## Migration / rollout

1. Land crates + CLI commands (compiles green, all new code unit-tested).
2. Land systemd units in `scripts/` but **timers disabled by default**.
   Matt opt-in with `systemctl enable --now nightdrive-album-drop.timer`
   when ready.
3. First production run: `nightdrive-cli album drop-next --dry-run`
   against `tokyo-cyberpunk-vol-1` to verify the chain end-to-end
   before letting the timer fire.
4. Enable thumbnail-retry timer first (low-risk, no GPU). Watch one
   cycle.
5. Enable theme-propose timer second. Watch one cycle (proposals
   show up in `proposed[]`, Telegram pings; Matt can NACK to test).
6. Enable album-drop timer last.

## Open issues / risks

- **Refresh-race with openclaw main OAuth**: the openclaw-fleet skill
  flags that bare-metal claude-orchestrator + gateway already share
  the OAuth credential file. Adding nightdrive as a third consumer
  (via HTTP to the gateway, not direct file access) doesn't worsen
  the race — gateway handles its own refresh. Low risk.
- **Backlog file race**: timers don't overlap (different schedules)
  but theme-propose + manual `backlog add` could collide. Use
  `flock` on `docs/album-backlog.json` for all writes.
- **Danger-zone false positives**: a too-strict list could 3-strike
  every proposal. Mitigation: log every rejection so we can tune.
- **What if the channel gets a strike during the soak window?** The
  auto-promote rule still fires. Add a "channel-health" pre-check:
  `album-drop` refuses to fire if `youtube_strikes > 0`.
  Implementation: simple field in `docs/album-backlog.json`,
  Matt-toggleable, default 0.
- **Vol. 5+ are different albums than Vol. 1-4 stylistically.** The
  composer needs the existing 5 JSONs as in-context examples or it'll
  drift. Solution: composer reads the most recent 3 album JSONs from
  `docs/albums/` and includes them as few-shot examples in the prompt.

## Out of scope (deliberately)

- Livestream supervisor (`livestream` subcommand). Still stubbed.
  Catalog-first rule still holds (240min before livestream); we'll
  hit that automatically after ~5 more albums.
- `status` subcommand fill-in. Useful but not blocking.
- Cover art pre-gen on a separate timer. Current per-album SDXL gen
  is fast enough that batching it forward doesn't help.
- Replacing the LiteLLM path for `nightdrive-llm`. The per-track
  composition spec gen stays on LiteLLM Sonnet; only the
  *album-composer* (12-track meta-spec) and *theme-propose*
  (weekly proposals) route through openclaw main.
