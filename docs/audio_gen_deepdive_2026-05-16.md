# Audio-Gen Deep Dive — 2026-05-16

> **Trigger:** Matt: "I can still somehow hear that 30s clip transition…
> we likely need to have an agent or two to pipeline our
> prompt/composition and if we can we need to start with saving
> everything to something editable (MIDI, FLAC) so we can get it on more
> platforms ie Spotify."
>
> **Locked constraints (this session):** Local-only inference. Vocals
> TBD per album. All-tiered editability (FLAC + stems + MIDI). Fix
> *all three* gaps: seams, structure, instrument fidelity.

---

## 0. TL;DR (read this if nothing else)

1. **The seam Matt hears is fixable in TWO completely independent ways.** The
   cheap fix (Multi-Band Diffusion decoder on existing MG) ships in a day
   and likely cuts the seam-audibility by a wide margin. The clean fix
   (replace MG with ACE-Step 1.5) eliminates seams **by construction** —
   ACE-Step generates a full 4-min song in one shot, no chunking, no
   continuation prefix re-encode, no timbre drift at minute 1.
2. **MusicGen is no longer the right primary engine.** ACE-Step 1.5
   (released Jan 2026, XL April 2026) is **MIT-licensed (commercial-clean,
   kills the CC-BY-NC strike risk we accepted on day one)**, runs in
   **<4 GB VRAM** for the base model, generates a full song in **<10s on
   an RTX 3090** vs. our current ~14-18 min per track wall, and supports
   both instrumental and vocal modes via a `[instrumental]` tag.
3. **The prompt-composition pipeline is missing two agent layers.** Today:
   `album-composer` → one `musicgen_prompt` per track → fed verbatim to
   every segment in that track. Proposed: add an **arranger** (turns
   per-track `sections[]` into per-segment prompt evolutions) and a
   **listener** (audio QC: detects flat takes, key drift, missed motifs,
   triggers regen with seed bump + critique). The album-composer doesn't
   change; it stays the album-design authority.
4. **Editable outputs are tiered:** FLAC tomorrow (just stop converting
   the master to MP3-only — the FLAC already exists at
   `tracks/<id>/master.flac`), stems via Demucs `htdemucs_ft` next week
   (~9 dB SDR, drums/bass/vocals/other), MIDI transcription via Spotify
   `basic-pitch` or Magenta `MT3` as an experimental side channel.
5. **Spotify distribution:** unlocked by the FLAC tier. DistroKid or
   TuneCore are the distributors; an artist account on either accepts
   FLAC/WAV uploads. Decision needed on commercial entity (Ridge Cell vs
   a new "NightDrive" LLC) — pricing scales differently per service.

The rest of this doc is the evidence, the model menu, the architecture,
and a phased rollout.

---

## 1. Diagnosis — Why MG-stereo-medium Sounds Like MG (and Where the Seam Comes From)

The audio-gen crate at `crates/nightdrive-audio-gen/src/lib.rs` has *two*
fully-implemented engines:

- **`StableAudioClient`** (lines 56-242) — calls SAO sidecar for each
  ≤47s segment, stitches with equal-power cosine crossfade in
  `crossfade_into` (lines 317-350). **Blind crossfade — no audio prior
  shared between segments.** Track #1 (Nocturnal Lanes) used this; Matt's
  first listen flagged audible seams every ~34 s.
- **`MusicGenClient`** (lines 377-581) — uses MG's `generate_continuation`:
  first segment is fresh text-to-audio, subsequent segments receive the
  last `continuation_prefix_seconds` (default 5 s) of accumulated audio
  as `prev_audio_b64`. The sidecar re-encodes that prefix through EnCodec,
  generates an extension, returns prefix + new audio. We strip the
  regenerated prefix (it's the "redo" of audio we already have) and
  append only the new continuation. This is what tracks #2-N (including
  all of Sunset Vol. 1 and Tron Vol. 1) used.

**So Matt is hearing seams on the *continuation* engine, not the
crossfade engine.** The continuation engine should theoretically be
seamless — and *most* of the time it is, but here's why it isn't fully:

### 1.1 Same prompt every segment → texture drift

In `MusicGenClient::render`, the same `spec.musicgen_prompt` is sent for
all N segments (line 474 + line 507). The model's noise-floor character
on each call is *similar* but not *identical* — slightly different
high-frequency hash, slightly different reverb tail decay, slightly
different drum-machine-style consistency. The continuation stitches the
*musical material* coherently (chord, drums, lead) but doesn't fully
mask the *production-character* drift. You hear it as a faint "shimmer"
or "feel" change at the join.

### 1.2 32 kHz mono-rebroadcast loses spatial mask

`musicgen-server.py` line 110-114: if the model returns mono, we duplicate
the channel to fake stereo. MG-stereo-medium *should* return real
stereo (it's the "stereo" variant) but if a track ever falls back to
mono, the seam is unmasked — there's no stereo image difference to
distract the ear from the texture shift.

### 1.3 EnCodec round-trip artifacts on the prefix

Every continuation segment encodes the previous audio's tail through
EnCodec (the audio tokenizer used by MG), generates new tokens, decodes
back to PCM. The tokens for the prefix region are deterministically
quantized — but on the *decode* side, the model has implicit knowledge
that "tokens that came from continuation context are 'committed' and
tokens that are 'mine' are fresh." This is fine harmonically but creates
a barely-audible "envelope refresh" right at the 5-second-mark where
the prefix-stripped boundary sits.

### 1.4 No section progression in the prompt

The `CompositionSpec` has `sections` (intro/verse/chorus/bridge/outro)
with per-section `instrumentation` strings (see HANDOFF.md §5.1 and
album-composer.md). **None of that information reaches the MG sidecar.**
We just send the global `musicgen_prompt` for every segment. The model
is asked to generate the *same thing* 12 times in a row, which means it
never knows when to drop the lead in, when to swell the pads, when to
mute the kick for the bridge. The result is a track that feels like
"one mood, same texture, for 4 minutes." That's *also* part of the "I
can still hear it" complaint — even if the seam disappears, the song
itself is structurally flat.

### 1.5 Instrument fidelity ceiling of MG-stereo-medium

MG-stereo-medium is 1.5B parameters, trained on ShutterStock + ~20k hrs
licensed music. Its synthwave knowledge is genuine but limited — the
"DX7 pad" you specify comes out as *something pad-like with rolloff in
the right place*, not an actual DX7. Newer architectures (ACE-Step,
DiffRhythm 2) trained on much larger or differently-curated corpora
produce noticeably crisper instrument timbres.

---

## 2. The 2026 Open-Source Local Audio-Gen Menu

Surveyed today, ordered by fit-for-NightDrive:

### 2.1 ACE-Step 1.5 — **the recommended new primary engine**

- **Released:** Jan 2026 (1.5) + April 2026 (1.5 XL with 4B DiT decoder)
- **License:** MIT — *commercial-clean, no strike risk, no caveats*
- **Architecture:** Diffusion transformer + 5 Hz LM head + flow-matching decoder
- **VRAM:**
  - Base 1.5 (~1.7B SFT AIO): **<4 GB VRAM** — fits kokonoe alongside SDXL!
  - XL (4B DiT): ≥12 GB with offload, ≥20 GB recommended without
- **Speed:** <2 s per full song on A100, <10 s on RTX 3090. On P100 likely 30-60 s (no fp16, PT backend), still 15-50× faster than our MG continuation
- **Generation:** Full 4-min songs in ONE shot. No chunking. No continuation. **The seam problem dies by construction.**
- **Vocals:** Optional. Use `[instrumental]` or `[inst]` as the *only*
  content in the lyrics field for instrumental tracks. Genuine multi-language
  vocals when you want them.
- **Pascal P100 (cnc):** Supported. Auto-falls-back to PyTorch backend
  (no vLLM since sm_60 < 7.0). Slower than Volta+ but functional.
- **Special features:**
  - **LoRA fine-tuning from a few songs** — once we have Sunset Vol. 1 +
    Tron Vol. 1 + future albums published, train a "NightDrive style"
    LoRA on top of ACE-Step base. The model now knows *our* voice.
  - **Vocal-to-BGM conversion** — feed an a-capella, get an arrangement.
    Future feature.
  - **Conditioning controls:** structured prompt format (genre + mood +
    tempo + instruments + arrangement + production) plus optional
    melody/style reference audio.

**Why this is the answer:**
- Solves seam ✅ (single-shot)
- Solves structure ✅ (the model is trained to know intro/verse/chorus from full-song training)
- Solves fidelity ✅ (newer, larger architecture)
- Solves license ✅ (MIT — clean for monetized channel)
- Faster than current MG ✅
- Fits existing hardware ✅ (kokonoe 8 GB or cnc P100 16 GB)

### 2.2 MusicGen-stereo-medium + Multi-Band Diffusion (MBD)

- **License:** CC-BY-NC (already accepted risk)
- **What MBD adds:** Replaces MG's default EnCodec decoder with a
  diffusion-based decoder. Same tokens in, much higher-fidelity waveform
  out. Documented in `audiocraft/docs/MBD.md`. Trivial API:
  `mbd.tokens_to_wav(tokens)` where `tokens` is the MG token output.
- **Cost:** Extra compute per track (diffusion inference on the token
  stream). Estimate: +30-50% wall time, still under our current 14-18
  min per track on kokonoe.
- **Why include this even if we're moving to ACE-Step:** It's a
  drop-in upgrade *today*. We can ship it on the existing MG sidecar in
  hours, get an immediate quality bump on already-built infrastructure,
  and have a fallback engine if ACE-Step's synthwave-specific output
  ever needs a backup.

### 2.3 Stable Audio Open 1.0 (current crossfade engine — re-evaluate)

- **License:** Stable Audio Community License. **NOT actually commercial
  unless org revenue is <$1M/yr** — and even then, the "free for
  commercial under $1M" is *under certain conditions* per their license.
  This is a *grey* zone, not green. Worse than I thought.
- **Stable Audio Open 2** as open weights: **does not exist publicly**
  as of 2026-05-16. Stable Audio 2.0 / 2.5 are Stability's SaaS-only
  products.
- **Verdict:** Already in our pipeline, but the license isn't as clean
  as we'd like and the model is older (47 s segment ceiling, blind
  crossfade required). **Demote from primary, keep as secondary fallback
  only.**

### 2.4 DiffRhythm 2 — **defer (no instrumental mode yet)**

- **License:** Apache 2.0 (cleanest!)
- **Generation:** Up to 210 s full song with synchronized vocals
- **Critical limitation:** "Instrumental music generation" is listed in
  the project's TODO. Today it requires lyrics input. **For NightDrive's
  current instrumental-only ethos, this is a non-starter.** Re-evaluate
  in 1-2 quarters when instrumental mode lands.
- **DiffRhythm 1** (the predecessor) does 4:45 songs and is Apache 2.0
  but the architecture is older and quality is worse than DiffRhythm 2
  by all reports.

### 2.5 YuE — overkill / too heavy

- **License:** Apache 2.0
- **VRAM:** Base 7B model wants 24 GB+. Quantized YuEGP can run on
  8 GB but musicality reportedly suffers.
- **Generation:** Several minutes of lyrics-to-song with vocals
- **Why skip:** ACE-Step 1.5 dominates on both quality and hardware fit.
  YuE is interesting if we ever want a *vocal-forward* track and ACE-Step's
  vocals disappoint, but that's a contingent future scenario.

### 2.6 Quick reference table

| Model | VRAM (min) | Song length | Vocals | License | Speed |
|---|---|---|---|---|---|
| **ACE-Step 1.5 base** | <4 GB | 4 min single-shot | optional | MIT | <10 s on 3090 |
| ACE-Step 1.5 XL | 12 GB w/ offload | 4+ min | optional | MIT | slower |
| MG-stereo-medium + MBD | ~5 GB | 30 s × N continuation | no | CC-BY-NC | ~25 s/segment |
| MG-stereo-large + MBD | ~10 GB | 30 s × N continuation | no | CC-BY-NC | slower |
| DiffRhythm 2 | ~8 GB est. | 210 s | required | Apache 2.0 | unknown |
| YuE 7B | 24 GB | minutes | required | Apache 2.0 | slow |
| YuEGP (quantized) | 8 GB | minutes | required | Apache 2.0 | very slow |
| Stable Audio Open 1.0 | ~4 GB | 47 s + crossfade | no | Stable Community | ~25 s/segment |

---

## 3. Recommended Engine Strategy

```
                  ┌─────────────────────────────────────────────┐
                  │  ENGINE CHOICE PER ALBUM (config / agent)    │
                  └─────────────────────────────────────────────┘
                                       │
              ┌────────────────────────┼────────────────────────┐
              ▼                        ▼                        ▼
   ACE-Step 1.5 base/XL    MG-stereo + MBD (fallback)   SAO 1.0 (deprecated)
   ─ instrumental albums    ─ albums already in flight   ─ ad-hoc only
   ─ ALL future albums      ─ regression catcher          ─ no new work
   ─ MIT, no strike risk    ─ proven on Sunset/Tron       ─ license grey
```

**Phase 0 (today–this week)**: ship MBD on the existing MG sidecar.
Re-render one Tron Vol. 1 track with MBD vs without, blind A/B. If MBD
materially helps, MG+MBD becomes "good enough" for the next ~1 album
while ACE-Step lands.

**Phase 1 (this week, post P100s)**: deploy ACE-Step 1.5 base on
kokonoe (it fits in <4 GB alongside SDXL — solves the VRAM-contention
problem documented in HANDOFF §18). Also deploy on a cnc P100 in
parallel to bench Pascal performance. **First ACE-Step test album:
Tokyo Cyberpunk Vol. 1** (the queued third album from HANDOFF §20). New
engine, new aesthetic — clean signal on whether the upgrade is heard.

**Phase 2 (week 2)**: Make ACE-Step the default. MG stays as
`engine = "musicgen"` in config; ACE-Step is `engine = "ace_step"` and
the new default. SAO becomes `engine = "stable_audio"` retained for
the audit's "this was real once" purpose only.

---

## 4. Prompt Composition — the Multi-Agent Pipeline

Matt: *"we likely need to have an agent or two to pipeline our
prompt/composition."* Here's where they slot in.

### 4.1 What we have today

```
album-composer  (Claude subagent, .claude/agents/album-composer.md)
  └── writes docs/albums/<slug>.json with 12 × per-track:
        title, key, bpm, sections[], musicgen_prompt, cover_prompt,
        composer_notes, motif tracking, key-relationship-to-prior
            ↓
orchestrator::pipeline_one_album reads JSON, calls audio-gen
            ↓
audio-gen::MusicGenClient sends SAME musicgen_prompt for every segment
            ↓
sidecar generates audio, returns WAV
```

The album-composer is *excellent at album design*. The breakdown happens
*below* it: the per-segment specificity in `sections[]` is thrown away.

### 4.2 Proposed additions

```
album-composer  (unchanged — album design authority)
        ↓
arranger  (NEW — per-section prompt evolution)
   └── reads docs/albums/<slug>.json + section structure
   └── emits per-section prompt blocks:
        intro_prompt, verse_prompt, chorus_prompt, bridge_prompt, outro_prompt
   └── enforces continuity: each section prompt names what carries
        over and what changes from the prior section
        ↓
prompt-engineer  (NEW — model-aware translator)
   └── input: per-section prompt blocks + target engine (ace_step | musicgen)
   └── output: engine-native prompts
        - ACE-Step: structured "Genre + mood + tempo + instruments +
          arrangement + production" format + [instrumental] tag
        - MusicGen: comma-separated descriptor format, ≤60 words
   └── handles T5 token cap warnings for the ACE-Step path
        (audiocraft logs `[synthwave-gen][WARN] track 'X' prompt is N
        units, TAIL WILL BE DROPPED` — same trap, port the preflight)
        ↓
audio-gen::AceStepClient (or MusicGenClient with section evolution)
   └── ACE-Step: one HTTP call per track, full song at once
   └── MG: one HTTP call per section with the per-section prompt;
       continuation context still chains across sections
        ↓
listener  (NEW — audio QC)
   └── reads raw.wav + spec
   └── checks:
      ─ key detection (does the audio land in spec.musical_key?)
      ─ BPM detection (does it land in spec.bpm ± 2?)
      ─ energy/dynamics: is this a "flat take" with no movement?
      ─ silence detection: did the model produce dead air segments?
      ─ motif presence: rough chroma-feature match against the
        composer-defined motifs (heuristic, not exact)
   └── on fail: write a critique to the spec, bump seed, regenerate
        ↓
master + encode + upload  (existing)
```

**Why this maps to "an agent or two":**

- **arranger** can be a *prompt-only* Claude call, no special tools. Reads
  the album JSON, writes back enriched JSON. Fast, cheap.
- **prompt-engineer** can be a deterministic Rust function — no LLM
  needed. Just a string formatter aware of each model's prompt syntax.
  Encode this as a `nightdrive-llm::prompt_for_engine(spec, section,
  engine)` helper. **Not actually an agent, just structured code.**
- **listener** is the real agent — needs to ingest audio, run analysis,
  emit critique. Two implementation tracks:
  - **Lite (today):** Pure Rust `nightdrive-audio-qc` crate using
    `aubio` (BPM detection), `essentia` or hand-rolled FFT for key
    estimation. Threshold-based pass/fail. Calls happen in-process,
    no Claude needed.
  - **Smart (later):** Spectral features + a small classifier LLM (a
    fine-tuned LLaVA-style audio model or whisper-large embeddings)
    that "listens" and writes a paragraph critique. Higher quality
    but more expensive per track.

**Minimum-viable agent layer (Phase 1):** Add the **arranger** (Claude
subagent, .claude/agents/arranger.md) and a **deterministic
prompt-engineer helper** in nightdrive-llm. Defer the listener to Phase 2.

### 4.3 What the arranger does (concretely)

Given an album JSON entry like:

```json
{
  "track_number": 6,
  "title": "Apex",
  "role": "peak 1",
  "key": "D major",
  "bpm": 108,
  "sections": [
    {"name": "intro", "bars": 4,  "instrumentation": "pad swell + filtered arp"},
    {"name": "verse", "bars": 16, "instrumentation": "+ sub bass + drums entering"},
    {"name": "chorus","bars": 16, "instrumentation": "+ lead + sidechain pump"},
    {"name": "bridge","bars": 8,  "instrumentation": "stripped, only pad + bass"},
    {"name": "chorus","bars": 16, "instrumentation": "full + extra lead double"},
    {"name": "outro", "bars": 8,  "instrumentation": "tape stop"}
  ],
  "musicgen_prompt": "synthwave 108 BPM D major peak track, lush DX7 pad..."
}
```

…the arranger emits:

```json
{
  "section_prompts": [
    {
      "name": "intro",
      "bars": 4,
      "start_seconds": 0,
      "end_seconds": 8.9,
      "prompt": "synthwave 108 BPM D major intro, lush DX7 pad swelling slowly, filtered arpeggio rising in the right channel, no drums yet, anticipation, no vocals, instrumental",
      "continuity_with_prior": "—"
    },
    {
      "name": "verse",
      "bars": 16,
      "start_seconds": 8.9,
      "end_seconds": 44.4,
      "prompt": "synthwave 108 BPM D major verse, same DX7 pad continuing from intro, sub bass enters on downbeat, gated reverb drums building, filtered arp still in right channel, driving forward, instrumental",
      "continuity_with_prior": "pad continues, arp continues, drums and bass new"
    },
    ...
  ]
}
```

The MG (or ACE-Step) client then either:

- **ACE-Step path:** concatenates the section prompts into a single
  arrangement-aware prompt (ACE-Step accepts structured arrangement
  descriptions) and generates the whole track in one call. Section
  *information* survives even though the call is one-shot.
- **MG continuation path:** generates segment-by-section, with the
  prefix being the last N seconds of the prior section's audio. Each
  segment gets the *specific* section prompt rather than the global
  one. Major improvement on the current behavior.

---

## 5. Editable Outputs — FLAC + Stems + MIDI

Three deliverables, three difficulty tiers.

### 5.1 FLAC — already there, just stop hiding it

`nightdrive-audio-master` already produces `master.flac` (lossless, 24-bit
likely) as the canonical intermediate. The pipeline currently converts
this to MP3/AAC for the YouTube encode, but the FLAC is still on disk at
`/var/lib/nightdrive/tracks/<id>/master.flac`.

**Action:** Add a `nightdrive-cli export-flac --album <slug>` subcommand
that copies all album FLACs into a clean `exports/<album>/` directory
with normalized filenames (`01 - First Light Off the Pier.flac`). Done.
~30 minutes of work.

**Spotify / Apple Music path:** DistroKid accepts WAV or FLAC at
16/24-bit, 44.1/48/96 kHz. TuneCore similar. Cost is $19.99/yr to
unlimited releases on DistroKid or per-release pricing on TuneCore.
Distribution from the FLAC tier requires no new pipeline work — only
business decisions:
- Artist name: "NightDrive" or "Ridge Cell Records" or new entity?
- Royalty splits?
- Trademark check on "NightDrive" as artist alias?

### 5.2 Stems — Demucs `htdemucs_ft`

**htdemucs_ft** is the SOTA hybrid transformer source separator from Meta
(Facebook Research). Splits a stereo audio file into 4 stems: **drums /
bass / vocals / other**. For our instrumental synthwave, "other"
captures everything synth-y (pads, leads, arps); "vocals" should be
empty (good QC signal — if vocals isn't empty, the model
hallucinated singing and we should retake).

- **License:** MIT
- **Quality:** ~9.0 dB SDR on MUSDB-HQ benchmark; `htdemucs_ft` is
  fine-tuned and ~0.2 dB better than the default
- **Runtime:** Pure Python (PyTorch), ~30 s per 4-min track on a 3070 Ti
- **Install:** `pip install demucs`
- **CLI:** `demucs --two-stems=other -o exports/<album>/stems <input.wav>`
  or `demucs -n htdemucs_ft <input.wav>` for full 4-stem split
- **Output format:** WAV per stem in a per-track directory

**Integration:** new crate `nightdrive-stems` with `StemSeparator` trait
+ `DemucsClient` HTTP impl pointing at a `sidecar/demucs_server.py` on
cnc (post-P100). For the kokonoe-only era, just shell out to demucs CLI
in a `tokio::task::spawn_blocking` from a new orchestrator stage. Stems
join the pipeline at stage 4.5 (after mastering, before final encode).

**Time:** 2-3 days to integrate including witness test.

### 5.3 MIDI transcription — basic-pitch or MT3

Two options:

**Spotify `basic-pitch`:**
- Lightweight (~30 MB model, ~80 ms/sec inference on CPU)
- MIT license
- Polyphonic, pitch-bend detection
- "Works best on one instrument at a time" — so we'd run it per-stem
  rather than on the full mix. (drum stems aren't pitched; bass/other
  stems get MIDI'd.)
- Output: standard MIDI files, 1 per stem

**Magenta `MT3`:**
- Multi-instrument multi-task transcription
- T5-small backbone (~60 M params)
- Trained on Slakh2100 + others
- Higher quality multi-instrument transcription than basic-pitch
- Output: MIDI with per-instrument tracks
- Apache 2.0

**Recommendation:** Start with **basic-pitch on stems** (post-Demucs).
Each stem is mostly mono-timbral (bass = bass, other = synths in one
voicing). Basic-pitch's strength matches stem material.

**Caveat:** No automatic transcription is going to be note-perfect on a
generated synthwave track with thick pads, side-chain pumping, and
diffusion artifacts. The MIDI output should be framed as "a starting
point sketch for DAW work," **not** a faithful re-creation. Matt opens
it in Reaper/Ableton, snaps to grid, deletes garbage notes, fills in
the real bass line — and now has an editable, re-renderable score.

**Render-from-MIDI path (the loop closure):** MIDI files → FluidSynth +
a synthwave SF2 soundfont OR Sforzando + a sfz synthwave instrument
pack OR direct VST host (renoise / reaper command line) → FLAC. This is
where the *true* "infinite remixes / variations" possibility opens —
once we have MIDI + commercial synthwave VST patches, every track can
be re-rendered with different mixes, different patches, different
arrangements.

**Time:** 3-5 days for basic-pitch integration + MIDI export tested
against one track. The "DAW workflow" exploration is separate, can be
Matt's own creative time later.

### 5.4 Output layout per track (proposed)

```
/var/lib/nightdrive/tracks/<id>/
├── spec.json                  (existing)
├── cover.png                  (existing)
├── master.flac                (existing — promoted to public artifact)
├── master.mp3                 (existing — fallback)
├── scene.mp4                  (existing)
├── final.mp4                  (existing)
├── thumbnail.jpg              (existing)
├── stems/                     NEW
│   ├── drums.wav
│   ├── bass.wav
│   ├── vocals.wav             (should be empty / silence for instrumental)
│   └── other.wav
├── midi/                      NEW
│   ├── bass.mid
│   ├── other.mid
│   └── combined.mid           (concat of stem MIDIs into one multi-track file)
└── forecast.json              (existing weather panel archive)
```

`exports/<album>/` is the publishable bundle: cover + FLAC + MP3 +
stems.zip + midi.zip + spec.json copied here, named `<NN> - <Title>`.

---

## 6. Spotify / Streaming Distribution Path

Once FLAC export exists:

1. Sign up for DistroKid (~$19.99/yr) or TuneCore (per-release). DistroKid
   has unlimited releases on the base tier — better for high-volume
   catalog. TuneCore pays 100% of royalties; DistroKid takes a cut on
   some tiers.
2. Artist setup: Pick the artist name. "NightDrive" is risky — check
   trademark, check existing musicians. A unique alias ("nightdrive
   sessions" or similar) might be safer.
3. AI-music disclosure: Spotify's AI policy (as of 2026) requires
   disclosure when AI was used. We're already doing this in the YouTube
   description; do the same in Spotify metadata.
4. Album-by-album upload from `exports/<album>/`: cover + FLACs + ISRC
   codes (DistroKid issues these automatically) + per-track metadata
   (BPM, key, genre tags).
5. Streams pay out per-stream micro-royalties. Won't be a revenue driver
   on its own; *is* a brand-presence driver and a path to playlist
   curation.

**Decision needed:** What's the artist legal entity? Ridge Cell Repair
LLC is the publisher of record for everything Matt does; for music
specifically, that's *fine* legally but the brand connection is weird.
A separate "NightDrive" sole-proprietorship or DBA might be cleaner.
**Not technical, but blocks Spotify upload.**

---

## 7. Specific Tactical Fixes for the Seam Complaint (Phased)

### Phase 0 — Today / this session (within MG)

**Action 1: Per-section prompt evolution.** Wire the per-section
`sections[]` of `CompositionSpec` through to per-segment prompts in
`MusicGenClient::render`. The current code sends `spec.musicgen_prompt`
for every segment (lines 474 + 507); change this to derive
per-section prompts from `spec.sections[]` and route them per segment
based on cumulative-time. This *alone* should reduce the "same texture
for 4 minutes" complaint significantly.

**Action 2: Bump continuation prefix from 5s to 8s.** Longer prefix =
more context for the model to lock its production-character to. Cost: a
bit more EnCodec re-encode work per segment.

**Action 3: Wire MBD into musicgen-server.py.** After `model.generate(...)`,
call `mbd.tokens_to_wav(tokens)` instead of `model.compression_model.decode(tokens)`.
Diffusion decoder. Replaces the artifact-prone EnCodec decoder. Cost:
+30-50% wall time per segment.

Implementation effort: ~1 day for all three. **Ship this before
anything else** — it's pure win on the current pipeline.

### Phase 1 — Next 1-2 weeks (replace MG)

**Action 4: ACE-Step 1.5 base sidecar on kokonoe.** New file
`sidecar/acestep_server.py` modeled on the existing
`sidecar/musicgen_server.py`. <4 GB VRAM = fits alongside MG (kill MG
sidecar to test) or alongside SDXL (now MG-sized VRAM frees up).

**Action 5: `AceStepClient` in `nightdrive-audio-gen`.** Modeled on
`MusicGenClient`, but with the single-shot generation pattern (no
continuation, no segment loop, no crossfade). One POST → one full WAV.
Add a third arm to `client_for(cfg)` for `engine = "ace_step"`.

**Action 6: Update config schema.** `[audio_gen]` gains an `engine`
choice including `ace_step`. Per-engine sub-tables: `[audio_gen.musicgen]`,
`[audio_gen.ace_step]`, `[audio_gen.stable_audio]`. Backward-compatible.

**Action 7: First ACE-Step album = Tokyo Cyberpunk Vol. 1.** Already on
the roadmap (HANDOFF §20 item 4). Brand-new aesthetic + brand-new engine
= clean A/B. If ACE-Step's output isn't audibly better than MG+MBD on
this album, we've learned something cheaply.

Implementation effort: ~3-5 days.

### Phase 2 — Weeks 2-3 (multi-agent, stems, audit)

- Build the **arranger** Claude subagent (`.claude/agents/arranger.md`)
  modeled on the existing album-composer.
- Build the deterministic **prompt-engineer** Rust helper in
  `nightdrive-llm`.
- Build the **listener** audio-QC crate with aubio BPM + key detection.
  Threshold-based pass/regen logic.
- Wire `nightdrive-stems` Demucs htdemucs_ft.

### Phase 3 — Month 2 (LoRA + MIDI + Spotify)

- Train **NightDrive-ACE-Step-LoRA** on Sunset Vol. 1 + Tron Vol. 1 +
  Tokyo Cyberpunk Vol. 1 audio. Lock in our specific aesthetic.
- Integrate **basic-pitch** for MIDI export per stem.
- **Spotify / DistroKid bootstrap** for Sunset Vol. 1 retroactive
  release.

---

## 8. Open Decisions for Matt

These are the real forks; everything else flows from them.

1. **Spend MBD effort if we're moving to ACE-Step anyway?** I lean yes
   — it's <1 day of work and it improves audio for the *interim*
   period before ACE-Step is the default. Plus MG-fallback keeps the
   MBD upgrade.
2. **What's the test for "ACE-Step is good enough"?** Suggest: blind A/B
   with Matt's ear on two ~30-second clips, one from MG+MBD, one from
   ACE-Step, both Tokyo Cyberpunk Vol. 1 source material. If ACE-Step
   wins, default flips immediately.
3. **Artist alias / business entity for Spotify?** Required before we
   can publish anywhere.
4. **LoRA training corpus consent.** ACE-Step LoRAs need source audio.
   Matt's own published catalog is fine (he owns the rights as the
   "creator" under YouTube ToS and the channel monetization). But
   training on *other artists' synthwave* is a copyright-of-training
   data question that we should *not* touch on a monetized channel.
   Stay within our own catalog.
5. **Listener agent: heuristic or smart?** Lite (Rust + aubio) is enough
   to catch dead-air takes and gross-key-drift. Smart (audio LLM with
   critique) is overkill for now. Defer the smart version.
6. **Stems / MIDI: ship as YouTube description "download link" or as
   Bandcamp/Patreon paid tier?** YouTube description links are
   trivial; Bandcamp/Patreon turn it into a small-money side
   revenue stream. Probably both, eventually.
7. **Keep Sunset Vol. 1 + Tron Vol. 1 on MG**, or retroactively
   re-render with ACE-Step? Lean no — they're shipped, sounds we
   already validated, audience already (will) hear them. Future
   albums get the new engine; the existing ones become a "historical"
   batch.

---

## 9. What This Doc Does NOT Cover (Out of Scope)

- The wgpu visualizer (N3.1) — separate roadmap item, no overlap with audio-gen
- Livestream pipeline (N2.4) — same, but will benefit from FLAC archive
- The forecast/TWC encoder polish — separate
- Symbolic *composition* (vs transcription) — Anticipatory Music
  Transformer, Music Transformer, MMM. These generate MIDI *from
  scratch* and would fundamentally rework the pipeline (MIDI → render
  via VST/SF2 → audio instead of audio → maybe MIDI for editability).
  Interesting future direction but a separate project; **not on the path**
  for fixing the current seam-and-quality complaint.

---

## 10. References

**Models surveyed:**
- ACE-Step 1.5 (MIT) — github.com/ace-step/ACE-Step-1.5
- ACE-Step 1.5 XL — same repo, April 2026 release
- ACE-Step GPU compatibility — github.com/ace-step/ACE-Step-1.5/blob/main/docs/en/GPU_COMPATIBILITY.md
- ACE-Step prompt guide — ambienceai.com, civitai.com low-vram variant
- MusicGen + Multi-Band Diffusion — facebookresearch.github.io/audiocraft/docs/MBD.html
- MusicGen-stereo-melody — huggingface.co/facebook/musicgen-stereo-melody
- DiffRhythm 2 (Apache 2.0) — github.com/ASLP-lab/DiffRhythm2 + arxiv 2510.22950
- YuE 7B (Apache 2.0) — github.com/multimodal-art-projection/YuE
- YuEGP (quantized for low VRAM) — github.com/deepbeepmeep/YuEGP
- Stable Audio Open 1.0 — huggingface.co/stabilityai/stable-audio-open-1.0 (Stable Audio Community License, NOT clean commercial)

**Stems / MIDI:**
- Demucs htdemucs_ft (MIT) — github.com/facebookresearch/demucs
- Spotify basic-pitch (MIT) — github.com/spotify/basic-pitch
- Magenta MT3 (Apache 2.0) — github.com/magenta/mt3
- FluidSynth (LGPL) — fluidsynth.org

**Internal references:**
- `crates/nightdrive-audio-gen/src/lib.rs` — current StableAudioClient + MusicGenClient
- `crates/nightdrive-llm/src/lib.rs` — current prompt template
- `.claude/agents/album-composer.md` — current album-design agent
- `musicgen-server.py` — current MG sidecar (`prev_audio_b64` continuation)
- `docs/albums/sunset-drive-vol-1.json` + `tron-drive-vol-1.json` — current album JSONs
- `HANDOFF.md` §16 (MusicGen engine landed), §18 (audiocraft Windows install), §20 (Sunset Vol. 1), §21 (Tron Vol. 1)

---

*End of deep dive. Next step pending Matt's call: do we ship Phase 0
(MBD + per-section prompts on MG) first, or jump straight to Phase 1
(ACE-Step sidecar on kokonoe)? My vote: Phase 0 today + Phase 1 in
parallel — both are low-risk and Phase 0 improves whatever is already
in the pipeline before ACE-Step lands.*
