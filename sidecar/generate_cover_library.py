"""
generate_cover_library.py — one-shot SDXL run to build the nightdrive cover
library. Not a sidecar; meant to be invoked once per "I want more covers."

Reads a hand-tuned prompt list (25 prompts spanning outrun highways, palm
trees, Miami Vice sunsets, Tron-style grids, Akira motorcycles, etc.) and
generates each at 1024×1024 with a deterministic seed. Saves PNGs to
assets/covers/library/. The orchestrator picks one per track by hashing the
track_id mod library_size — so the same track always gets the same cover,
but successive tracks rotate through the library.

## VRAM

SDXL Base 1.0 at fp16 takes ~7 GB on the 3070 Ti. STOP the MG sidecar
(which holds ~3.4 GB) before running this — kokonoe's 8 GB card can't host
both at once. Once this script exits, the model unloads and MG can be
restarted.

## Cost

Stability AI Community License (same as Stable Audio Open) — commercial use
allowed under the revenue threshold. No CC-BY-NC strike risk here.

## Run

  cd J:\\nightdrive
  & "J:\\pledgeandcrowns\\tools\\synthwave-gen\\.venv\\Scripts\\python.exe" `
      sidecar/generate_cover_library.py
"""
from __future__ import annotations

import sys
import time
from pathlib import Path

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
sys.stderr.reconfigure(encoding="utf-8", errors="replace")

MODEL_ID = "stabilityai/stable-diffusion-xl-base-1.0"
DEVICE = "cuda:0"
OUT_DIR = Path(__file__).resolve().parent.parent / "assets" / "covers" / "library"
NEGATIVE = (
    "text, watermark, signature, logo, low quality, blurry, jpeg artifacts, "
    "low resolution, deformed, ugly, distorted, cropped, frame, border, "
    "people faces, human figures with detailed faces"
)

# 25 hand-tuned synthwave prompts. Variation across: time-of-day, setting,
# foreground subject, color palette, and stylistic era marker. Bigger
# variety per axis than per-image so the library doesn't feel same-y when
# tracks rotate through.
PROMPTS = [
    "synthwave 1985 album cover, neon palm trees on a beach at sunset, magenta and orange sky, chrome typography in distance, retro futurism, no text",
    "outrun aesthetic, lone vintage red sports car driving on infinite empty highway, dusk sky transitioning from purple to pink, vanishing point chrome grid horizon, retro 80s",
    "Miami Vice synthwave, palm tree silhouettes against neon orange and pink sunset over the ocean, art deco architectural lines, 1980s nostalgia",
    "retrowave chrome wireframe grid floor extending to vanishing point, glowing magenta sun setting behind sharp pyramidal mountain silhouettes, scanlines, VHS aesthetic",
    "cyberpunk synthwave city skyline at night, towers with neon magenta and cyan windows, holographic palm trees in foreground, retro futurism, no text",
    "synthwave empty highway at night, glowing neon center line, magenta pink fog drifting across road, geometric silhouette mountains in distance, no text",
    "1985 outrun aesthetic, DeLorean-style chrome sports car driving through neon-lit tunnel, magenta and cyan light streaks blurring past windows",
    "synthwave outer space landscape, ringed planet rising over chrome wireframe terrain, distant neon stars, retro 80s sci-fi album cover aesthetic",
    "synthwave Tokyo skyline at night, neon Japanese kanji signs glowing magenta and cyan, holographic dragon coiled around skyscraper, retro futurism",
    "Tron-inspired synthwave grid, glowing electric blue and purple circuit lines extending to vanishing point under dark starlit sky",
    "retro 80s album cover, geometric triangle frame surrounding magenta setting sun, palm tree silhouettes in foreground, vaporwave aesthetic, no text",
    "synthwave coastal road at night after rain, headlights of distant car reflecting on wet pavement, neon palm trees lining the road, magenta sky",
    "Akira-inspired synthwave, futuristic motorcycle racing down neon-lit highway, magenta and orange light streaks, retro futurism, vaporwave aesthetic",
    "synthwave mountain pass at dusk, glowing road lines threading through dark mountains, purple sky with hot pink sun, lone car taillights in distance",
    "retro space station synthwave, hexagonal portholes glowing cyan and magenta, art deco trim, 1980s sci-fi book cover aesthetic",
    "synthwave Las Vegas strip at night, neon signs glowing in pink and cyan, chrome typography on hotels, palm trees, vintage cars, 1985 aesthetic",
    "vaporwave underwater scene, glowing neon coral reefs, geometric stylized fish, magenta and cyan filtered light, retro 80s aesthetic",
    "synthwave desert highway at sunset, lone red sports car driving toward distant mesa, magenta and orange sky, retro 80s outrun aesthetic",
    "1980s retro abstract album cover, layered geometric triangles and circles, neon magenta and cyan, chrome metallic accents, vanishing point composition",
    "synthwave winter scene, neon ski lift cables glowing pink against magenta snowy mountains at dusk, retro 80s aesthetic, chrome chairlift",
    "outrun aesthetic forest road at night, neon pink fog filtering through dark tall trees, distant vintage sports car headlights, retro futurism",
    "synthwave cyberpunk alley at night, neon kanji signs reflecting in puddles, holographic palm tree, magenta and cyan rain falling, retro futurism",
    "1985 synthwave album art, glowing neon pyramid floating above chrome grid floor, magenta sun behind it, retro 80s minimalist aesthetic",
    "synthwave rooftop bar at night with neon palm trees, glowing pool reflecting the magenta sky, distant city skyline, 1980s aesthetic",
    "synthwave horizon at twilight, two suns setting over chrome wireframe ocean, neon pink and orange gradient sky, retro futurism, no text or figures",
]

print(f"[lib-gen] loading {MODEL_ID} (fp16)...")
t0 = time.time()

import torch
from diffusers import StableDiffusionXLPipeline

pipe = StableDiffusionXLPipeline.from_pretrained(
    MODEL_ID,
    torch_dtype=torch.float16,
    variant="fp16",
    use_safetensors=True,
)
pipe = pipe.to(DEVICE)
# Use the DPMSolver scheduler for faster convergence at lower step counts.
# Default scheduler (Euler) needs ~30 steps for stable SDXL output; DPM++
# 2M Karras gets there in ~20 with comparable quality.
from diffusers import DPMSolverMultistepScheduler
pipe.scheduler = DPMSolverMultistepScheduler.from_config(
    pipe.scheduler.config, use_karras_sigmas=True, algorithm_type="dpmsolver++"
)
# VAE slicing alone (no attention slicing) keeps peak VRAM under 8 GB on the
# 3070 Ti while keeping each step fast. The attention-slicing flag was
# tripling inference time in the first cut.
pipe.enable_vae_slicing()
print(f"[lib-gen] model loaded in {time.time() - t0:.1f}s")

free, total = torch.cuda.mem_get_info(0)
print(f"[lib-gen] VRAM: {(total - free) / 2**30:.2f} / {total / 2**30:.2f} GB used after load")

OUT_DIR.mkdir(parents=True, exist_ok=True)

successful = 0
for idx, prompt in enumerate(PROMPTS, start=1):
    # Deterministic seed per prompt slot so re-runs produce identical covers.
    seed = 1000 + idx
    out_path = OUT_DIR / f"cover-{idx:02d}-seed{seed}.png"

    if out_path.exists():
        print(f"[lib-gen] [{idx:02d}/{len(PROMPTS)}] skip (exists): {out_path.name}")
        successful += 1
        continue

    print(f"[lib-gen] [{idx:02d}/{len(PROMPTS)}] generating: {prompt[:70]}...")
    t0 = time.time()
    try:
        gen = torch.Generator(DEVICE).manual_seed(seed)
        # DPM++ 2M Karras at 20 steps matches default scheduler @ 30 steps for
        # SDXL — ~33% wall-time savings per image at comparable quality.
        image = pipe(
            prompt=prompt,
            negative_prompt=NEGATIVE,
            width=1024,
            height=1024,
            num_inference_steps=20,
            guidance_scale=7.0,
            generator=gen,
        ).images[0]
        image.save(out_path)
        elapsed = time.time() - t0
        successful += 1
        print(f"[lib-gen]   wrote {out_path.name} in {elapsed:.1f}s")
    except torch.cuda.OutOfMemoryError as e:
        torch.cuda.empty_cache()
        print(f"[lib-gen]   OOM on slot {idx}: {e} — skipping")
        continue
    except Exception as e:
        print(f"[lib-gen]   FAILED slot {idx}: {e}")
        continue
    finally:
        torch.cuda.empty_cache()

print(f"[lib-gen] done — {successful}/{len(PROMPTS)} covers in {OUT_DIR}")
