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

import argparse
import json
import sys
import time
from pathlib import Path

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
sys.stderr.reconfigure(encoding="utf-8", errors="replace")

parser = argparse.ArgumentParser(description="generate cover-art library entries")
parser.add_argument(
    "--max", type=int, default=0,
    help="cap on new covers to generate (0 = all missing slots). Lets us "
         "back-fill the library N at a time instead of one ~25-minute run.",
)
parser.add_argument(
    "--low-vram", action="store_true",
    help="enable attention slicing + sequential CPU offload so SDXL fits in "
         "~4 GB free VRAM. Each cover takes 2-3x longer; use when other "
         "apps are competing for VRAM on the 3070 Ti.",
)
parser.add_argument(
    "--album", type=str, default=None,
    help="generate the 12 covers for an album defined in docs/albums/<slug>.json. "
         "Overrides the default library prompt list and writes to "
         "assets/covers/albums/<slug>/track-NN.png instead of assets/covers/library/. "
         "Each cover's prompt comes from the JSON's tracks[i].cover_prompt field.",
)
args = parser.parse_args()

MODEL_ID = "stabilityai/stable-diffusion-xl-base-1.0"
DEVICE = "cuda:0"
REPO_ROOT = Path(__file__).resolve().parent.parent
NEGATIVE = (
    "text, watermark, signature, logo, low quality, blurry, jpeg artifacts, "
    "low resolution, deformed, ugly, distorted, cropped, frame, border, "
    "people faces, human figures with detailed faces"
)

# 25 hand-tuned synthwave prompts for the random library. Variation across:
# time-of-day, setting, foreground subject, color palette, and stylistic era
# marker. Bigger variety per axis than per-image so the library doesn't feel
# same-y when tracks rotate through. This list is *not* used in --album mode.
LIBRARY_PROMPTS = [
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

# Now pick which prompt set + output dir we're running with. Album mode
# overrides both; default is the 25-slot random library.
if args.album:
    album_path = REPO_ROOT / "docs" / "albums" / f"{args.album}.json"
    if not album_path.exists():
        sys.exit(f"[lib-gen] album JSON not found: {album_path}")
    album = json.loads(album_path.read_text(encoding="utf-8"))
    PROMPTS = [t["cover_prompt"] for t in album["tracks"]]
    OUT_DIR = REPO_ROOT / "assets" / "covers" / "albums" / args.album
    # Per-album seed family: djb2 of the slug, then XOR per-track index in
    # the loop below. Regenerating a single track's cover is deterministic.
    _h = 5381
    for _b in args.album.encode("utf-8"):
        _h = ((_h * 33) + _b) & 0xFFFFFFFF
    ALBUM_SEED_BASE = _h
    OUT_FILENAME = lambda idx, seed: OUT_DIR / f"track-{idx:02d}.png"
    print(f"[lib-gen] album mode: {args.album} ({len(PROMPTS)} tracks)")
else:
    PROMPTS = LIBRARY_PROMPTS
    OUT_DIR = REPO_ROOT / "assets" / "covers" / "library"
    ALBUM_SEED_BASE = None
    OUT_FILENAME = lambda idx, seed: OUT_DIR / f"cover-{idx:02d}-seed{seed}.png"

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
# Use the DPMSolver scheduler for faster convergence at lower step counts.
# Default scheduler (Euler) needs ~30 steps for stable SDXL output; DPM++
# 2M Karras gets there in ~20 with comparable quality.
from diffusers import DPMSolverMultistepScheduler
pipe.scheduler = DPMSolverMultistepScheduler.from_config(
    pipe.scheduler.config, use_karras_sigmas=True, algorithm_type="dpmsolver++"
)
if args.low_vram:
    # Sequential CPU offload moves model components to GPU only while in use,
    # then back to CPU. Combined with attention + VAE slicing this fits SDXL
    # in ~4 GB of free VRAM at the cost of ~2-3x slower inference. Use when
    # Chrome/Discord/etc are holding 2-3 GB and you don't want to close them.
    pipe.enable_sequential_cpu_offload()
    pipe.enable_attention_slicing()
    pipe.enable_vae_slicing()
    print("[lib-gen] low-vram mode: sequential CPU offload + slicing enabled")
else:
    pipe = pipe.to(DEVICE)
    # VAE slicing alone (no attention slicing) keeps peak VRAM under 8 GB on
    # the 3070 Ti while keeping each step fast. The attention-slicing flag was
    # tripling inference time in the first cut.
    pipe.enable_vae_slicing()
print(f"[lib-gen] model loaded in {time.time() - t0:.1f}s")

free, total = torch.cuda.mem_get_info(0)
print(f"[lib-gen] VRAM: {(total - free) / 2**30:.2f} / {total / 2**30:.2f} GB used after load")

OUT_DIR.mkdir(parents=True, exist_ok=True)

successful = 0
new_generated = 0
for idx, prompt in enumerate(PROMPTS, start=1):
    # Library mode: seed = 1000 + slot index (matches existing cover filenames).
    # Album mode: seed = djb2(slug) XOR (idx * golden-ratio constant) — stable
    # per-track but distinct per-album.
    if ALBUM_SEED_BASE is None:
        seed = 1000 + idx
    else:
        seed = (ALBUM_SEED_BASE ^ (idx * 0x9E3779B9)) & 0x7FFFFFFF
    out_path = OUT_FILENAME(idx, seed)

    if out_path.exists():
        print(f"[lib-gen] [{idx:02d}/{len(PROMPTS)}] skip (exists): {out_path.name}")
        successful += 1
        continue

    if args.max > 0 and new_generated >= args.max:
        print(f"[lib-gen] hit --max {args.max}; stopping. {len(PROMPTS) - idx + 1} prompts unrun.")
        break

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
        new_generated += 1
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
