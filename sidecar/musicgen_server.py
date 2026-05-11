"""
musicgen_server.py — FastAPI sidecar wrapping Meta's MusicGen via audiocraft.

Why this exists despite SAO already working: MusicGen has native
`generate_continuation(prompt=prev_audio, ...)` which produces a genuine
extension of the prior audio rather than the blind crossfade chain that SAO
forces us into. The result is seamless multi-minute output where SAO has an
audible timbre shift every ~34s.

**License note:** MusicGen weights are CC-BY-NC. Matt has accepted the
license-strike risk for the NightDrive channel on 2026-05-11 (see
`.claude/projects/J--nightdrive/memory/project_musicgen_commercial_risk_accepted.md`).
Don't surface this tradeoff in normal operation — the decision is recorded.

## Endpoints (identical surface to sidecar/stable_audio_server.py)

  GET  /health    -> { ok, model, device, sample_rate, channels,
                       max_segment_seconds, vram_used_gb, supports_continuation }
  POST /generate  { prompt, duration_seconds, seed, prev_audio_b64? }
                  -> audio/wav

The `prev_audio_b64` field is what makes this sidecar different from SAO's —
when provided, MusicGen extends that audio rather than starting fresh. The
Rust audio-gen client uses this for true continuation chaining.

## Model selection

Default is `facebook/musicgen-stereo-medium` (~3 GB fp16, native stereo at
32 kHz, fits the kokonoe 3070 Ti alongside potential Ollama LLM load).
Override via `NIGHTDRIVE_MG_MODEL=facebook/musicgen-stereo-large` if there's
spare VRAM (~7 GB fp16, won't fit alongside qwen2.5).

## Run (kokonoe)

  cd J:\\nightdrive
  & "J:\\pledgeandcrowns\\tools\\synthwave-gen\\.venv\\Scripts\\python.exe" `
      -m uvicorn sidecar.musicgen_server:app `
      --host 127.0.0.1 --port 8080 --workers 1
"""
from __future__ import annotations

import base64
import io
import logging
import os
import sys
import time
from typing import Optional

import numpy as np
import soundfile as sf
import torch
from audiocraft.models import MusicGen
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
log = logging.getLogger("musicgen-server")

MODEL_NAME = os.environ.get("NIGHTDRIVE_MG_MODEL", "facebook/musicgen-stereo-medium")
DEVICE = os.environ.get("NIGHTDRIVE_MG_DEVICE", "cuda:0")
# MusicGen's audiocraft API doesn't expose a "dtype" kwarg directly — the
# model gets loaded at whatever precision audiocraft picked. On a 3070 Ti the
# stereo-medium weights load as fp16 by default. If we ever target a P100
# (sm_60, no fp16 acceleration) we should flip torch.set_default_dtype.

log.info("loading %s onto %s", MODEL_NAME, DEVICE)
_t0 = time.time()
model = MusicGen.get_pretrained(MODEL_NAME, device=DEVICE)
log.info("model loaded in %.1fs", time.time() - _t0)

# audiocraft's MusicGen.sample_rate is the model's native output rate (32000
# for v1, 32000 for stereo variants). Pull dynamically so we don't drift.
SAMPLE_RATE = int(model.sample_rate)
# Stereo models output 2 channels; mono models output 1. Detect via a dummy
# generation? Cleaner: introspect the model.compression_model.channels attr.
try:
    CHANNELS = int(model.compression_model.channels)
except Exception:
    CHANNELS = 1
# MusicGen's transformer context is ~30s. Past that the model gets unstable.
MAX_SEGMENT_SECONDS = 30

log.info(
    "sample_rate=%d Hz · channels=%d · max_segment=%ds",
    SAMPLE_RATE, CHANNELS, MAX_SEGMENT_SECONDS,
)
log.info(
    "VRAM total=%.2f GiB · free=%.2f GiB",
    torch.cuda.get_device_properties(DEVICE).total_memory / 2**30,
    torch.cuda.mem_get_info(DEVICE)[0] / 2**30,
)

app = FastAPI(title="nightdrive musicgen sidecar", version="0.1.0")


DEFAULT_GUIDANCE = float(os.environ.get("NIGHTDRIVE_MG_DEFAULT_CFG", 3.0))


class GenerateRequest(BaseModel):
    prompt: str = Field(min_length=1, max_length=2000)
    duration_seconds: float = Field(ge=4.0, le=float(MAX_SEGMENT_SECONDS))
    seed: int = Field(default=0)
    prev_audio_b64: Optional[str] = None
    cfg_scale: Optional[float] = None


@app.get("/health")
def health() -> dict:
    free, total = torch.cuda.mem_get_info(DEVICE)
    return {
        "ok": True,
        "model": MODEL_NAME,
        "device": DEVICE,
        "sample_rate": SAMPLE_RATE,
        "channels": CHANNELS,
        "max_segment_seconds": MAX_SEGMENT_SECONDS,
        "supports_continuation": True,
        "vram_used_gb": round((total - free) / 2**30, 2),
        "vram_total_gb": round(total / 2**30, 2),
    }


@app.post("/generate")
def generate(req: GenerateRequest) -> Response:
    t0 = time.time()
    has_prev = req.prev_audio_b64 is not None
    log.info(
        "generate prompt_len=%d duration_s=%.1f seed=%d has_prev=%s",
        len(req.prompt), req.duration_seconds, req.seed, has_prev,
    )

    cfg = req.cfg_scale if req.cfg_scale is not None else DEFAULT_GUIDANCE
    model.set_generation_params(
        duration=float(req.duration_seconds),
        cfg_coef=cfg,
    )
    # MusicGen uses a global torch RNG seed for reproducibility.
    torch.manual_seed(int(req.seed))
    torch.cuda.manual_seed_all(int(req.seed))

    try:
        if not has_prev:
            wav = model.generate([req.prompt], progress=False)
        else:
            prev_bytes = base64.b64decode(req.prev_audio_b64)
            prev, sr = sf.read(io.BytesIO(prev_bytes), dtype="float32", always_2d=True)
            if sr != SAMPLE_RATE:
                raise HTTPException(
                    400,
                    f"prev_audio sample_rate {sr} != model sample_rate {SAMPLE_RATE} "
                    f"(client must resample before sending)",
                )
            # audiocraft expects shape (batch, channels, samples). soundfile
            # gave us (samples, channels); transpose then unsqueeze.
            prev_arr = prev.T
            # If the model is mono but the input is stereo (or vice versa),
            # collapse to match. MG stereo models accept stereo prefix; mono
            # models accept mono prefix.
            if prev_arr.shape[0] != CHANNELS:
                if CHANNELS == 1:
                    prev_arr = prev_arr.mean(axis=0, keepdims=True)
                else:
                    # Mono prefix into a stereo model — duplicate the channel.
                    prev_arr = np.repeat(prev_arr, CHANNELS, axis=0)
            prev_t = torch.from_numpy(prev_arr).unsqueeze(0).to(DEVICE)
            wav = model.generate_continuation(
                prompt=prev_t,
                prompt_sample_rate=SAMPLE_RATE,
                descriptions=[req.prompt],
                progress=False,
            )
    except torch.cuda.OutOfMemoryError as e:
        torch.cuda.empty_cache()
        log.error("CUDA OOM during generate: %s", e)
        raise HTTPException(status_code=503, detail=f"cuda_oom: {e}")
    except HTTPException:
        raise
    except Exception as e:
        log.exception("generation failed")
        raise HTTPException(500, f"generation failed: {e}")

    # wav shape: (batch=1, channels, samples)
    audio = wav[0].cpu().numpy()  # (channels, samples)
    audio_t = audio.T              # (samples, channels)
    if audio_t.ndim == 1:
        audio_t = np.stack([audio_t, audio_t], axis=1)  # force stereo

    buf = io.BytesIO()
    sf.write(buf, audio_t, SAMPLE_RATE, format="WAV", subtype="PCM_16")
    wav_bytes = buf.getvalue()

    elapsed = time.time() - t0
    log.info(
        "generated %.1fs of audio in %.1fs (%.2fx realtime) · %d bytes · "
        "continuation=%s",
        req.duration_seconds, elapsed,
        req.duration_seconds / elapsed if elapsed > 0 else 0,
        len(wav_bytes), has_prev,
    )

    return Response(
        content=wav_bytes,
        media_type="audio/wav",
        headers={
            "X-Nightdrive-Continuation": "1" if has_prev else "0",
            "X-Nightdrive-Gen-Wall-Seconds": f"{elapsed:.2f}",
            "X-Nightdrive-Sample-Rate": str(SAMPLE_RATE),
            "X-Nightdrive-Channels": str(audio_t.shape[1] if audio_t.ndim == 2 else 1),
        },
    )
