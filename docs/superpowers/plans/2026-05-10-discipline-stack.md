# Discipline-Stack Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** ship 5 on-demand subagents (bench-runner, honesty-auditor, roadmap-tracker, witness-test-author, coverage-matrix) plus the 4 anchor artifacts they consume (`scripts/audit.ps1`, `docs/BENCH_LEDGER.md`, `bench/pipeline_one_track/run_all.ps1`, `tests/witnesses/README.md`), and a `CLAUDE.md` cross-reference. Mirrors `J:\aether\.claude\agents\` exactly with anchors re-bound to nightdrive's domain.

**Architecture:** Each agent is a Claude Code subagent — a `.md` file with YAML frontmatter (`name`, `description`, `tools`) and a system-prompt body. The parent agent dispatches by name when the user's intent matches the `description`. Output flows back to the caller in-band; no `state/` cursors, no `reviews/` log directory. The 5 agents share a closed feedback loop: `roadmap-tracker` says what to attack → `witness-test-author` drafts the test → `bench-runner` times it → `coverage-matrix` shows the gap closed → `honesty-auditor` verifies the claim before any external statement.

**Tech Stack:** Markdown + YAML frontmatter (agent files), PowerShell 5.1 with `-ExecutionPolicy Bypass` (audit.ps1 + bench/run_all.ps1), Markdown (BENCH_LEDGER.md, tests/witnesses/README.md). No new Rust code in this pass — the agents operate on the existing scaffold.

**Source pattern:** `J:\aether\.claude\agents\` — five validated agents Matt's been using on the aether project. Read each one before porting; the `description` field, the `tools` whitelist, the section structure (Inputs / What you produce / Rules / Failure modes / Output format) are all load-bearing.

**Anchor re-binding (memorize this — it's referenced in every agent task):**

| aether anchor                          | nightdrive equivalent                                              |
|----------------------------------------|---------------------------------------------------------------------|
| `J:\aether\` (project root)            | `J:\nightdrive\`                                                    |
| `docs/ROADMAP_V2.md`                   | `docs/ROADMAP.md` (already shipped 2026-05-10)                      |
| `// roadmap: P7.3` in `.aether` tests   | `// stage: N` in Rust tests under `tests/witnesses/`                |
| `tests/runtime/*.aether`                | `tests/witnesses/*.rs` (Rust integration tests, no mocks)           |
| `scripts/audit.ps1` (Rust binary wrap)  | `scripts/audit.ps1` (pure PowerShell, no aether-audit.exe)          |
| `docs/BENCH_LEDGER.md`                  | Same path; columns: date, sha7, stage, track_id, wall_s, vram_peak_mib, leader |
| `runtime/src/{lib,cuda}.rs` (op surface)| `crates/*/src/lib.rs` × pipeline-stage × test-type                  |
| `bench/matmul_micro/run_all.ps1`        | `bench/pipeline_one_track/run_all.ps1`                              |
| Phases P6-P10 with items P7.3, P6.4    | Phases N1-N5 with items N1.4, N2.6, N3.1                            |
| `cargo build -p aetherc -q`             | `cargo check --workspace`                                            |
| Aether-specific build/test patterns    | nightdrive Rust 2024 edition + sqlx + tokio                          |

---

## Task 1: Create directory skeleton

**Files:**
- Create dir: `.claude/agents/`
- Create dir: `scripts/`
- Create dir: `bench/pipeline_one_track/`
- Create dir: `tests/witnesses/`
- Create dir: `docs/` (already exists from roadmap pass; verify)

- [ ] **Step 1: Create the four new directories**

```bash
mkdir -p /j/nightdrive/.claude/agents
mkdir -p /j/nightdrive/scripts
mkdir -p /j/nightdrive/bench/pipeline_one_track
mkdir -p /j/nightdrive/tests/witnesses
```

- [ ] **Step 2: Verify the layout**

Run: `ls -la /j/nightdrive/.claude/agents /j/nightdrive/scripts /j/nightdrive/bench/pipeline_one_track /j/nightdrive/tests/witnesses`
Expected: each dir exists and is empty (besides `./` and `../`).

---

## Task 2: Create `scripts/audit.ps1` — the 7-section gate

**Files:**
- Create: `scripts/audit.ps1`
- Test: manual run on the current scaffold

**Why first:** the agents reference this script in their bodies. It needs to exist before the agents can be invoked meaningfully. Aether's audit.ps1 wraps a `aether-audit.exe` Rust binary; nightdrive doesn't have that, so this is pure PowerShell that runs the section logic inline.

- [ ] **Step 1: Write `scripts/audit.ps1`**

Create the file with the following content (PowerShell 5.1 syntax — no pipeline-chain operators, no ternary):

```powershell
# scripts/audit.ps1 -- nightdrive single-command discipline gate.
#
# Sections (numbered [N/7] so honesty-auditor and roadmap-tracker can
# quote counts verbatim):
#   [1/7] build         -- cargo check --workspace, error count
#   [2/7] test          -- cargo test --workspace, pass count
#   [3/7] stub inventory-- todo!() / unimplemented!() / warn!("not yet implemented") / // TODO(nightdrive)
#   [4/7] stage witnesses- count // stage: N markers under tests/witnesses/
#   [5/7] bench freshness- last row in BENCH_LEDGER.md vs. 7-day rule
#   [6/7] schema drift  -- HANDOFF.md section 7 schema vs. 20260510000000_init.sql column names
#   [7/7] summary       -- "OK - audit clean" or "FAIL - <list>"
#
# Pure PowerShell -- no aether-audit.exe equivalent (yet). Emits final exit
# code 0 on clean, 1 on any FAIL section, 2 on script error.

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

$failures = @()

# --- [1/7] build ----------------------------------------------------------
Write-Host "==> [1/7] build"
$prev = $ErrorActionPreference; $ErrorActionPreference = "Continue"
$buildOutput = & cargo check --workspace --quiet 2>&1
$buildErrors = ($buildOutput | Select-String -Pattern "^error" -CaseSensitive).Count
$ErrorActionPreference = $prev
Write-Host "    cargo check errors: $buildErrors"
if ($buildErrors -gt 0) { $failures += "build:$buildErrors" }

# --- [2/7] test -----------------------------------------------------------
Write-Host "==> [2/7] test"
$ErrorActionPreference = "Continue"
$testOutput = & cargo test --workspace --quiet 2>&1
$testFails = ($testOutput | Select-String -Pattern "^test result: FAILED" -CaseSensitive).Count
$ErrorActionPreference = $prev
Write-Host "    cargo test failed-suites: $testFails"
if ($testFails -gt 0) { $failures += "test:$testFails" }

# --- [3/7] stub inventory ------------------------------------------------
Write-Host "==> [3/7] stub inventory"
$stubPatterns = @(
    'todo!\(',
    'unimplemented!\(',
    'unreachable!\(',
    'warn!\("not yet implemented"',
    '// TODO\(nightdrive\)'
)
$stubCount = 0
$stubByFile = @{}
if (Test-Path "crates") {
    $rsFiles = Get-ChildItem -Path crates -Recurse -Filter "*.rs" -ErrorAction SilentlyContinue
    foreach ($file in $rsFiles) {
        $content = Get-Content -Path $file.FullName -Raw -ErrorAction SilentlyContinue
        if ($null -eq $content) { continue }
        foreach ($pat in $stubPatterns) {
            $hits = [regex]::Matches($content, $pat).Count
            if ($hits -gt 0) {
                $stubCount += $hits
                $rel = $file.FullName.Substring($root.Length + 1)
                if (-not $stubByFile.ContainsKey($rel)) { $stubByFile[$rel] = 0 }
                $stubByFile[$rel] += $hits
            }
        }
    }
}
Write-Host "    stubs: $stubCount across $($stubByFile.Count) file(s)"
foreach ($k in $stubByFile.Keys) { Write-Host "      $k : $($stubByFile[$k])" }

# --- [4/7] stage witnesses -----------------------------------------------
Write-Host "==> [4/7] stage witnesses"
$witnessByStage = @{}
$witnessTotal = 0
if (Test-Path "tests/witnesses") {
    $witnessFiles = Get-ChildItem -Path tests/witnesses -Recurse -Filter "*.rs" -ErrorAction SilentlyContinue
    foreach ($file in $witnessFiles) {
        $content = Get-Content -Path $file.FullName -ErrorAction SilentlyContinue
        if ($null -eq $content) { continue }
        $head = $content | Select-Object -First 10
        foreach ($line in $head) {
            $m = [regex]::Match($line, '//\s*stage:\s*(\d+)')
            if ($m.Success) {
                $stage = [int]$m.Groups[1].Value
                if (-not $witnessByStage.ContainsKey($stage)) { $witnessByStage[$stage] = 0 }
                $witnessByStage[$stage] += 1
                $witnessTotal += 1
            }
        }
    }
}
Write-Host "    witnesses: $witnessTotal across stages: $(($witnessByStage.Keys | Sort-Object) -join ',')"

# --- [5/7] bench freshness -----------------------------------------------
Write-Host "==> [5/7] bench freshness"
$benchStale = $false
if (Test-Path "docs/BENCH_LEDGER.md") {
    $benchLines = Get-Content "docs/BENCH_LEDGER.md" -ErrorAction SilentlyContinue
    $dataRows = $benchLines | Where-Object { $_ -match '^\| 20\d{2}-' }
    if ($dataRows.Count -eq 0) {
        Write-Host "    no bench rows yet (ok during scaffold)"
    } else {
        $lastRow = $dataRows[-1]
        $lastDateMatch = [regex]::Match($lastRow, '\| (20\d{2}-\d{2}-\d{2}) ')
        if ($lastDateMatch.Success) {
            $lastDate = [datetime]::ParseExact($lastDateMatch.Groups[1].Value, "yyyy-MM-dd", $null)
            $age = ([datetime]::Now - $lastDate).Days
            Write-Host "    last bench row: $($lastDateMatch.Groups[1].Value) ($age days old)"
            if ($age -gt 7 -and $witnessTotal -ge 7) {
                $benchStale = $true
                $failures += "bench-stale:$($age)d"
            }
        }
    }
} else {
    Write-Host "    BENCH_LEDGER.md missing (ok before first bench run)"
}

# --- [6/7] schema drift --------------------------------------------------
Write-Host "==> [6/7] schema drift (HANDOFF.md section 7 vs 20260510000000_init.sql)"
$driftFindings = @()
if ((Test-Path "HANDOFF.md") -and (Test-Path "20260510000000_init.sql")) {
    $sqlContent = Get-Content "20260510000000_init.sql" -Raw
    # Pull column names from the tracks table CREATE statement.
    $tracksMatch = [regex]::Match($sqlContent, 'CREATE TABLE IF NOT EXISTS tracks \((.*?)\);', 'Singleline')
    if ($tracksMatch.Success) {
        $sqlCols = [regex]::Matches($tracksMatch.Groups[1].Value, '^\s+(\w+)\s+', 'Multiline') |
            ForEach-Object { $_.Groups[1].Value } |
            Where-Object { $_ -notin @('CREATE','PRIMARY','FOREIGN','REFERENCES','DEFAULT') }
        # Pull column names from HANDOFF.md section 7's tracks block.
        $handoffContent = Get-Content "HANDOFF.md" -Raw
        $section7Match = [regex]::Match($handoffContent, 'CREATE TABLE tracks \((.*?)\);', 'Singleline')
        if ($section7Match.Success) {
            $handoffCols = [regex]::Matches($section7Match.Groups[1].Value, '^\s+(\w+)\s+', 'Multiline') |
                ForEach-Object { $_.Groups[1].Value } |
                Where-Object { $_ -notin @('CREATE','PRIMARY','FOREIGN','REFERENCES','DEFAULT') }
            $sqlOnly = $sqlCols | Where-Object { $handoffCols -notcontains $_ }
            $handoffOnly = $handoffCols | Where-Object { $sqlCols -notcontains $_ }
            if ($sqlOnly.Count -gt 0) { $driftFindings += "sql-only: $($sqlOnly -join ',')" }
            if ($handoffOnly.Count -gt 0) { $driftFindings += "handoff-only: $($handoffOnly -join ',')" }
        }
    }
}
if ($driftFindings.Count -gt 0) {
    Write-Host "    DRIFT: $($driftFindings -join ' | ')"
    $failures += "drift"
} else {
    Write-Host "    no schema drift"
}

# --- [7/7] summary --------------------------------------------------------
Write-Host "==> [7/7] summary"
if ($failures.Count -eq 0) {
    Write-Host "OK - audit clean (build:$buildErrors test:$testFails stubs:$stubCount witnesses:$witnessTotal)"
    exit 0
} else {
    Write-Host "FAIL - $($failures -join ', ')"
    exit 1
}
```

- [ ] **Step 2: Run audit.ps1 against the current scaffold**

Run: `powershell -ExecutionPolicy Bypass -File /j/nightdrive/scripts/audit.ps1`
Expected output (current scaffold has no `crates/` dir yet, no tests/witnesses/, no BENCH_LEDGER.md, but does have HANDOFF.md + 20260510000000_init.sql):
- `[1/7]` cargo check returns errors (no Cargo.toml workspace yet at root level for `cargo check --workspace` — OR if Cargo.toml is in place but no crates, build:0). Expected: build:0 if scaffold is reshufflled, otherwise non-zero — note count.
- `[2/7]` cargo test similar
- `[3/7]` stubs:0 (no `crates/` dir)
- `[4/7]` witnesses:0
- `[5/7]` no bench rows yet
- `[6/7]` should report `DRIFT` (HANDOFF.md uses `subgenre, bpm, musical_key, audio_path, ...`; SQL uses `seed, visualizer_path, duration_secs`) — confirms section 6 actually catches the known drift item
- `[7/7]` `FAIL - drift` (and possibly build/test counts)

The drift detection is the headline success: section [6/7] must surface the known HANDOFF/migration mismatch documented in `CLAUDE.md`.

- [ ] **Step 3: If section [6/7] does NOT report DRIFT, debug the regex**

The drift detection depends on both `HANDOFF.md` containing the literal string `CREATE TABLE tracks (` in its section 7 prose, and on `20260510000000_init.sql` containing `CREATE TABLE IF NOT EXISTS tracks (`. Both confirmed present from the earlier read pass.

If the script reports "no schema drift" but the drift is real (per `CLAUDE.md` "Schema drift" warning), the regex needs adjustment. Iterate until drift is detected.

---

## Task 3: Create `docs/BENCH_LEDGER.md`

**Files:**
- Create: `docs/BENCH_LEDGER.md`

- [ ] **Step 1: Write the file**

```markdown
# nightdrive bench ledger

**Hardware (locked):**
- **Pre-2026-05-17:** kokonoe RTX 3070 Ti 8 GB, fp16 (Stable Audio Open peaks ~6-7 GB).
- **Post-2026-05-17:** cnc-server 3× Tesla P100 16 GB, fp32 (Pascal sm_60 has no fp16 acceleration; per `J:\llm-wiki\patterns\candle-p100-pascal-compat.md`).

**Hardware change resets baseline.** Do not compare rows across different
hardware. When the cnc P100s land, write a horizontal rule and start a fresh
table with a new caption.

**Append-only.** Never edit historical rows. If a stage is re-baselined, append a new row with a `note` flag — don't overwrite.

**Columns:**
- `date`        — YYYY-MM-DD UTC
- `sha7`        — short git sha; `(head)` if not yet a git repo
- `stage`       — N from `HANDOFF.md` §5 (0..8) or `pipeline_full` for end-to-end
- `track_id`    — fixed seed for reproducibility (default seed=1010, BPM=92, 240s)
- `wall_s`      — wall-clock seconds
- `vram_peak_mib` — peak GPU VRAM during the stage; `-` for CPU-only
- `leader`      — `nightdrive` for self-hosted runs, or comparator name on multi-system benches
- `note`        — optional free-form (regression flags, hardware notes)

---

## kokonoe RTX 3070 Ti 8 GB, fp16 (pre-cnc-P100 baseline)

| date       | sha7    | stage         | track_id          | wall_s | vram_peak_mib | leader     | note |
|------------|---------|---------------|-------------------|-------:|--------------:|------------|------|
```

- [ ] **Step 2: Verify**

Run: `cat /j/nightdrive/docs/BENCH_LEDGER.md | head -30`
Expected: header + hardware caption + columns + empty table block.

---

## Task 4: Create `bench/pipeline_one_track/run_all.ps1` (skeleton)

**Files:**
- Create: `bench/pipeline_one_track/run_all.ps1`

- [ ] **Step 1: Write the skeleton**

```powershell
# bench/pipeline_one_track/run_all.ps1 -- nightdrive end-to-end pipeline
# bench. Times each stage of one full pipeline run on a fixed track_id and
# appends one row per stage to docs/BENCH_LEDGER.md.
#
# Until Phase N1 ships (see docs/ROADMAP.md), most stages are no-ops and
# the script just verifies its own contract: it runs without ps1 errors
# and writes a "scaffold" row to the ledger. Once a stage's crate ships,
# replace the corresponding placeholder timing block with the real call.
#
# Default config (matches the spec's reproducibility lock):
#   track_id = nd-bench-001
#   seed     = 1010
#   bpm      = 92
#   duration = 240s
#
# Wall-time budget: the full pipeline should complete in under 10 minutes
# on cnc post-P100. If it's slower, audit.ps1 [5/7] will flag it.

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
Set-Location $root

$ledger = Join-Path $root "docs\BENCH_LEDGER.md"
if (-not (Test-Path $ledger)) {
    throw "BENCH_LEDGER.md missing at $ledger -- run Task 3 first"
}

$today = (Get-Date).ToString("yyyy-MM-dd")
$sha7 = "(head)"  # replace with `git rev-parse --short HEAD` once nightdrive is a git repo

$trackId = "nd-bench-001"
Write-Host "==> bench/pipeline_one_track on track_id=$trackId"

$stages = @(
    @{ id = 1; name = "llm_spec_gen"     ; crate = "nightdrive-llm"            }
    @{ id = 2; name = "audio_gen"        ; crate = "nightdrive-audio-gen"      }
    @{ id = 3; name = "cover_art"        ; crate = "nightdrive-art"            }
    @{ id = 4; name = "audio_master"     ; crate = "nightdrive-audio-master"   }
    @{ id = 5; name = "visualizer"       ; crate = "nightdrive-visuals"        }
    @{ id = 6; name = "final_encode"     ; crate = "nightdrive-encoder"        }
    @{ id = 7; name = "youtube_upload"   ; crate = "nightdrive-youtube"        }
)

foreach ($s in $stages) {
    $cratePath = Join-Path $root "crates\$($s.crate)\src\lib.rs"
    if (-not (Test-Path $cratePath)) {
        Write-Host "    [stage $($s.id) $($s.name)] crate not yet shipped (skip)"
        $row = "| $today | $sha7 | $($s.id) | $trackId | - | - | nightdrive | crate-not-shipped |"
        Add-Content -Path $ledger -Value $row -Encoding utf8
        continue
    }
    # Real timing logic lands when the crate's ship-task in ROADMAP.md is checked.
    Write-Host "    [stage $($s.id) $($s.name)] crate present; timing not yet implemented"
    $row = "| $today | $sha7 | $($s.id) | $trackId | - | - | nightdrive | timing-not-yet-implemented |"
    Add-Content -Path $ledger -Value $row -Encoding utf8
}

Write-Host "OK - bench complete (skeleton mode); rows appended to $ledger"
```

- [ ] **Step 2: Run the skeleton bench**

Run: `powershell -ExecutionPolicy Bypass -File /j/nightdrive/bench/pipeline_one_track/run_all.ps1`
Expected: 7 lines of "crate not yet shipped (skip)" + 7 rows appended to `docs/BENCH_LEDGER.md` with `crate-not-shipped` notes. Final line "OK - bench complete (skeleton mode); rows appended to ...".

- [ ] **Step 3: Verify ledger rows**

Run: `tail -10 /j/nightdrive/docs/BENCH_LEDGER.md`
Expected: 7 rows with `| 2026-05-10 | (head) | <N> | nd-bench-001 | - | - | nightdrive | crate-not-shipped |` for stages 1-7.

---

## Task 5: Create `tests/witnesses/README.md`

**Files:**
- Create: `tests/witnesses/README.md`

- [ ] **Step 1: Write the file**

```markdown
# tests/witnesses/

Real-endpoint integration tests. Every file in this directory tags itself
with `// stage: N` matching `HANDOFF.md` §5 (stages 0-8). The
`coverage-matrix` agent reads these tags; `roadmap-tracker` counts them
per phase; `audit.ps1` section [4/7] reports the per-stage totals;
`witness-test-author` produces new ones on demand.

## The `// stage: N` convention

Every witness test starts with this header (in the first 10 lines of the
file so the audit grep finds it):

```rust
// stage: 4
// expect: master.flac written, target_lufs within ±0.5 of -14.0
// requires: ffmpeg (any recent version with loudnorm filter)
//
// Proves the loudnorm two-pass chain in nightdrive-audio-master::run
// produces a master.flac that measures within tolerance of the
// configured target. Hits real ffmpeg via subprocess; no mocks.
#[tokio::test]
async fn loudnorm_hits_target() {
    // ...
}
```

Stage table (mirrors `HANDOFF.md` §5):

| N | Stage              | Primary crate              |
|---|--------------------|----------------------------|
| 0 | Trigger / pipeline | `nightdrive-orchestrator`  |
| 1 | Composition spec   | `nightdrive-llm`           |
| 2 | Audio generation   | `nightdrive-audio-gen`     |
| 3 | Cover art          | `nightdrive-art`           |
| 4 | Audio mastering    | `nightdrive-audio-master`  |
| 5 | Visualizer         | `nightdrive-visuals`       |
| 6 | Final encode       | `nightdrive-encoder`       |
| 7 | Publish            | `nightdrive-youtube`       |
| 8 | Livestream         | `nightdrive-orchestrator`  |

A test that legitimately exercises multiple stages tags with the
**latest** stage it observes. A test that proves cross-stage glue
(e.g. orchestrator dispatch) tags `// stage: 0`.

## The no-mocks rule

Witness tests **hit the real endpoint**. No `mockito`, no `wiremock`, no
in-process fakes. Specifically:

- LLM tests hit real Ollama on kokonoe at the URL from
  `$NIGHTDRIVE_OPENCLAW_URL`.
- Audio-gen tests hit the real Stable Audio Open sidecar on cnc.
- Mastering / encoder tests spawn real `ffmpeg` subprocess.
- YouTube tests upload to a real (private) YouTube video resource and
  delete it afterward.
- Storage tests use real SQLite on a tempdir DB.

Tests that can't reach their endpoint mark themselves `#[ignore]` with a
reason, so a developer without the network access (or without cnc
P100s) can still `cargo test` and have the suite pass:

```rust
#[tokio::test]
#[cfg_attr(not(feature = "live-audio-sidecar"), ignore = "needs cnc:8080 reachable")]
async fn audio_sidecar_health() { ... }
```

The exception: `Task N4.1 retry` policy tests use a local mock-server
because the failure modes (transient 500s in a controlled order) can't
be reliably reproduced against a real endpoint. This is the only
documented exception. Cite this README in the test if you add another.

## Why no mocks (the user's burned-by lesson)

Per Matt's global feedback: "we got burned last quarter when mocked
tests passed but the prod migration failed." Mocks drift from reality;
real-endpoint tests catch the drift. The cost (test latency, network
flake, occasional skips) is worth it.

## Cross-references

- `docs/ROADMAP.md` — every numbered roadmap item lists its expected witness file.
- `.claude/agents/witness-test-author.md` — the agent that drafts these.
- `scripts/audit.ps1` section [4/7] — counts the tagged tests.
```

- [ ] **Step 2: Verify**

Run: `cat /j/nightdrive/tests/witnesses/README.md | head -30`
Expected: title, stage table, and the no-mocks rule visible.

---

## Task 6: Create `.claude/agents/bench-runner.md`

**Files:**
- Read: `J:\aether\.claude\agents\bench-runner.md` (source pattern)
- Create: `J:\nightdrive\.claude\agents\bench-runner.md`

**Substitution map** (apply to the aether file):

| aether                                                          | nightdrive                                                              |
|-----------------------------------------------------------------|--------------------------------------------------------------------------|
| `Aether's standing benches against Candle + PyTorch + (when applicable) hand-tuned reference` | `nightdrive's pipeline-stage timings (LLM spec gen, audio-gen segment, mastering, visualizer render, final encode, YouTube upload)` |
| `runtime/src/cuda.rs, runtime/src/lib.rs, compiler/src/codegen/asm/, or compiler/src/mir/fuse.rs` | `crates/nightdrive-audio-gen/src/, crates/nightdrive-audio-master/src/, crates/nightdrive-encoder/src/, crates/nightdrive-visuals/src/` |
| `bench/<name>/run_all.ps1` umbrella runner                      | `bench/pipeline_one_track/run_all.ps1` umbrella runner                  |
| Bench name examples (`matmul_micro`, `all`)                     | `pipeline_one_track`, `audio_gen_segment_chain`, `visuals_render_minute`|
| ledger row: `\| date \| sha7 \| bench \| config \| aether \| candle \| torch \| leader \|` | ledger row: `\| date \| sha7 \| stage \| track_id \| wall_s \| vram_peak_mib \| leader \| note \|` |
| Hardware: `i9-11900K + 3070 Ti`                                  | Hardware: pre-2026-05-17 `kokonoe RTX 3070 Ti 8 GB fp16`; post: `cnc 3× Tesla P100 16 GB fp32` |
| 10-min wall-time gate                                            | Same 10-min gate (per ROADMAP.md cross-cutting rule 8)                  |
| `cargo build --workspace 2>&1 \| grep "^error"` precondition    | Same — keep verbatim                                                     |
| Failure modes: `nvtop` GPU contention, PyTorch warm-up, etc.    | Same shape, but examples become: GPU contention via `nvidia-smi`, audio-gen warm-up across segments, ffmpeg threading flag mismatches |
| Escalation: ≥10% regression on `matmul 256³ or 512³ GPU`        | ≥10% regression on a HEADLINE stage: `audio_gen` wall-clock OR `final_encode` MP4 size OR `youtube_upload` bytes/sec |

- [ ] **Step 1: Read the aether source**

Run: `cat /j/aether/.claude/agents/bench-runner.md`
Read the full file (~50 lines). Note the frontmatter and 4 body sections (Inputs / What you produce / Rules / Failure modes / When to escalate).

- [ ] **Step 2: Write the nightdrive version**

Apply the substitution map to produce `J:\nightdrive\.claude\agents\bench-runner.md`. Frontmatter must have:

```yaml
---
name: bench-runner
description: Use this subagent to time nightdrive's pipeline stages (LLM spec gen, audio-gen segment chain, mastering, visualizer render, final encode, YouTube upload), normalize the numbers, and append rows to docs/BENCH_LEDGER.md. Invoke after any commit that touches crates/nightdrive-audio-gen/, crates/nightdrive-audio-master/, crates/nightdrive-encoder/, or crates/nightdrive-visuals/. The audit.ps1 [5/7] bench-freshness check will demand a fresh row from this agent before letting a perf-relevant commit through.
tools: Read, Write, Edit, Bash, Glob, Grep
---
```

Body sections retain aether's structure. Critical re-bindings:
- "Inputs" lists `bench/pipeline_one_track/run_all.ps1` as the umbrella runner; current `docs/BENCH_LEDGER.md`; `git rev-parse --short HEAD` for sha (or `(head)` if not yet a git repo per `CLAUDE.md`).
- "What you produce" shows the new column shape.
- "Rules" keeps "NEVER massage numbers" verbatim — this is the load-bearing honesty rule.
- Hardware caption rule: "When cnc has the P100s and audio-gen actually runs, every row of the new baseline goes under a fresh `## cnc 3× Tesla P100` heading; never compare across hardware blocks."
- "When to escalate" cites the new headline regressions: audio-gen wall-clock, final-encode MP4 size, youtube upload bytes/sec.

- [ ] **Step 3: Verify frontmatter parses**

Run: `head -5 /j/nightdrive/.claude/agents/bench-runner.md`
Expected: 5 lines, three `---` markers (lines 1, 4 OR later) with `name:`, `description:`, `tools:` fields. The description ≥ 200 chars (so the dispatcher can match intent specifically). YAML is well-formed.

---

## Task 7: Create `.claude/agents/honesty-auditor.md`

**Files:**
- Read: `J:\aether\.claude\agents\honesty-auditor.md`
- Create: `J:\nightdrive\.claude\agents\honesty-auditor.md`

**Substitution map:**

| aether                                                     | nightdrive                                                     |
|------------------------------------------------------------|-----------------------------------------------------------------|
| `J:\aether\` repo references                               | `J:\nightdrive\`                                                |
| `cargo build -p aetherc` precondition                      | `cargo check --workspace`                                       |
| `target/debug/aetherc.exe tests/runtime/<X>.aether ...`    | `cargo test --workspace --test <X>` for Rust witness tests; OR `powershell -ExecutionPolicy Bypass -File scripts/audit.ps1` for the gate |
| "Aether is faster than Candle/PyTorch at Z" claim          | "nightdrive renders a track in under N seconds" / "audio-gen wall-clock improved by X%" |
| `docs/BENCH_LEDGER.md` per-iter µs comparison              | per-stage `wall_s` + `vram_peak_mib` comparison                |
| `scripts/audit.ps1` returns "OK - audit clean" with N count | Same — but n is "build:0 test:N stubs:M witnesses:K"           |
| Roadmap item PN.M is done                                  | Roadmap item N1.4 is done                                      |
| `tests/runtime/` for `// roadmap: PN.M` witness            | `tests/witnesses/` for `// stage: N` witness                   |
| `docs/ROADMAP_V2.md`                                       | `docs/ROADMAP.md`                                              |

**Day-one specific targets to bake into the description:**

The `description` field must mention the two known drift items so the dispatcher can wire it up immediately:
- `HANDOFF.md` §7 schema vs `20260510000000_init.sql` column-name diff (verifies `audit.ps1` [6/7]'s drift detection).
- `HANDOFF.md` §3 "supermicro 8× Tesla P40 192 GB VRAM" hardware claim vs `J:\llm-wiki\fleet\Fleet Overview.md` reality (returns ❌ FALSE; cites the wiki page).

- [ ] **Step 1: Read the aether source**

Run: `cat /j/aether/.claude/agents/honesty-auditor.md`

- [ ] **Step 2: Write the nightdrive version**

Frontmatter:

```yaml
---
name: honesty-auditor
description: Use this subagent before any external claim about nightdrive (telegram pings, README updates, bench results, marketing copy, HANDOFF.md edits). Cross-references each claim against actual code, docs, and the running audit — does the cited file exist? Does the named fn return what's claimed? Does the test exit clean? Does the hardware actually exist in J:\llm-wiki\fleet\? Returns yes/no per claim with citation. Burned by the ClaudioOS "boot-ready" incident — never claim a stub is shipped.
tools: Read, Glob, Grep, Bash
---
```

Body retains aether's section structure (Inputs / What you produce / Standard checks per claim type / Rules / Failure modes / Output format).

Standard checks adapted for nightdrive:
1. **"Test X passes / exits N"** — `cargo test --workspace --test <name>` and check exit code.
2. **"Feature Y works / Stage Z is implemented"** — grep `crates/<crate>/src/` for `todo!\(` / `unimplemented!\(` / `warn!\("not yet implemented"` / `// TODO\(nightdrive\)`. If any found in the claimed code path → ❌ FALSE.
3. **"Track published end-to-end"** — query the SQLite at `$NIGHTDRIVE_WORK_DIR/nightdrive.sqlite` for the upload row.
4. **"nightdrive does X faster than Y"** — read `docs/BENCH_LEDGER.md`; if no fresh row for that stage → 🤷 UNVERIFIABLE; recommend `bench-runner`.
5. **"Hardware X is in the fleet"** — read `J:\llm-wiki\fleet\Fleet Overview.md`; if not present → ❌ FALSE.
6. **"Roadmap item Nx.y is done"** — read `docs/ROADMAP.md` for the witness criterion; check `tests/witnesses/` for a file with `// stage: N` matching the item's stage; if no witness file → ❌ FALSE.

Day-one drift items in the body's "Initial calibration" section (one paragraph): when invoked without arguments, audit these two known claims first to verify the agent is wired correctly:
- "HANDOFF.md §7 schema matches the 20260510000000_init.sql migration" → expected ❌ FALSE.
- "supermicro with 8× Tesla P40 (192 GB VRAM) is the inference muscle box" → expected ❌ FALSE per Fleet Overview.

- [ ] **Step 3: Verify**

Run: `head -5 /j/nightdrive/.claude/agents/honesty-auditor.md`
Expected: frontmatter parses; description mentions the two day-one drift items so a future invoker has the wiring context.

---

## Task 8: Create `.claude/agents/roadmap-tracker.md`

**Files:**
- Read: `J:\aether\.claude\agents\roadmap-tracker.md`
- Create: `J:\nightdrive\.claude\agents\roadmap-tracker.md`

**Substitution map:**

| aether                                                  | nightdrive                                                          |
|---------------------------------------------------------|----------------------------------------------------------------------|
| `J:\aether\`                                             | `J:\nightdrive\`                                                    |
| `docs/ROADMAP_V2.md` 5 mega-phases (P6..P10)             | `docs/ROADMAP.md` 5 phases (N1..N5)                                 |
| Each `## N.M Title (EFFORT)` heading                     | Each `## NX.Y Title (EFFORT[, depends ...])` heading                |
| `tests/runtime/*.aether` with `// roadmap: P7.3, P10.6`  | `tests/witnesses/*.rs` with `// stage: N` (N=0..8)                  |
| `examples/*.aether`                                      | (no equivalent yet — strike that bullet)                            |
| `scripts/audit.ps1` section [7/7] for witnessed-count   | `scripts/audit.ps1` section [4/7] for witnessed-count per stage     |
| 74-test suite reference                                  | "growing witness suite under tests/witnesses/" (no fixed count yet) |
| CLAUDE.md historical critical-path 1-28                 | `HANDOFF.md` §9 (12-item build order) + §5 (stages 0-8)             |

- [ ] **Step 1: Read the aether source**

Run: `cat /j/aether/.claude/agents/roadmap-tracker.md`

- [ ] **Step 2: Write the nightdrive version**

Frontmatter:

```yaml
---
name: roadmap-tracker
description: Use this subagent to read the current state of nightdrive's roadmap (docs/ROADMAP.md + // stage:N witness markers in tests/witnesses/) and produce a status report — what's witnessed, what's missing, what to attack next per phase N1-N5. Invoke at session start, after a large feature lands, or before claiming progress to a stakeholder. Never writes code; pure analysis + reporting. Caps reports at 500 words.
tools: Read, Glob, Grep, Bash
---
```

Body keeps aether's structure exactly. The "Inputs you read" list:
1. `docs/ROADMAP.md` — phases N1..N5, ~50 numbered items.
2. `tests/witnesses/*.rs` — `// stage: N` markers in first 10 lines of each file.
3. `HANDOFF.md` §5 (stage table 0-8) + §9 (12-item build order) + §10 (revenue timeline).
4. `CLAUDE.md` for hard-rule context + the discipline-stack pointer.
5. `scripts/audit.ps1` output — section [4/7] gives the live per-stage witness count. Run via `powershell -ExecutionPolicy Bypass -File scripts/audit.ps1` and tail the last 40 lines.

"What you produce" — same 5 bullets (Headline / Top 5 / Blockers / Audit health / Recommendation), but Top 5 selection rules adapt:
- (a) lowest dependency count from `docs/ROADMAP.md` "Suggested ordering / parallelism" graph
- (b) highest user-visible value (closer to Phase N1.14 "first VOD" or N5.1 "first public" milestones)
- (c) smallest effort label (S > M > L > XL)

Rules retain "NEVER edit a file" verbatim. The "claimed-but-not-witnessed" sub-section rule applies: if HANDOFF.md or commit history claims an item shipped but no `// stage: N` matching test exists, flag it.

- [ ] **Step 3: Verify**

Run: `head -5 /j/nightdrive/.claude/agents/roadmap-tracker.md`
Expected: frontmatter parses; mentions phase IDs N1-N5 and the 500-word cap.

---

## Task 9: Create `.claude/agents/witness-test-author.md`

**Files:**
- Read: `J:\aether\.claude\agents\witness-test-author.md`
- Create: `J:\nightdrive\.claude\agents\witness-test-author.md`

**Substitution map:**

| aether                                                      | nightdrive                                                          |
|-------------------------------------------------------------|----------------------------------------------------------------------|
| Roadmap item ID `P7.3` / `P6.4` examples                    | Roadmap item ID `N1.4` / `N2.6` / stage ID `// stage: 4`            |
| `tests/runtime/<name>.aether` output path                   | `tests/witnesses/<name>.rs` output path                              |
| `// roadmap: P{phase}.{item}` header                        | `// stage: N` header (N=0..8)                                       |
| `// expect: exit={code}` / `// expect: stdout contains ...` | `// expect: <Rust assert summary>` (e.g. "master.flac LUFS within ±0.5 of -14.0") |
| `// requires: cuda` (Aether GPU tests)                      | `// requires: <real-endpoint dependency>` (e.g. "ffmpeg", "ollama at $NIGHTDRIVE_OPENCLAW_URL", "cnc:8080 reachable") |
| `use runtime; ... fn main() -> i32 { ... }` test shape       | `#[tokio::test] async fn name() { ... }` test shape                  |
| `target/debug/aetherc.exe <file> --emit=aether-bin`         | `cargo test --workspace --test <name>`                               |
| `stdlib/runtime.aether` extern decls                         | `nightdrive_core` shared types + the relevant per-stage crate        |
| `compiler/src/codegen/asm/mod.rs` "compilable today" check  | `crates/<crate>/src/lib.rs` "shipped today" check                    |
| 100-line cap                                                 | Same 100-line cap                                                    |
| Naming: `enum_some_none.aether`, `conv2d_3x3.aether`         | Naming: `audio_sidecar_health.rs`, `loudnorm_hits_target.rs`         |
| "needs P7.1 dtype matrix to land" caveat                    | "needs N1.5 sao-sidecar to be deployed on cnc; tagged #[ignore] until then" |

**The no-mocks rule** must be in the body explicitly — cite `tests/witnesses/README.md` for the documented exception (retry-policy tests).

- [ ] **Step 1: Read the aether source**

Run: `cat /j/aether/.claude/agents/witness-test-author.md`

- [ ] **Step 2: Write the nightdrive version**

Frontmatter:

```yaml
---
name: witness-test-author
description: Use this subagent to draft a witness test for a specific nightdrive roadmap item (e.g. N1.4 llm crate, N2.6 OBS integration) or a specific pipeline stage (// stage: N where N is 0..8 from HANDOFF.md §5). Reads docs/ROADMAP.md for the witness criterion, looks at similar existing tests under tests/witnesses/ for stylistic precedent, writes a tests/witnesses/<descriptive_name>.rs file with the right // stage: N tag + expect markers + minimal-but-real exercise of the feature. Hits real endpoints — no mocks (per the rule documented in tests/witnesses/README.md). Returns the test path + a one-line summary. Doesn't run the audit — that's the caller's job.
tools: Read, Write, Glob, Grep
---
```

Body keeps aether's 5-section shape. The header convention shown in the body must match exactly what's in `tests/witnesses/README.md`:

```rust
// stage: N
// expect: <one-line claim that the test asserts>
// requires: <real-endpoint dependency, or "none" for pure-CPU tests>
//
// <2-4 sentence description: WHAT this test proves.
//  Cite the crate / fn / external endpoint exercised.>
#[tokio::test]
async fn descriptive_name() { ... }
```

Picking the assertion target — adapt aether's "exit=N pick distinctive" rule:
- For "the file exists" / "round-trips" tests, assert against a specific byte size, hash, or measurable property — never just `assert!(true)`.
- For external-endpoint tests, assert against a non-trivial response field (sample_rate=44100, model="stable-audio-open-1.0", privacyStatus="private") — something the endpoint can only return if the codepath actually ran.

Rules adapt:
- The test MUST compile + run today via `cargo test --workspace --test <name>`. If the underlying crate isn't shipped yet, STOP and report the gap — don't write a test that can't compile yet. (Same as aether's "if the feature isn't yet in the compiler, STOP".)
- Keep tests under 100 lines.
- Tag with EXACTLY the right stage number (case-sensitive `// stage: N`, single space after the colon).
- Don't import anything not declared in the relevant crate's public API.
- For real-endpoint tests, mark `#[cfg_attr(not(feature = "<feature>"), ignore = "needs <endpoint>")]` so the suite passes without the network access.

- [ ] **Step 3: Verify**

Run: `head -5 /j/nightdrive/.claude/agents/witness-test-author.md`
Expected: frontmatter parses; tools whitelist is `Read, Write, Glob, Grep` (NOT Bash — author-only, doesn't run anything).

---

## Task 10: Create `.claude/agents/coverage-matrix.md`

**Files:**
- Read: `J:\aether\.claude\agents\coverage-matrix.md`
- Create: `J:\nightdrive\.claude\agents\coverage-matrix.md`

**Substitution map:**

| aether                                                     | nightdrive                                                            |
|------------------------------------------------------------|------------------------------------------------------------------------|
| (op, dtype, device) coverage axis                          | (crate, stage, test-type) coverage axis                                |
| `runtime/src/lib.rs` for CPU op definitions                | `crates/*/src/lib.rs` for crate-level public surface                  |
| `runtime/src/cuda.rs` for GPU op definitions               | (n/a — drop)                                                           |
| `compiler/src/codegen/asm/mod.rs::method_dispatch` user-callable | `crates/*/src/lib.rs` `pub` items (the user-callable surface)         |
| `docs/ROADMAP_V2.md` section 7.3 EXPECTED op surface       | `docs/ROADMAP.md` Phases N1-N5 EXPECTED crate × stage × test matrix    |
| Optional Candle source enumeration                         | (drop)                                                                 |
| Output table: ops × CPU/GPU/dispatch                        | Output table: crates (rows) × `unit | integration | witness | bench` (cols), with the stage number as a third column |
| Examples: matmul, conv2d, gelu                              | Examples: nightdrive-core, nightdrive-llm, nightdrive-audio-master    |
| Coverage = K / N as %                                       | Same K / N % calc                                                      |

- [ ] **Step 1: Read the aether source**

Run: `cat /j/aether/.claude/agents/coverage-matrix.md`

- [ ] **Step 2: Write the nightdrive version**

Frontmatter:

```yaml
---
name: coverage-matrix
description: Use this subagent to produce a (crate × stage × test-type) coverage matrix for nightdrive. Reads crates/*/src/lib.rs to enumerate the workspace, cross-references against docs/ROADMAP.md's expected per-stage witnesses, and produces a markdown table showing which (crate, stage, test-type) cells are covered + which are missing. Helpful before claiming "the pipeline is tested end-to-end" — the matrix tells you precisely what's true. Used as input to roadmap-tracker reports and PR descriptions.
tools: Read, Glob, Grep
---
```

Body adapted from aether's. Inputs:
1. `crates/*/src/lib.rs` — enumerate via Glob.
2. `tests/witnesses/*.rs` — group by `// stage: N` tag.
3. `crates/*/tests/*.rs` (Cargo's per-crate integration test convention) and `crates/*/src/**/*.rs` containing `#[cfg(test)] mod tests` (unit tests).
4. `bench/*/run_all.ps1` — bench presence per stage.
5. `docs/ROADMAP.md` — the EXPECTED matrix (every numbered item lists its witness file path).

Output table format:

```markdown
## Coverage matrix (as of <date>)

| crate                        | stage | unit | integration | witness | bench |
|------------------------------|:-----:|:----:|:-----------:|:-------:|:-----:|
| nightdrive-core              |   0   |  ✓   |     ✓       |    ✓    |   -   |
| nightdrive-llm               |   1   |  ✓   |     -       |    ✓    |   -   |
| nightdrive-audio-gen         |   2   |  -   |     -       |    -    |   ✓   |
| nightdrive-art               |   3   |  -   |     -       |    -    |   -   |
| nightdrive-audio-master      |   4   |  -   |     -       |    -    |   -   |
| nightdrive-visuals           |   5   |  -   |     -       |    -    |   -   |
| nightdrive-encoder           |   6   |  -   |     -       |    -    |   -   |
| nightdrive-youtube           |   7   |  -   |     ✓       |    -    |   -   |
| nightdrive-storage           |  0,3-7|  ✓   |     -       |    -    |   -   |
| nightdrive-orchestrator      |  0,8  |  -   |     -       |    -    |   ✓   |
| nightdrive-cli               |   0   |  -   |     ✓       |    -    |   -   |
```

Plus summary block:
- Total cells expected: N (= crates × test-types covered by ROADMAP.md witnesses)
- Total ✓: K
- Coverage = K / N as %
- Gaps grouped by stage: stage 4 has 0/3, stage 5 has 0/4, …

Rules adapted:
- "Don't double-count" — a crate that participates in 2 stages still appears in 1 row, with the `stage` column listing both.
- "Be HONEST about variants" — if `nightdrive-llm` has unit tests but no witness yet, list it as `unit:✓ witness:-` rather than rounding up.
- Don't include test-only crates (e.g. internal test helpers) in the matrix.

- [ ] **Step 3: Verify**

Run: `head -5 /j/nightdrive/.claude/agents/coverage-matrix.md`
Expected: frontmatter parses; tools whitelist is `Read, Glob, Grep` (read-only).

---

## Task 11: Update `CLAUDE.md` — add "Discipline stack" subsection

**Files:**
- Modify: `J:\nightdrive\CLAUDE.md`

- [ ] **Step 1: Find the insertion point**

The discipline-stack subsection goes after the "DO NOT REINVENT" section (which ends with the "How to use this section" subsection — line ending with `(per `feedback_wheel_capture`).`). Search for that line.

Run: `grep -n "feedback_wheel_capture" /j/nightdrive/CLAUDE.md`
Expected: one match. Subsection insertion point is the `---` separator immediately following.

- [ ] **Step 2: Insert the subsection**

Use Edit with `old_string` = the `---` separator after the DO NOT REINVENT section and `new_string` = `---\n\n## Discipline stack\n\n[content below]\n\n---\n`. Content:

````markdown
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
````

- [ ] **Step 3: Verify the insertion**

Run: `grep -n "Discipline stack" /j/nightdrive/CLAUDE.md`
Expected: one match for the `## Discipline stack` heading.

Run: `grep -c "feedback_wheel_capture" /j/nightdrive/CLAUDE.md`
Expected: 1 (the existing reference; we didn't accidentally duplicate it).

---

## Task 12: Smoke test — verify the stack wires together

**Files:**
- Run: `scripts/audit.ps1`
- Read: `.claude/agents/honesty-auditor.md`

- [ ] **Step 1: Re-run audit.ps1 with the witness directory + bench skeleton present**

Run: `powershell -ExecutionPolicy Bypass -File /j/nightdrive/scripts/audit.ps1`

Expected:
- `[1/7]` build:0 (or noted error count if `cargo check` fails — current scaffold may not yet be in workspace shape)
- `[2/7]` test:0 (no test crates yet)
- `[3/7]` stubs:0 (no `crates/` dir yet — the staged `lib.rs`/`main.rs`/`config.rs` at root don't count because they're not under `crates/`)
- `[4/7]` witnesses:0 (only the README is in `tests/witnesses/`, no `// stage:` tagged `.rs` files)
- `[5/7]` notes the 7 freshly-appended skeleton rows from Task 4
- `[6/7]` reports DRIFT for the schema mismatch (success — this is the target case)
- `[7/7]` `FAIL - drift` (and possibly build:N if scaffold isn't workspace-ready)

The `[6/7]` DRIFT is the headline — it means the audit script catches the known drift, validating that honesty-auditor will too when invoked on the same claim.

- [ ] **Step 2: Verify all 5 agent files exist and parse**

Run: `ls /j/nightdrive/.claude/agents/`
Expected: 5 files, all `.md`.

Run for each: `head -5 /j/nightdrive/.claude/agents/<name>.md`
Expected: each starts with `---`, has `name:`, `description:`, `tools:` fields, ends frontmatter with `---`.

- [ ] **Step 3: Manual dispatch smoke (optional, non-blocking)**

Manually invoke `honesty-auditor` via the Agent tool with this claim list:

```
1. HANDOFF.md section 7 schema matches the 20260510000000_init.sql migration.
2. The supermicro with 8x Tesla P40 (192 GB VRAM) is the inference muscle box for nightdrive.
```

Expected output:
- Claim 1: ❌ FALSE with citation `20260510000000_init.sql:9` (column `key`) vs `HANDOFF.md:194` (column `musical_key`), and similar for `seed`/`subgenre` etc.
- Claim 2: ❌ FALSE with citation `J:\llm-wiki\fleet\Fleet Overview.md` listing the actual GPU pool, and `J:\llm-wiki\experiments\4-gpu-server-listings-2026-04-24.md` confirming supermicro is research toward buying.
- Summary: `Verified: 0 | Partial: 0 | False: 2 | Unverifiable: 0`.

If the agent returns these two correct verdicts, the discipline stack is wired and operational.

---

## Done criterion (whole plan)

- [ ] Five agent files exist under `.claude/agents/`, each with valid YAML frontmatter (`name`, `description`, `tools`) and a body section structure matching aether's source pattern.
- [ ] `scripts/audit.ps1` runs to completion with no PowerShell errors.
- [ ] `scripts/audit.ps1` section [6/7] catches the known HANDOFF/migration schema drift (this is the load-bearing smoke test for the gate).
- [ ] `docs/BENCH_LEDGER.md` exists with header + hardware caption + columns.
- [ ] `bench/pipeline_one_track/run_all.ps1` runs to completion and appends 7 skeleton rows to the ledger.
- [ ] `tests/witnesses/README.md` documents the `// stage: N` convention + no-mocks rule.
- [ ] `CLAUDE.md` "Discipline stack" subsection added with paths and trigger guidance.
- [ ] (Optional smoke) `honesty-auditor` invoked against the two known drift items returns ❌ FALSE for both.

## Out of scope (per the spec)

- `.claude/state/` cursor files
- `.claude/reviews/` finding logs
- Slack/Telegram notifications from agents (use Matt's global `/notify` skill via the caller)
- GitHub Actions integration (banned per global rule)
- Auto-update of HANDOFF.md by `roadmap-tracker` (it's read-only)
- `git init` and commits — no repo yet per harness env; defer until git is initialized in a separate pass

---

## Self-review notes

Spec coverage check (against `docs/superpowers/specs/2026-05-10-discipline-stack-design.md`):
- Layout (Section design > Layout) → Tasks 1, 6-10 cover the 5 agent files; Tasks 2-5 cover the anchor artifacts. ✓
- Anchor re-binding → Tasks 6-10 each have an explicit substitution map. ✓
- Per-agent specs → Tasks 6-10 cover all 5 agents, each with frontmatter and body re-bindings. ✓
- `// stage: N` convention → Task 5 (README) + reinforced in Tasks 9 (witness-test-author body) and CLAUDE.md update. ✓
- `scripts/audit.ps1` 7-section contract → Task 2 implements all 7 sections. ✓
- `docs/BENCH_LEDGER.md` initial format → Task 3. ✓
- Bootstrap plan items 1-6 → Tasks 6-10, 2, 3, 4, 5, 11 respectively. ✓
- Triggering on-demand only (no auto-fire) → reflected in agent description fields (each says "Use this subagent to ..."). ✓
- Out of scope items → captured in the "Out of scope" section above. ✓
- Success criteria (5 items in spec) → Task 12 smoke test maps to spec's success criteria. ✓

Placeholder scan: no "TBD", "TODO", or "implement later" markers in plan steps. Each task has either inline content or a precise port instruction (substitution map + source file path). ✓

Type consistency: agent names spelled identically across all references (`bench-runner`, `honesty-auditor`, `roadmap-tracker`, `witness-test-author`, `coverage-matrix`). The `// stage: N` convention is consistent. Section numbers `[N/7]` of audit.ps1 match the spec. ✓
