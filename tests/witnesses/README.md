# tests/witnesses/

Real-endpoint integration tests. Every file in this directory tags itself
with `// stage: N` matching `HANDOFF.md` §5 (stages 0-8). The
`coverage-matrix` agent reads these tags; `roadmap-tracker` counts them
per phase; `audit.ps1` section [4/7] reports the per-stage totals;
`witness-test-author` produces new ones on demand.

## The `// stage: N` convention

Every witness test starts with this header (in the first 10 lines of the
file so the audit grep finds it):

```rust
// stage: 4
// expect: master.flac written, target_lufs within ±0.5 of -14.0
// requires: ffmpeg (any recent version with loudnorm filter)
//
// Proves the loudnorm two-pass chain in nightdrive-audio-master::run
// produces a master.flac that measures within tolerance of the
// configured target. Hits real ffmpeg via subprocess; no mocks.
#[tokio::test]
async fn loudnorm_hits_target() {
    // ...
}
```

Stage table (mirrors `HANDOFF.md` §5):

| N | Stage              | Primary crate              |
|---|--------------------|----------------------------|
| 0 | Trigger / pipeline | `nightdrive-orchestrator`  |
| 1 | Composition spec   | `nightdrive-llm`           |
| 2 | Audio generation   | `nightdrive-audio-gen`     |
| 3 | Cover art          | `nightdrive-art`           |
| 4 | Audio mastering    | `nightdrive-audio-master`  |
| 5 | Visualizer         | `nightdrive-visuals`       |
| 6 | Final encode       | `nightdrive-encoder`       |
| 7 | Publish            | `nightdrive-youtube`       |
| 8 | Livestream         | `nightdrive-orchestrator`  |

A test that legitimately exercises multiple stages tags with the
**latest** stage it observes. A test that proves cross-stage glue
(e.g. orchestrator dispatch) tags `// stage: 0`.

## The no-mocks rule

Witness tests **hit the real endpoint**. No `mockito`, no `wiremock`, no
in-process fakes. Specifically:

- LLM tests hit real Ollama on kokonoe at the URL from
  `$NIGHTDRIVE_OPENCLAW_URL`.
- Audio-gen tests hit the real Stable Audio Open sidecar on cnc.
- Mastering / encoder tests spawn real `ffmpeg` subprocess.
- YouTube tests upload to a real (private) YouTube video resource and
  delete it afterward.
- Storage tests use real SQLite on a tempdir DB.

Tests that can't reach their endpoint mark themselves `#[ignore]` with a
reason, so a developer without the network access (or without cnc
P100s) can still `cargo test` and have the suite pass:

```rust
#[tokio::test]
#[cfg_attr(not(feature = "live-audio-sidecar"), ignore = "needs cnc:8080 reachable")]
async fn audio_sidecar_health() { ... }
```

The exception: `Task N4.1 retry` policy tests use a local mock-server
because the failure modes (transient 500s in a controlled order) can't
be reliably reproduced against a real endpoint. This is the only
documented exception. Cite this README in the test if you add another.

## Why no mocks (the user's burned-by lesson)

Per Matt's global feedback: "we got burned last quarter when mocked
tests passed but the prod migration failed." Mocks drift from reality;
real-endpoint tests catch the drift. The cost (test latency, network
flake, occasional skips) is worth it.

## Cross-references

- `docs/ROADMAP.md` — every numbered roadmap item lists its expected witness file.
- `.claude/agents/witness-test-author.md` — the agent that drafts these.
- `scripts/audit.ps1` section [4/7] — counts the tagged tests.
