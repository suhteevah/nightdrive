"""
wallpaper_pack.py — outpaint + upscale every nightdrive cover into a 3-aspect
wallpaper set (square 2048x2048, desktop 3840x2196 ≈ 16:9, phone 2196x3840 ≈ 9:16).

Reads:
  - assets/covers/library/cover-NN-seedXXXX.png (the random library)
  - assets/covers/albums/<slug>/track-NN.png (album sets)

Writes:
  - assets/wallpapers/library/<basename>-{square,desktop,phone}.png
  - assets/wallpapers/<slug>/<basename>-{square,desktop,phone}.png

Outpaint strategy:
  - Take the 1024² source.
  - For landscape (target 1344x768 — SDXL's nearest 1.75:1 training bucket):
    scale source down to 768x768 fit-height, mirror-reflect the edges out to
    1344px width, then run SDXL img2img at strength 0.35 with the original
    cover_prompt. The reflect-pad is already aesthetically coherent so a
    low denoise polishes the seams without rewriting the center composition.
  - For portrait (target 768x1344): same idea flipped 90°.
  - Square 2K: PIL Lanczos upscale 1024→2048. No GPU needed; sharper than
    1024 native, fits vinyl-sleeve / mug-wrap / sticker print-prep.
  - Final desktop/phone PNGs are Lanczos-upscaled from the outpainted 1344x768
    / 768x1344 to 3840x2196 / 2196x3840 (≈4K).

For "real 4K" (3840x2160 exactly) we'd crop the 2196 height down by 36px;
for "real phone 4K" we'd letterbox or accept the slight overscan. The current
output is wallpaper-grade for any 16:9 / 9:16 display.

License posture:
  - SDXL Base 1.0 — Stability AI Community License, commercial use allowed
    under the $1M revenue threshold (nightdrive is nowhere near).
  - Original prompts retained from cover gen so the outpainted edges match
    the source aesthetic (no prompt drift).

Run (kokonoe, MG sidecar NOT running):
  cd J:\\nightdrive
  & "J:\\pledgeandcrowns\\tools\\synthwave-gen\\.venv\\Scripts\\python.exe" `
      sidecar/wallpaper_pack.py --all --low-vram
"""
from __future__ import annotations

import argparse
import json
import re
import sys
import time
from pathlib import Path

import numpy as np
from PIL import Image

sys.stdout.reconfigure(encoding="utf-8", errors="replace")
sys.stderr.reconfigure(encoding="utf-8", errors="replace")

REPO_ROOT = Path(__file__).resolve().parent.parent
MODEL_ID = "stabilityai/stable-diffusion-xl-base-1.0"
DEVICE = "cuda:0"
NEGATIVE = (
    "text, watermark, signature, logo, low quality, blurry, jpeg artifacts, "
    "low resolution, deformed, ugly, distorted, cropped, frame, border, "
    "people faces, human figures with detailed faces, repeated, mirror"
)

# SDXL training-resolution buckets we target. 1.75:1 (1344x768) is the
# closest bucket to true 16:9 (1.78:1); the 36px height shortfall on 4K
# crop is invisible to the eye. Reverse for portrait.
LANDSCAPE_W, LANDSCAPE_H = 1344, 768
PORTRAIT_W, PORTRAIT_H = 768, 1344
SQUARE_OUT = 2048   # for sticker / mug / vinyl-sleeve print prep
DESKTOP_W, DESKTOP_H = 3840, 2196  # ≈4K landscape
PHONE_W, PHONE_H = 2196, 3840      # ≈4K portrait


def parse_args():
    p = argparse.ArgumentParser(description="outpaint + upscale covers to wallpaper sets")
    g = p.add_mutually_exclusive_group(required=True)
    g.add_argument("--all", action="store_true",
                   help="process every cover under assets/covers/library/ + assets/covers/albums/*/")
    g.add_argument("--slug", type=str,
                   help="just one album: --slug sunset-drive-vol-1")
    g.add_argument("--library", action="store_true",
                   help="just the 11-cover random library, no albums")
    p.add_argument("--max", type=int, default=0,
                   help="cap on covers to process (0 = all)")
    p.add_argument("--low-vram", action="store_true",
                   help="sequential CPU offload + slicing (~2-3x slower, fits in <4 GB free)")
    return p.parse_args()


def discover_sources(args) -> list[tuple[Path, Path]]:
    """Return list of (source_png_path, output_dir_path) to process."""
    sources = []
    if args.all or args.library:
        lib = REPO_ROOT / "assets" / "covers" / "library"
        for png in sorted(lib.glob("cover-*-seed*.png")):
            sources.append((png, REPO_ROOT / "assets" / "wallpapers" / "library"))
    if args.all:
        albums_root = REPO_ROOT / "assets" / "covers" / "albums"
        if albums_root.is_dir():
            for album_dir in sorted(albums_root.iterdir()):
                if album_dir.is_dir():
                    for png in sorted(album_dir.glob("track-*.png")):
                        sources.append((png, REPO_ROOT / "assets" / "wallpapers" / album_dir.name))
    elif args.slug:
        album_dir = REPO_ROOT / "assets" / "covers" / "albums" / args.slug
        if not album_dir.is_dir():
            sys.exit(f"[wallpaper-pack] album dir not found: {album_dir}")
        for png in sorted(album_dir.glob("track-*.png")):
            sources.append((png, REPO_ROOT / "assets" / "wallpapers" / args.slug))
    return sources


# 25 hand-tuned synthwave prompts (mirror of generate_cover_library.py's
# LIBRARY_PROMPTS — kept in sync for prompt-faithful outpaint).
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


def prompt_for(source: Path) -> str:
    """Look up the prompt that originally generated this cover."""
    name = source.stem
    # Album: track-NN  (parent dir is the album slug)
    m = re.match(r"^track-(\d+)$", name)
    if m:
        slug = source.parent.name
        album_json = REPO_ROOT / "docs" / "albums" / f"{slug}.json"
        if not album_json.exists():
            return _generic_prompt()
        album = json.loads(album_json.read_text(encoding="utf-8"))
        track_num = int(m.group(1))
        for t in album["tracks"]:
            if int(t["track_number"]) == track_num:
                return t["cover_prompt"]
        return _generic_prompt()
    # Library: cover-NN-seedXXXX
    m = re.match(r"^cover-(\d+)-seed\d+$", name)
    if m:
        slot = int(m.group(1)) - 1
        if 0 <= slot < len(LIBRARY_PROMPTS):
            return LIBRARY_PROMPTS[slot]
    return _generic_prompt()


def _generic_prompt() -> str:
    return ("synthwave 1985 album cover, neon palm silhouettes, magenta and orange "
            "sunset sky, chrome reflections, retro futurism, no text, no people")


def reflect_pad_to_landscape(src: Image.Image) -> Image.Image:
    """Source 1024² -> 768×768 fit-height -> reflect-pad to 1344×768 landscape."""
    src_768 = src.resize((LANDSCAPE_H, LANDSCAPE_H), Image.LANCZOS)
    arr = np.array(src_768)
    pad_each = (LANDSCAPE_W - LANDSCAPE_H) // 2  # (1344-768)/2 = 288
    arr_padded = np.pad(arr, ((0, 0), (pad_each, pad_each), (0, 0)), mode="reflect")
    return Image.fromarray(arr_padded)


def reflect_pad_to_portrait(src: Image.Image) -> Image.Image:
    """Source 1024² -> 768×768 fit-width -> reflect-pad to 768×1344 portrait."""
    src_768 = src.resize((PORTRAIT_W, PORTRAIT_W), Image.LANCZOS)
    arr = np.array(src_768)
    pad_each = (PORTRAIT_H - PORTRAIT_W) // 2
    arr_padded = np.pad(arr, ((pad_each, pad_each), (0, 0), (0, 0)), mode="reflect")
    return Image.fromarray(arr_padded)


def main():
    args = parse_args()
    sources = discover_sources(args)
    if not sources:
        sys.exit("[wallpaper-pack] no source covers found")

    if args.max > 0:
        sources = sources[: args.max]

    print(f"[wallpaper-pack] {len(sources)} cover(s) to process")

    print(f"[wallpaper-pack] loading {MODEL_ID} (fp16)...")
    t_load = time.time()
    import torch
    from diffusers import StableDiffusionXLImg2ImgPipeline, DPMSolverMultistepScheduler

    pipe = StableDiffusionXLImg2ImgPipeline.from_pretrained(
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
        print("[wallpaper-pack] low-vram mode enabled")
    else:
        pipe = pipe.to(DEVICE)
        pipe.enable_vae_slicing()
    print(f"[wallpaper-pack] model loaded in {time.time() - t_load:.1f}s")

    total_done = 0
    for idx, (src_path, out_dir) in enumerate(sources, start=1):
        basename = src_path.stem
        # Library filenames carry the seed; strip it for cleaner output names.
        m = re.match(r"^(cover-\d+)-seed\d+$", basename)
        out_base = m.group(1) if m else basename
        out_dir.mkdir(parents=True, exist_ok=True)

        # Skip if all 3 outputs already exist.
        sq = out_dir / f"{out_base}-square.png"
        dt = out_dir / f"{out_base}-desktop.png"
        ph = out_dir / f"{out_base}-phone.png"
        if sq.exists() and dt.exists() and ph.exists():
            print(f"[wallpaper-pack] [{idx:02d}/{len(sources)}] skip (all exist): {out_base}")
            continue

        print(f"[wallpaper-pack] [{idx:02d}/{len(sources)}] {out_base} <- {src_path.name}")
        t0 = time.time()
        prompt = prompt_for(src_path)
        src_img = Image.open(src_path).convert("RGB")
        assert src_img.size == (1024, 1024), f"expected 1024x1024 source, got {src_img.size}"

        # Deterministic per-source seed so re-runs are stable.
        seed_basis = sum(ord(c) for c in src_path.stem)
        gen = torch.Generator(DEVICE if not args.low_vram else "cpu").manual_seed(seed_basis * 7919 & 0x7FFFFFFF)

        # --- square 2K (pure Lanczos, no GPU) ---
        if not sq.exists():
            src_img.resize((SQUARE_OUT, SQUARE_OUT), Image.LANCZOS).save(sq)

        # --- landscape outpaint @ 1344x768 ---
        if not dt.exists():
            init_landscape = reflect_pad_to_landscape(src_img)
            result = pipe(
                prompt=prompt,
                negative_prompt=NEGATIVE,
                image=init_landscape,
                strength=0.35,
                num_inference_steps=25,
                guidance_scale=7.0,
                generator=gen,
            ).images[0]
            # Upscale to ≈4K (3840x2196), then save.
            result.resize((DESKTOP_W, DESKTOP_H), Image.LANCZOS).save(dt)

        # --- portrait outpaint @ 768x1344 ---
        if not ph.exists():
            init_portrait = reflect_pad_to_portrait(src_img)
            result = pipe(
                prompt=prompt,
                negative_prompt=NEGATIVE,
                image=init_portrait,
                strength=0.35,
                num_inference_steps=25,
                guidance_scale=7.0,
                generator=gen,
            ).images[0]
            result.resize((PHONE_W, PHONE_H), Image.LANCZOS).save(ph)

        elapsed = time.time() - t0
        total_done += 1
        print(f"[wallpaper-pack]   wrote {out_base}-{{square,desktop,phone}}.png in {elapsed:.1f}s")
        if args.low_vram:
            torch.cuda.empty_cache()

    print(f"[wallpaper-pack] done — {total_done} cover(s) packed into 3-aspect sets")


if __name__ == "__main__":
    main()
