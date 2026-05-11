"""
stable_audio_server.py — FastAPI sidecar wrapping Stable Audio Open 1.0.

The Rust nightdrive-audio-gen crate calls this over HTTP. We chain
~30-45s segments at the Rust side; this sidecar generates one segment per
POST. Single uvicorn worker (model is GPU-resident, can't be cloned cheaply).

Endpoints
  POST /generate { prompt, duration_seconds, seed, steps?, cfg_scale?,
                   negative_prompt? }
                 -> audio/wav (PCM 16-bit, model.sampling_rate Hz, stereo)
  GET  /health   -> { ok, model, device, sample_rate, vram_used_gb }

Ported from J:\\pledgeandcrowns\\tools\\synthwave-gen\\generate.py per
CLAUDE.md §"DO NOT REINVENT" / "1. Audio generation". The key bits we keep
from that reference verbatim:
  - fp16 dtype on CUDA (~6-7 GB peak on a 3070 Ti)
  - gated-repo error message that walks the user through HF_TOKEN setup
  - T5 token-length pre-flight (text encoder max=128; silently truncates
    the TAIL where "no vocals, no thrumming bass" directives live)
  - per-generation `torch.cuda.empty_cache()` after the audio tensor is
    moved CPU-side, to keep VRAM headroom stable across many requests

Run (Windows / kokonoe):
  cd J:\\nightdrive
  $env:HF_TOKEN = "<token from huggingface.co/settings/tokens>"
  & "J:\\pledgeandcrowns\\tools\\synthwave-gen\\.venv\\Scripts\\python.exe" `
      -m uvicorn sidecar.stable_audio_server:app `
      --host 127.0.0.1 --port 8080 --workers 1
"""
from __future__ import annotations

import io
import logging
import os
import sys
import time
from typing import Optional

import numpy as np
import soundfile as sf
import torch
from diffusers import StableAudioPipeline
from fastapi import FastAPI, HTTPException
from fastapi.responses import Response
from pydantic import BaseModel, Field

# Per global rule: utf-8 stdio so emoji-in-prompt logs don't blow up on Windows.
sys.stdout.reconfigure(encoding="utf-8", errors="replace")
sys.stderr.reconfigure(encoding="utf-8", errors="replace")

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s [%(levelname)s] %(name)s: %(message)s",
    datefmt="%H:%M:%S",
)
log = logging.getLogger("stable-audio-server")

MODEL_ID = os.environ.get("NIGHTDRIVE_SAO_MODEL", "stabilityai/stable-audio-open-1.0")
DEVICE = os.environ.get("NIGHTDRIVE_SAO_DEVICE", "cuda:0")
# fp16 is the documented kokonoe-on-3070-Ti default. P100s (post-2026-05-17,
# cnc) have no fp16 acceleration; flip NIGHTDRIVE_SAO_DTYPE=float32 there.
DTYPE_STR = os.environ.get("NIGHTDRIVE_SAO_DTYPE", "float16")
_DTYPE = {"float16": torch.float16, "float32": torch.float32, "bfloat16": torch.bfloat16}[DTYPE_STR]

log.info("loading %s onto %s (dtype=%s)", MODEL_ID, DEVICE, DTYPE_STR)
_t0 = time.time()
try:
    pipe = StableAudioPipeline.from_pretrained(MODEL_ID, torch_dtype=_DTYPE)
except Exception as e:  # pragma: no cover — bootstrap-only path
    msg = str(e)
    if "GatedRepoError" in repr(type(e)) or "gated" in msg.lower() or "401" in msg or "403" in msg:
        log.error(
            "Stable Audio Open 1.0 is a gated HuggingFace repo. To access:\n"
            "  1. Visit https://huggingface.co/stabilityai/stable-audio-open-1.0\n"
            "     and click `Agree and access repository` (Stability AI Community License).\n"
            "  2. Create a token at https://huggingface.co/settings/tokens (read scope is fine).\n"
            "  3. Either run `huggingface-cli login` once and paste the token, OR set\n"
            "     HF_TOKEN=<your-token> in the env before launching uvicorn.\n"
        )
        sys.exit(5)
    raise

pipe = pipe.to(DEVICE)
SAMPLE_RATE = int(pipe.vae.sampling_rate)
TEXT_ENCODER_MAX = int(pipe.tokenizer.model_max_length)
log.info(
    "model loaded in %.1fs · sample_rate=%d Hz · text_encoder_max=%d units",
    time.time() - _t0, SAMPLE_RATE, TEXT_ENCODER_MAX,
)
log.info(
    "VRAM total=%.1f GiB, free=%.1f GiB",
    torch.cuda.get_device_properties(DEVICE).total_memory / 2**30,
    torch.cuda.mem_get_info(DEVICE)[0] / 2**30,
)


app = FastAPI(title="nightdrive stable-audio-open sidecar", version="0.1.0")


# Stable Audio Open 1.0 hard limit is ~47s (the model was trained for short
# clips). Defaults below match the synthwave-gen reference manifest.
DEFAULT_STEPS = int(os.environ.get("NIGHTDRIVE_SAO_DEFAULT_STEPS", 100))
DEFAULT_CFG = float(os.environ.get("NIGHTDRIVE_SAO_DEFAULT_CFG", 7.0))
DEFAULT_NEGATIVE = os.environ.get(
    "NIGHTDRIVE_SAO_DEFAULT_NEGATIVE",
    "vocals, lyrics, singing, talking, distorted, low quality, lo-fi noise, dialogue",
)


class GenerateRequest(BaseModel):
    prompt: str = Field(min_length=1, max_length=2000)
    # 47s is SAO's trained maximum; allow up to that, default 30s segments.
    duration_seconds: float = Field(ge=4.0, le=47.0)
    seed: int = Field(default=0)
    steps: Optional[int] = None
    cfg_scale: Optional[float] = None
    negative_prompt: Optional[str] = None


@app.get("/health")
def health() -> dict:
    free, total = torch.cuda.mem_get_info(DEVICE)
    return {
        "ok": True,
        "model": MODEL_ID,
        "device": DEVICE,
        "dtype": DTYPE_STR,
        "sample_rate": SAMPLE_RATE,
        "text_encoder_max_units": TEXT_ENCODER_MAX,
        "vram_used_gb": round((total - free) / 2**30, 2),
        "vram_total_gb": round(total / 2**30, 2),
    }


@app.post("/generate")
def generate(req: GenerateRequest) -> Response:
    t0 = time.time()
    log.info(
        "generate prompt_len=%d duration_s=%.1f seed=%d steps=%s cfg=%s",
        len(req.prompt), req.duration_seconds, req.seed,
        req.steps, req.cfg_scale,
    )

    # T5 token-length pre-flight (synthwave-gen reference, lines 182-200).
    # Surface a header instead of failing — short tail-drops still produce
    # usable audio, just not exactly what the prompt asked for.
    n_tokens = pipe.tokenizer(
        req.prompt, return_tensors="pt", truncation=False,
    ).input_ids.shape[1]
    tail_dropped = max(0, n_tokens - TEXT_ENCODER_MAX)
    if tail_dropped:
        log.warning(
            "prompt is %d units, tail %d will be dropped by T5 (max %d)",
            n_tokens, tail_dropped, TEXT_ENCODER_MAX,
        )

    steps = req.steps or DEFAULT_STEPS
    cfg = req.cfg_scale if req.cfg_scale is not None else DEFAULT_CFG
    negative = req.negative_prompt or DEFAULT_NEGATIVE

    gen = torch.Generator(DEVICE).manual_seed(int(req.seed))
    try:
        audio = pipe(
            prompt=req.prompt,
            negative_prompt=negative,
            num_inference_steps=steps,
            audio_end_in_s=float(req.duration_seconds),
            num_waveforms_per_prompt=1,
            guidance_scale=cfg,
            generator=gen,
        ).audios
    except torch.cuda.OutOfMemoryError as e:
        torch.cuda.empty_cache()
        log.error("CUDA OOM during generate: %s", e)
        raise HTTPException(status_code=503, detail=f"cuda_oom: {e}")

    # (batch=1, channels, samples) -> (samples, channels) for soundfile.
    waveform = audio[0].T.float().cpu().numpy()
    elapsed = time.time() - t0

    # Encode WAV to in-memory bytes for the HTTP response.
    buf = io.BytesIO()
    sf.write(buf, waveform, SAMPLE_RATE, format="WAV", subtype="PCM_16")
    wav_bytes = buf.getvalue()

    del audio, waveform
    torch.cuda.empty_cache()

    log.info(
        "generated %.1fs of audio in %.1fs (%.1fx realtime) — %d bytes",
        req.duration_seconds, elapsed,
        req.duration_seconds / elapsed if elapsed > 0 else 0,
        len(wav_bytes),
    )
    return Response(
        content=wav_bytes,
        media_type="audio/wav",
        headers={
            "X-Nightdrive-Tail-Dropped": str(tail_dropped),
            "X-Nightdrive-Gen-Wall-Seconds": f"{elapsed:.2f}",
            "X-Nightdrive-Sample-Rate": str(SAMPLE_RATE),
        },
    )
