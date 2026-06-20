# Prompting & agent orchestration — pointer

**Canonical doc:** `J:\llm-wiki\patterns\llm-prompting-and-orchestration-2026.md`
(fleet-wide field guide; built from a verification-gated research pass, June 2026 — 12 findings confirmed, 13 widely-cited claims killed and explicitly *not* promoted).

This page is the nightdrive-scoped excerpt. Read the canonical doc for sourcing, the confirmed/refuted split, and the model-version caveats.

**Audio-model prompting (the other half):** everything here is *LLM-side* (composer + orchestration). For prompting the actual music model — ACE-Step v1.5-turbo — see **`docs/acestep-prompting.md`** (caption vs tags, BPM/key via dedicated fields, turbo params like CFG-inert / steps=8 / shift=3.0, one-shot full-length generation).

## Why nightdrive is the reference implementation

nightdrive already embodies the validated 2026 shape — keep it this way:

- **Staged workflow, not an autonomous agent.** cron → compose → covers → render → master → encode → upload → 3-day sync-drop runs on predefined code paths. "Workflow before agent" is the confirmed default; agentic autonomy is a cost/latency/compounding-error tax you add only when it buys performance.
- **Parallel fan-out only for the wide, isolatable step.** Album composition fans out N `album-composer` sub-agents (e.g. the 16-album batch on 2026-06-17/18), each in a clean context window, each returning a terse summary — the confirmed sub-agent-isolation mechanism (explore in tens of thousands of tokens, return ~1–2k). Don't fan out sequential/shared-state steps (the render→master→encode chain stays a pipeline).
- **Self-verifying gates before anything ships.** `scratch/validate_albums.py` + `check_runtime_fields.py` (schema + the real `run_album` runtime contract), `scripts/audit.ps1`, the witness tests, and the honesty-auditor/witness sub-agents. A claim isn't real until a runnable artifact proves it — caught dyson-tomb's missing `track_count` and atompunk's motif field on 2026-06-17.

## The one action item for this repo

**Audit forceful imperatives.** Confirmed (2-0, zero contradicting sources): Claude 4.6+ is *more* obedient and **over-triggers** tools/subagents under "CRITICAL: You MUST / NEVER / ALWAYS" phrasing — the style that fixed *under*-triggering on older models now backfires. `CLAUDE.md` (global + this repo's) is dense with those. Soften to neutral ("Use X when…") wherever the forcefulness isn't a genuine safety rail, especially as the fleet's main model moves to 4.6 → 4.7 → 4.8.

## Other confirmed bits worth keeping in mind here

- Composer prompts: put the longform reference (the schema template, the predecessor album JSON) at the **top**, query/instructions after (~30% lift on multi-doc inputs).
- Model targeting: 4.6 uses adaptive thinking, `budget_tokens` deprecated; 4.7/4.8 reject it. Re-validate the composer after any openclaw main-model bump (cf. the composer-timeout / Opus-4.8-schema lesson).
- Don't trust the hype numbers: "90.2% multi-agent uplift", "15× tokens", cache pricing multipliers — all failed verification; re-source before acting on them.
