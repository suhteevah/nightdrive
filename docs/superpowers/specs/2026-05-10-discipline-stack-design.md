# Design — Discipline-Stack Subagents for nightdrive

**Date:** 2026-05-10
**Owner:** Matt Gates
**Status:** approved-pending-review (this doc)

## Problem

nightdrive is scaffold-stage. Eleven crates planned, most stubbed, real
external services (Ollama, Stable Audio Open, ffmpeg, YouTube,
OBS/RTMP) in the loop. Two failure modes are likely:

1. Overclaim drift — `HANDOFF.md` / `CLAUDE.md` / commit messages say
   "Stage N is shipped" while `warn!("not yet implemented")` is still
   in the code path. (The "ClaudioOS boot-ready" pattern.)
2. Untested integrations — without a real-endpoint test suite, every
   pipeline run is a manual smoke test against five external systems.

A 5-agent **discipline stack** validated on `J:\aether\` keeps these
in check. This spec ports that stack to nightdrive.

## Source pattern

`J:\aether\.claude\agents\` ships five on-demand subagents:
`bench-runner`, `honesty-auditor`, `roadmap-tracker`,
`witness-test-author`, `coverage-matrix`. They form a closed loop:
roadmap-tracker says what to attack → witness-test-author drafts the
test → bench-runner times the implementation → coverage-matrix shows
the gap closed → honesty-auditor verifies the claim before any
external statement.

Triggering is **on-demand via `description`-matching** — the parent
agent invokes by name when intent matches. No cursors, no
`.claude/state/`, no `.claude/reviews/`. Findings return in-band; the
caller decides what to persist.

## Design

### Layout

```
nightdrive/
├── .claude/agents/
│   ├── bench-runner.md
│   ├── honesty-auditor.md
│   ├── roadmap-tracker.md
│   ├── witness-test-author.md
│   └── coverage-matrix.md
├── docs/
│   └── BENCH_LEDGER.md            # append-only perf history
├── scripts/
│   └── audit.ps1                  # the gate the agents lean on
├── bench/
│   └── pipeline_one_track/
│       └── run_all.ps1            # bench-runner umbrella entry
└── tests/
    └── witnesses/
        └── README.md              # // stage: N tag convention
```

No state directory. No reviews directory. The agents are pure
on-demand dispatch.

### Anchor re-binding (aether → nightdrive)

| Aether anchor                          | Nightdrive equivalent                                              |
|----------------------------------------|---------------------------------------------------------------------|
| `docs/ROADMAP_V2.md` (P6–P10 items)    | `HANDOFF.md` §5 (stages 0-8) + §9 (12 build-order items)            |
| `// roadmap: P7.3` in `.aether` tests   | `// stage: N` in Rust `#[tokio::test]` fns under `tests/witnesses/` |
| `scripts/audit.ps1`                     | `scripts/audit.ps1` — runs `cargo check`, `cargo test`, stub-grep   |
| `docs/BENCH_LEDGER.md`                  | Same file. Columns: `date | sha7 | stage | track_id | wall_s | gpu_vram_peak_mib | leader` |
| `runtime/src/{lib,cuda}.rs` (op surface)| `crates/*/src/lib.rs` × pipeline-stage × test-type matrix          |

### Per-agent specs (terse — full text lives in the agent .md files)

**bench-runner.** Runs `bench/pipeline_one_track/run_all.ps1` for a
fixed `track_id`, times each stage (LLM spec gen, Stable Audio segment
render, ffmpeg loudnorm, wgpu visualizer render, ffmpeg final mux,
YouTube upload bytes/sec), captures GPU VRAM peak per stage. Appends
one row per stage to `docs/BENCH_LEDGER.md`. Aborts and escalates on
≥10% regression on a HEADLINE stage (audio-gen wall-clock or
final-encode size). Tools: `Read, Write, Edit, Bash, Glob, Grep`.

**honesty-auditor.** Cross-references claims (HANDOFF/CLAUDE/README
text, commit messages, agent reports) against actual code. Standard
checks: stage-implemented claims grep for `warn!("not yet
implemented")` / `todo!()` / `unimplemented!()` / `// TODO(nightdrive)`
in the relevant crate; published-track claims check `tracks.sqlite`
for the `uploads` row. Verdicts: ✅ VERIFIED with `<file>:<line>`, ⚠️
PARTIAL with the limit, ❌ FALSE with a citation, 🤷 UNVERIFIABLE.
Day-one targets two known drift items: HANDOFF.md §7 schema vs.
`20260510000000_init.sql` column-name diff; HANDOFF.md §3 "supermicro
8× P40" hardware claim vs. fleet reality. Tools: `Read, Glob, Grep, Bash`.

**roadmap-tracker.** Reads HANDOFF.md §5 (stages) + §9 (build order) +
§10 (revenue milestones), greps `tests/witnesses/` for `// stage: N`
markers, runs `scripts/audit.ps1` for fresh numbers. Reports:
witnessed/total per stage, top-3 next items by `(deps-clear, value,
effort)` score, blockers, "claimed-but-not-witnessed" sub-section.
Cap 500 words. Read-only — never edits files. Tools: `Read, Glob, Grep, Bash`.

**witness-test-author.** Given a stage ID (e.g. "stage 4 mastering"),
drafts ONE `tests/witnesses/<descriptive_name>.rs` that hits the real
endpoint — Ollama on kokonoe, Stable Audio Open on cnc (post-P100),
ffmpeg subprocess, YouTube upload sandbox. NEVER mocks. Tag with `//
stage: N`. STOPs and reports the gap if the underlying crate doesn't
compile yet. Test under 100 LOC; if larger, push back to caller for
staged landing. Tools: `Read, Write, Glob, Grep`.

**coverage-matrix.** Produces a markdown table:

```
| crate                    | stage | unit | integration | witness | bench |
| nightdrive-llm           |   1   |  ✓   |     ✓       |    ✓    |   -   |
| nightdrive-audio-gen     |   2   |  ✓   |     -       |    -    |   ✓   |
| nightdrive-art           |   3   |  -   |     -       |    -    |   -   |
...
```

Plus summary: total cells, covered, %, gaps grouped by stage. Cap
~3000 chars. Tools: `Read, Glob, Grep`.

### `// stage: N` convention

Stages from HANDOFF.md §5 are 0-8:

| N | Stage          | Crate touched primarily        |
|---|----------------|--------------------------------|
| 0 | Trigger        | `nightdrive-orchestrator`      |
| 1 | Composition    | `nightdrive-llm`               |
| 2 | Audio gen      | `nightdrive-audio-gen`         |
| 3 | Cover art      | `nightdrive-art`               |
| 4 | Mastering      | `nightdrive-audio-master`      |
| 5 | Visualizer     | `nightdrive-visuals`           |
| 6 | Final encode   | `nightdrive-encoder`           |
| 7 | Publish        | `nightdrive-youtube`           |
| 8 | Livestream     | `nightdrive-orchestrator`      |

Witness test header convention:

```rust
// stage: 4
// expect: master.flac written, target_lufs within ±0.5 of -14.0
// requires: ffmpeg (any recent version with loudnorm)
//
// Proves the loudnorm two-pass chain in nightdrive-audio-master::run
// produces a master.flac that measures within tolerance of the
// configured target. Hits real ffmpeg via subprocess; no mocks.
#[tokio::test]
async fn ...
```

### `scripts/audit.ps1` contract

Sections (numbered like aether's):
1. **build** — `cargo check --workspace 2>&1 | grep -c "^error"` → 0
2. **test** — `cargo test --workspace 2>&1` → all green
3. **stub inventory** — grep `crates/*/src/` for `todo!\(` / `unimplemented!\(` / `warn!\("not yet implemented"\)` / `// TODO\(nightdrive\)` — count per crate
4. **stage witnesses** — count `// stage: N` markers under `tests/witnesses/`, group by stage
5. **bench freshness** — last row in `BENCH_LEDGER.md`; warn if older than 7 days when stages 1-7 are all witnessed
6. **drift** — diff HANDOFF.md schema §7 against `20260510000000_init.sql` column names
7. **summary** — `OK - audit clean (build:0 test:N stubs:M witnesses:K)` or `FAIL - <list>`

Every section ends with `[N/7]` so honesty-auditor and roadmap-tracker
can quote the count verbatim.

### `docs/BENCH_LEDGER.md` initial format

```
# nightdrive bench ledger

Hardware (locked): cnc-server post-P100 upgrade — 3× Tesla P100 16GB,
fp32 inference, fixed seed=1010. Pre-upgrade rows: kokonoe RTX 3070
Ti 8GB, fp16. Hardware change → reset baseline.

Append-only. Never edit historical rows.

| date       | sha7    | stage | track_id          | wall_s | vram_peak_mib | leader  |
|------------|---------|-------|-------------------|--------|---------------|---------|
```

## Bootstrap plan (what gets created in this implementation pass)

1. `.claude/agents/{bench-runner,honesty-auditor,roadmap-tracker,witness-test-author,coverage-matrix}.md` — five agent prompts, ported from aether with anchors re-bound.
2. `scripts/audit.ps1` — the 7-section gate.
3. `docs/BENCH_LEDGER.md` — empty table with header + hardware caption.
4. `bench/pipeline_one_track/run_all.ps1` — skeleton; documents the contract, no-op until stages 1-7 have real implementations.
5. `tests/witnesses/README.md` — `// stage: N` tag convention + the no-mocks rule.
6. Update nightdrive `CLAUDE.md` — add a "Discipline stack" subsection pointing to the agents and the audit script.

## Triggering

On-demand only. Parent agents (Matt, sessions) dispatch by name. No
auto-fire on commit / merge. The `description` field on each agent
tells the parent when to invoke (e.g. honesty-auditor's description
says "Use before any external claim about nightdrive..."). Aether
validated this model — adding cursored auto-dispatch is unnecessary
ceremony.

## Out of scope

- `.claude/state/` cursors and `.claude/reviews/` finding logs
  (Pledge-and-Crown style). Aether doesn't have them; we're matching
  aether.
- Slack/Telegram notifications from agents. Matt's global `/notify`
  skill is the escalation path; agents return findings in-band.
- CI integration. Per global rule, GitHub Actions is banned; audit.ps1
  is the local gate.
- Auto-update of HANDOFF.md by roadmap-tracker. It's read-only —
  recommendations only. Matt updates HANDOFF.md based on the report.

## Open questions

None blocking implementation. Two to revisit later:

1. Once cnc has the P100s and audio-gen actually runs, decide whether
   `bench-runner` benches per-card (fanout) or single-card.
2. If the agents start producing consistent multi-page reports, decide
   whether to add a `.claude/reviews/` log after all. Don't pre-build it.

## Success criteria

- All 5 agent files lint clean (frontmatter parses, body coherent).
- `scripts/audit.ps1` runs end-to-end on the current scaffold and
  emits sections [1/7]…[7/7] with no ps1 errors (most sections will
  report 0 / not-yet, which is correct).
- `docs/BENCH_LEDGER.md` exists with header.
- nightdrive `CLAUDE.md` references the discipline stack.
- A test invocation of `honesty-auditor` against a hand-fed claim list
  including the two known drift items returns ❌ FALSE with citations.
