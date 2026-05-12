# nightdrive bench ledger

**Hardware (locked):**
- **Pre-2026-05-17:** kokonoe RTX 3070 Ti 8 GB, fp16 (Stable Audio Open peaks ~6-7 GB).
- **Post-2026-05-17:** cnc-server 3× Tesla P100 16 GB, fp32 (Pascal sm_60 has no fp16 acceleration; per `J:\llm-wiki\patterns\candle-p100-pascal-compat.md`).

**Hardware change resets baseline.** Do not compare rows across different
hardware. When the cnc P100s land, write a horizontal rule and start a fresh
table with a new caption.

**Append-only.** Never edit historical rows. If a stage is re-baselined, append a new row with a `note` flag — don't overwrite.

**Columns:**
- `date`        — YYYY-MM-DD UTC
- `sha7`        — short git sha; `(head)` if not yet a git repo
- `stage`       — N from `HANDOFF.md` §5 (0..8) or `pipeline_full` for end-to-end
- `track_id`    — fixed seed for reproducibility (default seed=1010, BPM=92, 240s)
- `wall_s`      — wall-clock seconds
- `vram_peak_mib` — peak GPU VRAM during the stage; `-` for CPU-only
- `leader`      — `nightdrive` for self-hosted runs, or comparator name on multi-system benches
- `note`        — optional free-form (regression flags, hardware notes)

---

## kokonoe RTX 3070 Ti 8 GB, fp16 (pre-cnc-P100 baseline)

| date       | sha7    | stage         | track_id          | wall_s | vram_peak_mib | leader     | note |
|------------|---------|---------------|-------------------|-------:|--------------:|------------|------|
| 2026-05-10 | (head) | 1 | nd-bench-001 | - | - | nightdrive | crate-not-shipped |
| 2026-05-10 | (head) | 2 | nd-bench-001 | - | - | nightdrive | crate-not-shipped |
| 2026-05-10 | (head) | 3 | nd-bench-001 | - | - | nightdrive | crate-not-shipped |
| 2026-05-10 | (head) | 4 | nd-bench-001 | - | - | nightdrive | crate-not-shipped |
| 2026-05-10 | (head) | 5 | nd-bench-001 | - | - | nightdrive | crate-not-shipped |
| 2026-05-10 | (head) | 6 | nd-bench-001 | - | - | nightdrive | crate-not-shipped |
| 2026-05-10 | (head) | 7 | nd-bench-001 | - | - | nightdrive | crate-not-shipped |
| 2026-05-11 | (head) | 1             | EGFUlex64L4       |     74 |             - | nightdrive | live-vod; ollama qwen2.5:7b-instruct on kokonoe |
| 2026-05-11 | (head) | 2             | EGFUlex64L4       |    244 |          6800 | nightdrive | live-vod; sao 8x35s segs, 1s xfade (seam-audible, bumped to 3s after) |
| 2026-05-11 | (head) | 3             | EGFUlex64L4       |      1 |             - | nightdrive | live-vod; sdxl-unreachable, ffmpeg-gradient fallback |
| 2026-05-11 | (head) | 4             | EGFUlex64L4       |     21 |             - | nightdrive | live-vod; ffmpeg loudnorm 2-pass -12.68->-14.0 LUFS |
| 2026-05-11 | (head) | 5             | EGFUlex64L4       |      0 |             - | nightdrive | live-vod; showwaves overlay folded into stage 6 |
| 2026-05-11 | (head) | 6             | EGFUlex64L4       |     60 |             - | nightdrive | live-vod; ffmpeg mux 67MB h264 crf18 + aac320k + faststart |
| 2026-05-11 | (head) | 7             | EGFUlex64L4       |     40 |             - | nightdrive | live-vod; yt data api v3 chunked PUT |
| 2026-05-11 | (head) | pipeline_full | EGFUlex64L4       |    440 |          6800 | nightdrive | live-vod; sao+ffmpeg-gradient+showwaves; 4m34s output, 67MB mp4 |
| 2026-05-11 | (head) | pipeline_full | FGPUo7oXCI4       |   1072 |          5000 | nightdrive | live-vod; mg-stereo-medium continuation 12 segs (1 fresh + 11 cont, 5s prefix); 57MB mp4; 2.4x sao penalty = encodec overhead |
| 2026-05-11 | (head) | pipeline_full | 2NvOEfVbv2c       |      - |             - | nightdrive | live-vod; mg+twc3panel+4city+kamx-radar+vt323+sdxllib; per-stage wall not split in §18 |
