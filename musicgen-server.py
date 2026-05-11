"""
musicgen-server.py — minimal FastAPI wrapper exposing audiocraft's MusicGen
as the HTTP endpoint that nightdrive-audio-gen calls.

Runs on supermicro (8x Tesla P40, 192GB VRAM). systemd-managed.

Endpoints
  POST /generate   { prompt, duration_seconds, seed?, prev_audio_b64? }  -> WAV bytes (audio/wav)
  GET  /health     -> { ok: true, model: "musicgen-large", device: "cuda:0" }

Install
  python3 -m venv /opt/musicgen/.venv
  source /opt/musicgen/.venv/bin/activate
  pip install fastapi uvicorn[standard] audiocraft torch torchaudio numpy soundfile

Run
  uvicorn musicgen-server:app --host 0.0.0.0 --port 8080 --workers 1

Notes
  - MusicGen-large can do ~30s per generation; nightdrive-audio-gen chains
    segments and crossfades them. The `prev_audio_b64` field exists so the
    Rust side can pass the last 4s of audio as continuation context.
  - Single worker — model is loaded once into GPU memory.
  - For a second model (Stable Audio Open) run a parallel instance on :8082.
"""

from __future__ import annotations
import base64
import io
import logging
import os
import time
from typing import Optional

import numpy as np
import soundfile as sf
import torch
from audiocraft.models import MusicGen
from fastapi import FastAPI, HTTPException
from fastapi.responses import Response
from pydantic import BaseModel, Field

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s %(levelname)s %(name)s %(message)s",
)
log = logging.getLogger("musicgen-server")

MODEL_NAME = os.getenv("MUSICGEN_MODEL", "facebook/musicgen-large")
DEVICE = os.getenv("MUSICGEN_DEVICE", "cuda:0")
SAMPLE_RATE = 32000

log.info("loading %s onto %s", MODEL_NAME, DEVICE)
_t0 = time.time()
model = MusicGen.get_pretrained(MODEL_NAME, device=DEVICE)
log.info("loaded in %.1fs", time.time() - _t0)

app = FastAPI(title="nightdrive musicgen", version="0.1.0")


class GenerateRequest(BaseModel):
    prompt: str = Field(min_length=1, max_length=2000)
    duration_seconds: int = Field(ge=4, le=30)
    seed: Optional[int] = None
    prev_audio_b64: Optional[str] = None
    guidance_scale: float = 3.0


@app.get("/health")
def health() -> dict:
    return {"ok": True, "model": MODEL_NAME, "device": DEVICE, "sample_rate": SAMPLE_RATE}


@app.post("/generate")
def generate(req: GenerateRequest) -> Response:
    t0 = time.time()
    log.info(
        "generate prompt_len=%d duration_s=%d guidance=%.2f has_prev=%s",
        len(req.prompt), req.duration_seconds, req.guidance_scale, req.prev_audio_b64 is not None,
    )

    model.set_generation_params(
        duration=req.duration_seconds,
        cfg_coef=req.guidance_scale,
    )
    if req.seed is not None:
        torch.manual_seed(req.seed)

    try:
        if req.prev_audio_b64 is None:
            wav = model.generate([req.prompt], progress=False)
        else:
            raw = base64.b64decode(req.prev_audio_b64)
            prev, sr = sf.read(io.BytesIO(raw), dtype="float32", always_2d=False)
            if sr != SAMPLE_RATE:
                raise HTTPException(400, f"prev_audio sample rate {sr} != {SAMPLE_RATE}")
            if prev.ndim == 2:
                prev = prev.mean(axis=1)
            prev_t = torch.from_numpy(prev).unsqueeze(0).unsqueeze(0).to(DEVICE)
            wav = model.generate_continuation(
                prompt=prev_t,
                prompt_sample_rate=SAMPLE_RATE,
                descriptions=[req.prompt],
                progress=False,
            )
    except Exception as e:
        log.exception("generation failed")
        raise HTTPException(500, f"generation failed: {e}")

    audio = wav[0].cpu().numpy()                      # (channels, samples)
    if audio.ndim == 1:
        audio = np.stack([audio, audio], axis=0)
    elif audio.shape[0] == 1:
        audio = np.repeat(audio, 2, axis=0)
    audio_t = audio.T                                 # (samples, channels)

    buf = io.BytesIO()
    sf.write(buf, audio_t, SAMPLE_RATE, format="WAV", subtype="PCM_16")
    buf.seek(0)

    log.info(
        "generated %d samples in %.1fs",
        audio_t.shape[0], time.time() - t0,
    )
    return Response(content=buf.getvalue(), media_type="audio/wav")
