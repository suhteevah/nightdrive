"""
acestep_server.py — FastAPI sidecar wrapping ACE-Step 1.5 (the local-music
generation model from ace-step/ACE-Step-1.5, MIT-licensed).

Why this exists alongside the MusicGen + SAO sidecars: ACE-Step generates a
**full multi-minute song in a single forward pass**. No segment chaining, no
continuation prefix, no audible 30s seams. It also accepts a structured
lyrics field that maps directly to our CompositionSpec.sections[] — the
per-section instrumentation hints that MG was throwing away become the
primary structural input to ACE-Step.

**License:** ACE-Step 1.5 is MIT-licensed. This is the path off the
CC-BY-NC strike risk we accepted with MusicGen on the monetized
NightDrive channel.

## Endpoints (same surface shape as musicgen_server.py / stable_audio_server.py)

  GET  /health    -> { ok, model, device, sample_rate, channels,
                       supports_structured_lyrics, vram_used_gb }
  POST /generate  { caption, lyrics, duration_seconds, bpm?, musical_key?,
                    seed?, guidance_scale?, inference_steps?, infer_method? }
                  -> audio/wav (PCM_16 stereo 48 kHz by default)

`caption` is the natural-language description (genre + mood + instruments +
production). `lyrics` is the section structure block:

    [Intro - pad swell + filtered arp]
    [Verse - + sub bass + soft drums]
    [Chorus - + lead + sidechain pump]
    [Bridge - stripped, only pad + bass]
    [Outro - tape stop fade]

For purely instrumental nightdrive tracks, the per-section markers replace
sung content — ACE-Step uses them as structural anchors instead.

## Model selection

Default is `ACE-Step/Ace-Step1.5` (base SFT variant, ~1.7B, fp16 ~4-6 GB
VRAM). Override via `NIGHTDRIVE_ACESTEP_CONFIG=acestep-v15-turbo` for the
fast variant. The XL (4B DiT) variant requires ≥12 GB w/ offload — usable
on a cnc P100 (16 GB) but won't fit alongside MG on the 8 GB 3070 Ti.

## Run (kokonoe)

    cd J:\\nightdrive
    & "J:\\acestep-venv\\Scripts\\python.exe" `
        -m uvicorn sidecar.acestep_server:app `
        --host 127.0.0.1 --port 8083 --workers 1

(Port 8080=lattice-server, 8081=SDXL, 8082=MusicGen, 8083=ACE-Step.)

## Install (separate venv, see scripts/install_acestep.ps1)

ACE-Step uses `uv` as its package manager and ships its own venv via
`uv sync`. Don't try to install into the synthwave-gen venv — torch and
diffusers versions conflict. See `scripts/install_acestep.ps1` for the
playbook.

## Pascal P100 note

When run on cnc P100s (sm_60), set `ACESTEP_LM_BACKEND=pt` to force the
PyTorch backend (vLLM requires sm_70+). Falls back automatically per
ACE-Step's GPU_COMPATIBILITY.md but explicit env var is faster.
"""
from __future__ import annotations

import io
import logging
import os
import sys
import tempfile
import time
from pathlib import Path
from typing import Optional

import numpy as np
import soundfile as sf
import torch
from fastapi import FastAPI, HTTPException
from fastapi.responses import Response
from pydantic import BaseModel, Field

# UTF-8 stdio per the global rule.
sys.stdout.reconfigure(encoding="utf-8", errors="replace")
sys.stderr.reconfigure(encoding="utf-8", errors="replace")

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
    datefmt="%H:%M:%S",
)
log = logging.getLogger("acestep-server")

# -----------------------------------------------------------------------------
# Configuration via env
# -----------------------------------------------------------------------------

# Path to the cloned ACE-Step-1.5 repo on disk. `uv sync` was run here.
ACESTEP_PROJECT_ROOT = os.environ.get("NIGHTDRIVE_ACESTEP_ROOT", r"J:\acestep")

# Model config to load. ACE-Step 1.5 ships several:
#   acestep-v15            base SFT 1.7B, full quality, ~6 GB VRAM
#   acestep-v15-turbo      8-step distilled, ~4 GB VRAM, much faster
#   acestep-v15-xl         4B DiT decoder, ≥12 GB VRAM
ACESTEP_CONFIG = os.environ.get("NIGHTDRIVE_ACESTEP_CONFIG", "acestep-v15-turbo")

# LM backend: "vllm" for sm_70+, "pt" for Pascal/Volta-minus
ACESTEP_LM_BACKEND = os.environ.get("ACESTEP_LM_BACKEND", "auto")
ACESTEP_LM_MODEL = os.environ.get(
    "NIGHTDRIVE_ACESTEP_LM_MODEL", "acestep-5Hz-lm-0.6B"
)

DEVICE = os.environ.get("NIGHTDRIVE_ACESTEP_DEVICE", "cuda:0")

# DiT-only mode skips the 5 Hz LM head load (saves ~1.5 GB VRAM). The lyrics
# field still gets sent but acts as a weaker structural anchor — diffusion
# attends to it via the caption pathway only. Set this on the kokonoe 3070
# Ti (8 GB) where LM+DiT can't both fit at the inference-buffer margin.
# Unset on cnc P100 (16 GB) for the full LM-conditioned path.
ACESTEP_DIT_ONLY = os.environ.get("NIGHTDRIVE_ACESTEP_DIT_ONLY", "0") in ("1", "true", "yes")
# ACE-Step 1.5 outputs 48 kHz by default. We don't downsample — the audio-master
# crate's ffmpeg loudnorm chain handles arbitrary input rates.
SAMPLE_RATE = 48000
CHANNELS = 2

# Inference defaults (override per request)
DEFAULT_INFERENCE_STEPS = int(
    os.environ.get(
        "NIGHTDRIVE_ACESTEP_STEPS",
        "8" if ACESTEP_CONFIG.endswith("turbo") else "32",
    )
)
DEFAULT_GUIDANCE_SCALE = float(os.environ.get("NIGHTDRIVE_ACESTEP_CFG", "7.0"))
DEFAULT_SHIFT = float(os.environ.get("NIGHTDRIVE_ACESTEP_SHIFT", "3.0"))
DEFAULT_INFER_METHOD = os.environ.get("NIGHTDRIVE_ACESTEP_METHOD", "ode")

# Add the ACE-Step project root to sys.path so `import acestep` works.
if ACESTEP_PROJECT_ROOT not in sys.path:
    sys.path.insert(0, ACESTEP_PROJECT_ROOT)

# -----------------------------------------------------------------------------
# ACE-Step imports + handler initialization
# -----------------------------------------------------------------------------
#
# ACE-Step's Python API is handler-based: a DiT (diffusion) handler and an LLM
# (5 Hz language model head) handler are initialized once, then `generate_music`
# is called per request. See ACE-Step-1.5/docs/en/INFERENCE.md.
#
# The exact import paths shifted across the 1.4 → 1.5 transition. We try the
# 1.5 names first, fall back to alternatives, and fail with a clear error if
# nothing works. This way the sidecar's failure mode is actionable instead of
# `ImportError: no module named 'foo'`.

ACEStepHandler = None  # type: ignore[assignment]
LLMHandler = None  # type: ignore[assignment]
GenerationParams = None  # type: ignore[assignment]
GenerationConfig = None  # type: ignore[assignment]
generate_music_fn = None  # type: ignore[assignment]

_import_errors: list[str] = []
try:
    from acestep.handler import AceStepHandler as _ACEStepHandler  # type: ignore
    from acestep.llm_inference import LLMHandler as _LLMHandler  # type: ignore
    from acestep.inference import (  # type: ignore
        GenerationParams as _GenerationParams,
        GenerationConfig as _GenerationConfig,
        generate_music as _generate_music,
    )
    ACEStepHandler = _ACEStepHandler
    LLMHandler = _LLMHandler
    GenerationParams = _GenerationParams
    GenerationConfig = _GenerationConfig
    generate_music_fn = _generate_music
    log.info("loaded ACE-Step 1.5 handler-based API (acestep.handler)")
except Exception as e:
    _import_errors.append(f"handler API: {e}")
    try:
        # Older 1.4-style pipeline class — fallback path.
        from acestep.pipeline_ace_step import ACEStepPipeline  # type: ignore
        log.warning(
            "ACE-Step handler-API import failed (%s); falling back to "
            "ACEStepPipeline. Consider upgrading the local checkout to "
            "ACE-Step 1.5 if performance is poor.",
            e,
        )
        ACEStepHandler = ACEStepPipeline  # type: ignore[assignment]
    except Exception as e2:
        _import_errors.append(f"pipeline API: {e2}")

if ACEStepHandler is None:
    msg = (
        "Could not import ACE-Step inference API from "
        f"{ACESTEP_PROJECT_ROOT}. Tried: "
        + "; ".join(_import_errors)
        + ". Verify `uv sync` succeeded in that directory and that "
        "NIGHTDRIVE_ACESTEP_ROOT points at the repo root."
    )
    log.error(msg)
    raise RuntimeError(msg)

# -----------------------------------------------------------------------------
# Model load (once at sidecar startup)
# -----------------------------------------------------------------------------

log.info(
    "loading ACE-Step config=%s lm_model=%s lm_backend=%s onto %s "
    "(project_root=%s)",
    ACESTEP_CONFIG,
    ACESTEP_LM_MODEL,
    ACESTEP_LM_BACKEND,
    DEVICE,
    ACESTEP_PROJECT_ROOT,
)
_t0 = time.time()

# Decide LM backend: honor explicit env var, else auto-pick by sm_xx.
if ACESTEP_LM_BACKEND == "auto":
    cap = torch.cuda.get_device_capability(DEVICE)
    if cap[0] < 7:
        ACESTEP_LM_BACKEND = "pt"
        log.info("sm_%d%d < 7.0 (Pascal/older) — forcing LM backend=pt", *cap)
    else:
        ACESTEP_LM_BACKEND = "vllm"
        log.info("sm_%d%d >= 7.0 — using LM backend=vllm", *cap)

dit_handler = ACEStepHandler()
if LLMHandler is not None and not ACESTEP_DIT_ONLY:
    llm_handler = LLMHandler()
else:
    if ACESTEP_DIT_ONLY:
        log.warning(
            "NIGHTDRIVE_ACESTEP_DIT_ONLY=1 — skipping 5Hz LM init "
            "(saves ~1.5 GB VRAM, lyrics-structure conditioning weaker)"
        )
    llm_handler = None

# Initialization signatures differ across the 1.5 / 1.4 lineage. Wrap in
# try-blocks so the sidecar can degrade gracefully.
try:
    dit_handler.initialize_service(
        project_root=ACESTEP_PROJECT_ROOT,
        config_path=ACESTEP_CONFIG,
        device=DEVICE,
    )
except AttributeError:
    # Older pipeline-class API — no separate initialize_service
    log.info(
        "DiT handler has no initialize_service — assuming pipeline-class API "
        "with construction-time load"
    )
except TypeError:
    # Some 1.5 patches changed kwarg names. Try positional.
    dit_handler.initialize_service(ACESTEP_PROJECT_ROOT, ACESTEP_CONFIG, DEVICE)

if llm_handler is not None:
    try:
        llm_handler.initialize(
            checkpoint_dir=str(Path(ACESTEP_PROJECT_ROOT) / "checkpoints"),
            lm_model_path=ACESTEP_LM_MODEL,
            backend=ACESTEP_LM_BACKEND,
            device=DEVICE,
        )
    except Exception as e:
        log.warning(
            "LLMHandler.initialize raised %s — continuing with DiT-only mode "
            "(lyrics structural conditioning will be weaker)",
            e,
        )
        llm_handler = None

log.info("model + handlers loaded in %.1fs", time.time() - _t0)

free, total = torch.cuda.mem_get_info(DEVICE)
log.info(
    "VRAM total=%.2f GiB · free=%.2f GiB · used=%.2f GiB",
    total / 2**30,
    free / 2**30,
    (total - free) / 2**30,
)

# -----------------------------------------------------------------------------
# FastAPI surface
# -----------------------------------------------------------------------------

app = FastAPI(title="nightdrive acestep sidecar", version="0.1.0")


class GenerateRequest(BaseModel):
    caption: str = Field(min_length=1, max_length=512)
    # Structured lyrics with [Section - notes] blocks. For instrumental
    # tracks use [Instrumental], [Intro - <hint>], [Verse - <hint>], etc.
    # ACE-Step uses these as structural anchors, not literal singing.
    lyrics: str = Field(default="[Instrumental]")
    duration_seconds: float = Field(ge=10.0, le=600.0)
    bpm: Optional[int] = Field(default=None, ge=30, le=300)
    musical_key: Optional[str] = None
    seed: int = Field(default=-1)
    guidance_scale: Optional[float] = Field(default=None, ge=1.0, le=15.0)
    inference_steps: Optional[int] = Field(default=None, ge=4, le=128)
    shift: Optional[float] = Field(default=None, ge=1.0, le=5.0)
    infer_method: Optional[str] = None  # "ode" or "sde"


@app.get("/health")
def health() -> dict:
    free, total = torch.cuda.mem_get_info(DEVICE)
    return {
        "ok": True,
        "model": ACESTEP_CONFIG,
        "lm_model": ACESTEP_LM_MODEL if llm_handler is not None else None,
        "lm_backend": ACESTEP_LM_BACKEND if llm_handler is not None else "disabled",
        "device": DEVICE,
        "sample_rate": SAMPLE_RATE,
        "channels": CHANNELS,
        "supports_structured_lyrics": llm_handler is not None,
        "vram_used_gb": round((total - free) / 2**30, 2),
        "vram_total_gb": round(total / 2**30, 2),
    }


@app.post("/generate")
def generate(req: GenerateRequest) -> Response:
    t0 = time.time()
    log.info(
        "generate caption_len=%d lyrics_lines=%d duration_s=%.1f bpm=%s key=%s seed=%d",
        len(req.caption),
        req.lyrics.count("\n") + 1,
        req.duration_seconds,
        req.bpm,
        req.musical_key,
        req.seed,
    )

    # If the handler-based API is loaded, use it. Otherwise (older
    # ACEStepPipeline fallback), we'll need a different code path.
    if generate_music_fn is None or GenerationParams is None or GenerationConfig is None:
        return _generate_via_pipeline(req, t0)

    params = GenerationParams(
        caption=req.caption,
        lyrics=req.lyrics,
        bpm=req.bpm,
        duration=float(req.duration_seconds),
        keyscale=req.musical_key,
        inference_steps=req.inference_steps or DEFAULT_INFERENCE_STEPS,
        guidance_scale=req.guidance_scale or DEFAULT_GUIDANCE_SCALE,
        shift=req.shift or DEFAULT_SHIFT,
        infer_method=req.infer_method or DEFAULT_INFER_METHOD,
        seed=req.seed,
    )
    config = GenerationConfig(
        batch_size=1,
        audio_format="wav",
        use_random_seed=(req.seed < 0),
        seeds=None if req.seed < 0 else [req.seed],
    )

    with tempfile.TemporaryDirectory() as td:
        try:
            result = generate_music_fn(
                dit_handler, llm_handler, params, config, save_dir=td
            )
        except torch.cuda.OutOfMemoryError as e:
            torch.cuda.empty_cache()
            log.error("CUDA OOM during generate: %s", e)
            raise HTTPException(status_code=503, detail=f"cuda_oom: {e}")
        except Exception as e:
            log.exception("generation failed")
            raise HTTPException(500, f"generation failed: {e}")

        if not getattr(result, "success", True):
            err = getattr(result, "error", "unknown")
            raise HTTPException(500, f"acestep returned failure: {err}")
        audios = getattr(result, "audios", None) or []
        if not audios:
            raise HTTPException(500, "acestep returned no audio outputs")
        # Prefer the in-memory tensor; fall back to file path if not present.
        first = audios[0]
        sr_out = int(first.get("sample_rate") or SAMPLE_RATE)
        tensor = first.get("tensor")
        if tensor is not None:
            audio = tensor.cpu().numpy()  # (channels, samples) float32
            if audio.ndim == 1:
                audio = np.stack([audio, audio], axis=0)
            elif audio.shape[0] == 1:
                audio = np.repeat(audio, 2, axis=0)
            audio_t = audio.T  # (samples, channels)
        else:
            path = first.get("path")
            if not path or not Path(path).exists():
                raise HTTPException(500, "acestep result has neither tensor nor path")
            audio_t, sr_out = sf.read(path, dtype="float32", always_2d=True)

        # Ensure stereo, encode as PCM_16.
        if audio_t.ndim == 1:
            audio_t = np.stack([audio_t, audio_t], axis=1)
        if audio_t.shape[1] == 1:
            audio_t = np.repeat(audio_t, 2, axis=1)

        buf = io.BytesIO()
        sf.write(buf, audio_t, sr_out, format="WAV", subtype="PCM_16")
        wav_bytes = buf.getvalue()
        elapsed = time.time() - t0
        log.info(
            "generated %.1fs of audio in %.1fs (%.2fx realtime) · %d bytes",
            req.duration_seconds,
            elapsed,
            req.duration_seconds / elapsed if elapsed > 0 else 0,
            len(wav_bytes),
        )
        return Response(
            content=wav_bytes,
            media_type="audio/wav",
            headers={
                "X-Nightdrive-Gen-Wall-Seconds": f"{elapsed:.2f}",
                "X-Nightdrive-Sample-Rate": str(sr_out),
                "X-Nightdrive-Channels": str(audio_t.shape[1]),
                "X-Nightdrive-Engine": "ace_step",
                "X-Nightdrive-Model": ACESTEP_CONFIG,
                "X-Nightdrive-Inference-Steps": str(
                    req.inference_steps or DEFAULT_INFERENCE_STEPS
                ),
            },
        )


def _generate_via_pipeline(req: GenerateRequest, t0: float) -> Response:
    """Fallback for older ACEStepPipeline API (no generate_music function)."""
    pipeline = dit_handler  # ACEStepPipeline instance via fallback path
    log.info("using pipeline-class fallback path")
    try:
        with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as tf:
            out_path = tf.name
        kwargs = dict(
            prompt=req.caption,
            lyrics=req.lyrics,
            audio_duration=float(req.duration_seconds),
            infer_step=req.inference_steps or DEFAULT_INFERENCE_STEPS,
            guidance_scale=req.guidance_scale or DEFAULT_GUIDANCE_SCALE,
            scheduler_type="euler",
            cfg_type="apg",
            omega_scale=10.0,
            manual_seeds=str(req.seed) if req.seed >= 0 else None,
            output_path=out_path,
            format="wav",
        )
        pipeline(**kwargs)
        audio_t, sr_out = sf.read(out_path, dtype="float32", always_2d=True)
        if audio_t.shape[1] == 1:
            audio_t = np.repeat(audio_t, 2, axis=1)
        buf = io.BytesIO()
        sf.write(buf, audio_t, sr_out, format="WAV", subtype="PCM_16")
        wav_bytes = buf.getvalue()
        elapsed = time.time() - t0
        log.info(
            "pipeline-fallback generated %.1fs in %.1fs · %d bytes",
            req.duration_seconds,
            elapsed,
            len(wav_bytes),
        )
        try:
            os.unlink(out_path)
        except OSError:
            pass
        return Response(
            content=wav_bytes,
            media_type="audio/wav",
            headers={
                "X-Nightdrive-Gen-Wall-Seconds": f"{elapsed:.2f}",
                "X-Nightdrive-Sample-Rate": str(sr_out),
                "X-Nightdrive-Channels": str(audio_t.shape[1]),
                "X-Nightdrive-Engine": "ace_step",
                "X-Nightdrive-Model": ACESTEP_CONFIG,
                "X-Nightdrive-Fallback": "pipeline",
            },
        )
    except torch.cuda.OutOfMemoryError as e:
        torch.cuda.empty_cache()
        raise HTTPException(status_code=503, detail=f"cuda_oom: {e}")
    except Exception as e:
        log.exception("pipeline-fallback generation failed")
        raise HTTPException(500, f"generation failed: {e}")
