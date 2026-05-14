"""
generate_album_covers_native.py — generate album covers at 3 native SDXL
aspect-ratio training buckets in one pass, no outpainting later.

For each track in `docs/albums/<slug>.json`, produces:
  - assets/covers/albums/<slug>/track-NN.png           (1024×1024, album cover)
  - assets/covers/albums/<slug>/track-NN-desktop.png   (1344×768, wallpaper)
  - assets/covers/albums/<slug>/track-NN-phone.png     (768×1344, phone wallpaper)

All three use the same `cover_prompt` + seed-family-derived per-track seed,
just different SDXL training-resolution buckets. SDXL natively supports
1344x768 (1.75:1, closest bucket to 16:9) and 768x1344 (the 9:16 mirror),
so this avoids the outpaint pass entirely. Output is sharper at the wallpaper
aspect ratios than outpaint+upscale because each is generated in-bucket.

Per memory/feedback_future_albums_native_aspect_gen.md (Matt 2026-05-12):
"for Vol. 2+ albums, generate covers at 1024² + 1344×768 + 768×1344 from
scratch via SDXL's native training buckets; don't outpaint after the fact
for new work."

Run (kokonoe, MG sidecar NOT running — needs ~6 GB VRAM headroom):
  cd J:\\nightdrive
  & "J:\\pledgeandcrowns\\tools\\synthwave-gen\\.venv\\Scripts\\python.exe" `
      sidecar/generate_album_covers_native.py --slug tron-drive-vol-1 --low-vram

Wall budget (low-vram, kokonoe 3070 Ti baseline):
  ~3 × 45s = ~135s per track × 12 tracks ≈ 27 min + model load
"""
from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
sys.stderr.reconfigure(encoding="utf-8", errors="replace")

parser = argparse.ArgumentParser(description="generate per-track album covers at 3 native SDXL aspect ratios")
parser.add_argument("--slug", required=True, help="album slug, matches docs/albums/<slug>.json")
parser.add_argument(
    "--max", type=int, default=0,
    help="cap on new tracks to process (0 = all). Useful for incremental runs.",
)
parser.add_argument(
    "--low-vram", action="store_true",
    help="sequential CPU offload + slicing (~2-3x slower, fits in <4 GB free)",
)
parser.add_argument(
    "--from-track", type=int, default=1, help="start at this track number (1-indexed)",
)
parser.add_argument(
    "--to-track", type=int, default=0, help="stop at this track number (inclusive). 0 = run through end",
)
args = parser.parse_args()

REPO_ROOT = Path(__file__).resolve().parent.parent
MODEL_ID = "stabilityai/stable-diffusion-xl-base-1.0"
DEVICE = "cuda:0"
NEGATIVE = (
    "text, watermark, signature, logo, low quality, blurry, jpeg artifacts, "
    "low resolution, deformed, ugly, distorted, cropped, frame, border, "
    "people faces, human figures with detailed faces"
)

# SDXL native training buckets we render to. 1024² for album thumbnail / YT
# cover. 1344×768 = SDXL's 1.75:1 bucket, the closest in-distribution match
# to 16:9 (1.78:1) — sharper than outpaint-then-crop. 768×1344 = mirror.
ASPECTS = [
    ("square",  1024, 1024),
    ("desktop", 1344,  768),
    ("phone",    768, 1344),
]

album_path = REPO_ROOT / "docs" / "albums" / f"{args.slug}.json"
if not album_path.exists():
    sys.exit(f"[album-native] album JSON not found: {album_path}")
album = json.loads(album_path.read_text(encoding="utf-8"))
tracks = album["tracks"]
OUT_DIR = REPO_ROOT / "assets" / "covers" / "albums" / args.slug
OUT_DIR.mkdir(parents=True, exist_ok=True)

# Per-album seed family: djb2 of the slug.
_h = 5381
for _b in args.slug.encode("utf-8"):
    _h = ((_h * 33) + _b) & 0xFFFFFFFF
ALBUM_SEED_BASE = _h


def seed_for(track_num: int, aspect_idx: int) -> int:
    # Same per-track seed across aspects so the composition is consistent
    # in *intent* even if the framing changes — but each track has a
    # distinct seed. (aspect_idx differentiates the three renders only via
    # a small bit-mix so they don't collide if a user ever wants to seed
    # them differently.)
    return (ALBUM_SEED_BASE ^ (track_num * 0x9E3779B9) ^ (aspect_idx * 0xDEADBEEF)) & 0x7FFFFFFF


print(f"[album-native] loading {MODEL_ID} (fp16)...")
t_load = time.time()
import torch
from diffusers import StableDiffusionXLPipeline, DPMSolverMultistepScheduler

pipe = StableDiffusionXLPipeline.from_pretrained(
    MODEL_ID,
    torch_dtype=torch.float16,
    variant="fp16",
    use_safetensors=True,
)
pipe.scheduler = DPMSolverMultistepScheduler.from_config(
    pipe.scheduler.config, use_karras_sigmas=True, algorithm_type="dpmsolver++"
)
if args.low_vram:
    pipe.enable_sequential_cpu_offload()
    pipe.enable_attention_slicing()
    pipe.enable_vae_slicing()
    print("[album-native] low-vram: sequential CPU offload + slicing")
else:
    pipe = pipe.to(DEVICE)
    pipe.enable_vae_slicing()
print(f"[album-native] model loaded in {time.time() - t_load:.1f}s")

total_done = 0
for t in tracks:
    track_num = int(t["track_number"])
    if track_num < args.from_track:
        continue
    if args.to_track > 0 and track_num > args.to_track:
        continue
    if args.max > 0 and total_done >= args.max:
        print(f"[album-native] hit --max {args.max}; stopping.")
        break

    title = t["title"]
    prompt = t["cover_prompt"]
    print(f"[album-native] track {track_num:02} '{title}'")

    for aspect_idx, (name, w, h) in enumerate(ASPECTS):
        suffix = "" if name == "square" else f"-{name}"
        out_path = OUT_DIR / f"track-{track_num:02}{suffix}.png"
        if out_path.exists():
            print(f"  skip (exists): {out_path.name}")
            continue
        seed = seed_for(track_num, aspect_idx)
        t0 = time.time()
        try:
            gen = torch.Generator(DEVICE if not args.low_vram else "cpu").manual_seed(seed)
            image = pipe(
                prompt=prompt,
                negative_prompt=NEGATIVE,
                width=w,
                height=h,
                num_inference_steps=20,
                guidance_scale=7.0,
                generator=gen,
            ).images[0]
            image.save(out_path)
            elapsed = time.time() - t0
            print(f"  {name:>7} {w}x{h} -> {out_path.name} in {elapsed:.1f}s")
        except torch.cuda.OutOfMemoryError as e:
            torch.cuda.empty_cache()
            print(f"  OOM on {name}: {e} — skipping")
            continue
        except Exception as e:
            print(f"  FAILED {name}: {e}")
            continue
        finally:
            if args.low_vram:
                torch.cuda.empty_cache()
    total_done += 1

print(f"[album-native] done — {total_done} track(s) processed in {OUT_DIR}")
