# Candle backend exploration — should nightdrive port MusicGen off Python?

**Date:** 2026-05-11
**Author:** (autonomous research pass)
**Recommendation:** **Defer.** Keep the Python audiocraft sidecar. Revisit only when upstream candle ships a working MusicGen `generate()` with audio-prior continuation, or when one of the gating issues below closes.

---

## Why this question came up

Track #2 (`FGPUo7oXCI4`, MusicGen continuation) took 17m 52s wall on a kokonoe
3070 Ti for a 4-minute track. The audiocraft Windows install was painful
enough to get its own memory entry (`reference_audiocraft_windows_install.md`)
— stub `xformers` package, `av --only-binary :all:`, torch 2.5.1+cu121
force-reinstall.

A Rust-native candle backend would, in theory:

1. Eliminate the Python venv install/maintenance cost.
2. Ship as a single Rust binary inside the nightdrive workspace.
3. Maybe run faster (no Python overhead, no Tensor Cores idle on Pascal).

The deciding question: **how much of MusicGen + EnCodec + T5 + the
generation loop is already implemented in upstream candle, and how much
would nightdrive have to write from scratch?**

## Findings

### `candle-examples/examples/musicgen/` — half-baked

- `musicgen_model.rs` (~400 LOC) implements the decoder architecture.
- `main.rs` only runs the **text encoder**: tokenize prompt, print
  embeddings, exit. **No audio is produced.**
- Target is `facebook/musicgen-small` (mono). Stereo variants and the
  32 kHz EnCodec are not wired.
- `prepare_decoder_attention_mask` is literally `todo!()`.
- T5 prompt encoding is not used — the example uses a vanilla tokenizer.

### Continuation: not implemented

- The decoder's `forward(input_ids)` accepts no audio-prior tokens.
- There is no `generate_continuation` path.
- **This is the deal-breaker for nightdrive.** Without continuation,
  we'd be back to SAO-style blind crossfade chaining, which is the
  thing MusicGen was brought in to fix (track #1 had audible seams every
  ~34s; track #2 with MG continuation has no seams).

### EnCodec: yes, but 24 kHz default

- `candle-transformers/src/models/encodec.rs` (~700 LOC) implements
  encode + decode. Used in production by Parler-TTS and MetaVoice.
- **Default config is 24 kHz** (the speech variant). MusicGen uses the
  32 kHz music variant (`facebook/encodec_32khz`). Adapting it is
  config-level work, not a rewrite.
- Two minor caveats: `PadMode::Reflect` is unsupported (falls back to
  `Replicate`), and ResNet dilations have a `TODO: Apply dilations!`.

### Upstream maturity signals

- **Issue [#975](https://github.com/huggingface/candle/issues/975)**
  ("AudioGen/MusicGen", Sept 2023) is still open, labeled
  `help wanted`. No maintainer engagement.
- **PR [#2145](https://github.com/huggingface/candle/pull/2145)**
  ("Musicgen forward implementation", April 2024) is open and
  unmerged after ~13 months. A Dec 2024 comment asks "did you add
  generate method?" implying the PR is forward-pass only — no
  sampling loop, no continuation.
- No published benchmark of candle MusicGen vs PyTorch audiocraft.
  General signal: candle ~4× slower than PyTorch on RealESRGAN with
  cuDNN. Unlikely to beat audiocraft on speed.

### Pascal sm_60 / P100 compat

- Mainline candle assumes sm_70+ in `reduce.cu` (atomicAdd on `__half`)
  and uses WMMA tensor-core kernels in MoE paths.
- Matt's local fork at `J:\candle` has the three documented patches
  (`J:\llm-wiki\patterns\candle-p100-pascal-compat.md`).
- MusicGen is dense (no MoE), so only the `reduce.cu` patch matters —
  but we'd still ship the fork on cnc post-2026-05-17.
- Kokonoe (3070 Ti, sm_86) runs mainline.

## Effort estimate (if we did port)

What would have to be written from scratch:

1. **Generation loop:** sampling (top-k, temperature, classifier-free
   guidance with 3.0 default), delay-pattern token interleaving (the
   "K=4 codebook stagger" that MusicGen depends on).
2. **Stereo codebook handling:** stereo variants double the codebook
   count and need correct stagger.
3. **EnCodec 32 kHz wiring:** load the music-variant weights, plumb
   through the audio-output path.
4. **T5 conditioning:** load T5-base, condition the decoder cross-attention
   on T5 embeddings.
5. **Audio-prior continuation:** EnCodec-encode prefix WAV to discrete
   tokens, feed as `input_ids[:, :prefix_len]`, then sample the
   remainder. This is the critical-path feature.
6. **CFG batching:** classifier-free guidance doubles the batch for the
   uncond pass.
7. **WAV in/out:** read PCM int16, write PCM int16 (we have hound).

Honest estimate: **3–6 weeks of focused Rust work** tracking a model
architecture that audiocraft already ships correctly. And the performance
ceiling is likely **worse than PyTorch**, not better.

## Counter-factuals worth watching

Re-open this exploration if any of the following happen:

- candle PR #2145 lands with a `generate()` method.
- A third-party crate publishes a working candle MusicGen with
  continuation (search `cargo-search musicgen` quarterly).
- A different audio model with a clean Rust port emerges (Stable Audio
  Open 1.1, AudioBox, etc.). The `AudioGenerator` trait in
  `crates/nightdrive-audio-gen/src/lib.rs` makes swapping engines a
  ~1-day job; the sidecar is the gnarly part.

## Decision

**Keep the Python audiocraft sidecar.** The install pain is real but
one-time. The seam quality of MusicGen continuation is worth the wall
time cost (17m vs SAO's 7m for a 4-min track, but no listener
complaints vs every-34s seam complaints).

If the cnc P100s land 2026-05-17 and give us multi-GPU headroom for
parallel rendering, the wall-time concern fades — generate two tracks
at once on two cards, total throughput doubles without touching the
inference path.

## References

- `J:\nightdrive\sidecar\musicgen_server.py` — current Python sidecar
- `J:\nightdrive\crates\nightdrive-audio-gen\src\lib.rs` — `MusicGenClient` trait impl
- [candle musicgen example](https://github.com/huggingface/candle/tree/main/candle-examples/examples/musicgen)
- [candle issue #975](https://github.com/huggingface/candle/issues/975)
- [candle PR #2145](https://github.com/huggingface/candle/pull/2145)
- [candle encodec.rs](https://github.com/huggingface/candle/blob/main/candle-transformers/src/models/encodec.rs)
- `J:\llm-wiki\patterns\candle-p100-pascal-compat.md`
- `~/.claude/projects/J--nightdrive/memory/reference_audiocraft_windows_install.md`
- `~/.claude/projects/J--nightdrive/memory/project_musicgen_commercial_risk_accepted.md`
