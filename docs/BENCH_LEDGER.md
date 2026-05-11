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
