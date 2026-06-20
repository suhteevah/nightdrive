# ACE-Step prompting & parameters — nightdrive audio reference

**TL;DR:** our ACE-Step v1.5-turbo setup is **mostly correct**. Three concrete deltas, all low-risk; none is a quality regression today. Built from a verification-gated research pass (`wf_aac55964-c45`, 2026-06-19): **13 findings confirmed, 12 killed** — and notably most of the "killed" claims were *stale-v1 critiques that would have wrongly challenged our choices* (they cite the old `ace-step/ACE-Step` repo, not the `ACE-Step-1.5` repo we run). Sourcing is almost entirely the model authors' own v1.5 docs + the paper (arXiv 2506.00045).

Companion: this is the **audio-model** half of `docs/prompting-and-orchestration.md` (which is LLM-side only). The transferable model facts also live at `J:\llm-wiki\patterns\acestep-prompting-instrumental.md`.

## Our current setup (for reference)
- **caption** = composer prose + ` {bpm} BPM` + ` {key}`, capped ~510 chars — `crates/nightdrive-audio-gen/src/prompt.rs::format_ace_step_caption`
- **lyrics** = `[Instrumental]` + bare `[Section]` tags — `format_ace_step_lyrics`
- dedicated **`bpm`** + **`keyscale`** request fields populated
- `inference_steps=8`, `guidance_scale=7.0`, `shift=3.0` (sidecar `DEFAULT_SHIFT`), `infer_method="ode"`, one-shot full-length, seed-pinned

## ✅ Keep (confirmed correct, first-party-sourced)
- **`inference_steps=8`** — the documented turbo default/optimal (range 1–20; base uses 32–64). No change.
- **One-shot full-length** — ACE-Step is diffusion (Sana DCAE + ~28-layer linear transformer), *not* autoregressive like MusicGen, so it renders the whole track in one pass via the `duration` field (10–600 s). No 30 s segment chaining, no seams. (`segment_seconds=30` in `[audio_gen]` is vestigial MusicGen-era cruft, ignored on the ace_step path.)
- **`[Instrumental]` lyrics tag** — the explicit, *trained* instrumental directive (paper §3.1.3; INFERENCE.md). Keep it leading the lyrics block.
- **Bare `[Section]` tags** — fine. The idea that EDM-native `[Build]/[Drop]/[Breakdown]` are *better* than `[Intro]/[Verse]/[Chorus]` did **not** survive verification. Don't churn the section tags. (And keep instrumentation hints *out* of the brackets — ACE-Step vocalizes them as ghost phonemes; the 2026-05-22 fix stands.)
- **Dedicated `bpm` + `keyscale` fields** — first-class v1.5 params ("user-provided values always win; the LM only fills empty fields"). This is the *correct* tempo/key control. Treated as a probabilistic anchor (bpm 108 may land 106–110) — by design, not a bug.
- **Prose caption** — supported, not penalized. The text encoder is deliberately format-agnostic (trained on tags AND prose AND usage-scenarios; LLM-augmented to prevent format overfitting; a Qwen3-0.6B "refiner" normalizes input). The real lever is **specificity** (concrete instruments/timbre/mood) over vague aesthetic phrasing — ours is already specific.
- **`infer_method="ode"` + seed pinning** — ode is deterministic (correct for reproducible renders); seed sets the DiT initial noise; vary it for diversity. ✓
- **BPM/key range** — all-minor synthwave at BPM ~96–112 sits in the model's documented stable zone (common keys C/G/D/Am/Em + BPM 60–180 reliable; rare keys / extreme tempos sparse).

## 🔧 Change (actionable, low-risk — recommended, NOT yet applied)
1. **Stop appending BPM + key to the caption** (`format_ace_step_caption`). Docs are explicit: *"Don't write tempo, BPM, key in Caption — set them through dedicated metadata parameters."* We already populate `bpm`/`keyscale`, so the caption append is redundant (docs say redundant, not harmful → zero downside, just cleaner). Two layers:
   - Easy: drop the two `push_separated(…, "{bpm} BPM")` / key lines in `prompt.rs`.
   - Fuller: tell the album-composer to stop embedding "108 BPM D major" in the `musicgen_prompt` prose itself (composer-prompt tweak) — that's the real source.
2. **`guidance_scale` is INERT on turbo — config-clarity cleanup.** CFG is "only supported for non-turbo." ACE-Step's own pre-flight already auto-overrides our 7.0 → 1.0 for turbo and logs it (HANDOFF §, ~line 1533) → **zero quality impact today**, but `[audio_gen] guidance_scale = 7.0` is *misleading*. Set it to 1.0 (or drop it) on the turbo path and stop tuning it. (Only matters if we ever switch to base/SFT — see Open questions.)
3. **Make `shift=3.0` explicit.** Author-recommended turbo value; we already get it via the sidecar `DEFAULT_SHIFT`, but it's not in `[audio_gen]`. Pin it in config so it's intentional, not an accidental default.

## 🧪 Worth an A/B (medium confidence, community-sourced — not verified)
- **Tag-style caption vs prose.** The model is format-robust, but multiple community testers report ACE-Step adheres *marginally* better to clean comma-separated "control-signal" tags than to prose for tight genre control. Uncontrolled/blog-quality → test it: same spec + same seed, render (a) current prose vs (b) a tag list (`"synthwave, DX7 pad, analog lead, sidechained sub bass, gated reverb drums, neon, driving, instrumental"`), ear-compare adherence + mud. Cheap; could be a real lever for our single genre.
- **One-shot quality at the long end.** 3–4 min is the documented "stable" band; 5–6 min "may have repetition/structure issues." Our tracks run 180–360 s, so the 5–6 min ones are at risk. Sweep quality at 4/5/6 min on the cnc P100 turbo path; if the long ones drift, either cap durations nearer ~4 min or use ACE-Step **repaint** (`repainting_start/end`) to fix sections in place instead of re-rolling.

## 🚫 Don't be fooled (killed claims — mostly stale v1)
The pass killed 12 claims; several would have wrongly "challenged" us because they cite the **old v1 repo**, not v1.5:
- "256-token UMT5 caption truncation" → **false for v1.5**; don't fear tail-truncation of "instrumental". (Our ~510-char cap is self-imposed/safe, not model-mandated.)
- "No dedicated bpm/key params; must embed in the prompt" → **false for v1.5** (they're first-class).
- "Base defaults 15.0 CFG / 60 steps challenge your 7.0/8" → irrelevant; those are base-model values, we run turbo.
- "[Build]/[Drop] tags are better for synthwave" / "use the dedicated Instrumental checkbox instead of [Instrumental]" → did not survive; keep what we have.

## Open questions
- Exact v1.5 caption length cap (the 256-token claim was stale-v1; no confirmed v1.5 number) — verify there's no silent tail-truncation.
- Does a tag caption measurably beat prose for *our* synthwave? (A/B above.)
- Where exactly does one-shot quality degrade on the P100 turbo path — 4/5/6 min?
- Worth ever switching the monetized channel to the base/SFT model (active APG CFG ~50 steps) for fidelity, at higher render cost? If so, tune `guidance_scale` + `guidance_interval` + `min_guidance_scale` together — the turbo findings don't transfer.

## Sources (primary)
ACE-Step-1.5 docs: `Tutorial.md`, `INFERENCE.md`, `GRADIO_GUIDE.md`, `ace_step_musicians_guide.md`, `API.md`, `docs/sidestep/Shift and Timestep Sampling.md`. Paper: arXiv 2506.00045. Base-model APG internals: `ace-step/ACE-Step/acestep/{pipeline_ace_step,apg_guidance}.py`. Full verified run: `wf_aac55964-c45` (2026-06-19).
