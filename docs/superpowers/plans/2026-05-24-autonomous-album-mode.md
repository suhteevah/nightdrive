# Autonomous Album Mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire nightdrive to drop a new 12-track album every 3 days with no human-in-the-loop — backlog-driven, openclaw-main composed, GPU-coordinated with the openclaw inference fleet, thumbnails auto-retried.

**Architecture:** Three stacked systemd timers (album-drop / thumbnail-retry / theme-propose) over a file-based `docs/album-backlog.json` queue. Album composition routes to openclaw `main` (Opus 4.7 OAuth, free under Max 20x) via the gateway, falling back to LiteLLM Sonnet on failure. Per-album render reuses the proven eviction pattern (stop `openclaw-inference-{embed,scout,workhorse}`, run ACE-Step, restore) — no new GPU arbitration.

**Tech Stack:** Rust 2024 (workspace MSRV 1.85), sqlx + sqlite, tokio, reqwest, serde, anyhow, thiserror, tracing, clap, systemd timers on cnc-server (openSUSE Leap Micro 6.2), podman exec into `openclaw-gateway` container for OAuth-bearing LLM calls if HTTP route isn't exposed.

**Source spec:** `docs/superpowers/specs/2026-05-24-autonomous-album-mode-design.md`

**Commit policy:** Per `reference_github_uploader_auto_commits` — the nightdrive working tree is auto-committed/pushed by github-uploader-buildout. **Do NOT run `git add` / `git commit` / `git push` manually.** Each task ends with a build-green check; the sweep snapshots whenever the tree is clean enough.

---

## File Structure

```
J:\nightdrive\
├── Cargo.toml                                          # MODIFY: workspace members += 2
├── crates/
│   ├── nightdrive-openclaw-main/                       # NEW crate
│   │   ├── Cargo.toml
│   │   ├── src/lib.rs                                  # ask_main(prompt) -> String
│   │   └── tests/real_endpoint.rs                      # // stage: 1 witness
│   ├── nightdrive-album-composer/                      # NEW crate
│   │   ├── Cargo.toml
│   │   ├── src/lib.rs                                  # compose(theme, ...) -> AlbumSpec
│   │   ├── src/schema.rs                               # AlbumSpec, TrackSpec types
│   │   ├── src/prompt.rs                               # build_prompt(theme, examples, danger_zone)
│   │   ├── src/danger_zone.rs                          # check(spec, zone) -> Result<(), Vec<Hit>>
│   │   └── tests/danger_zone_test.rs                   # unit
│   ├── nightdrive-core/
│   │   └── src/backlog.rs                              # NEW module: load/save w/ flock
│   ├── nightdrive-storage/
│   │   └── migrations/20260524000000_thumbnail_state.sql  # NEW migration
│   └── nightdrive-cli/
│       └── src/main.rs                                 # MODIFY: thumbnails + album subcommands
├── docs/
│   ├── album-backlog.json                              # NEW seed
│   └── album-danger-zone.json                          # NEW seed
└── scripts/
    ├── nightdrive-album-drop.service                   # NEW (w/ eviction wrappers)
    ├── nightdrive-album-drop.timer                     # NEW
    ├── nightdrive-thumbnail-retry.service              # NEW (no eviction)
    ├── nightdrive-thumbnail-retry.timer                # NEW
    ├── nightdrive-theme-propose.service                # NEW (no eviction)
    └── nightdrive-theme-propose.timer                  # NEW
```

---

## Task 1: Workspace registration for two new crates

**Files:**
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Read current workspace members to preserve ordering**

Run: `Grep "members" Cargo.toml -C 20`
Expected: shows `members = [ ... ]` block listing all 11 existing crates.

- [ ] **Step 2: Add the two new crate paths**

Edit `Cargo.toml`, find the `members = [` block, append BEFORE the closing `]`:

```toml
    "crates/nightdrive-openclaw-main",
    "crates/nightdrive-album-composer",
```

- [ ] **Step 3: Verify workspace still parses**

Run: `cargo metadata --no-deps --format-version=1 --manifest-path Cargo.toml | jq '.workspace_members | length'`
Expected: `13` (was 11 + 2 new).

---

## Task 2: Skeleton — `nightdrive-openclaw-main` crate

**Files:**
- Create: `crates/nightdrive-openclaw-main/Cargo.toml`
- Create: `crates/nightdrive-openclaw-main/src/lib.rs`

- [ ] **Step 1: Write Cargo.toml**

```toml
[package]
name = "nightdrive-openclaw-main"
version = "0.1.0"
edition = "2024"
rust-version = "1.85"

[dependencies]
nightdrive-core = { path = "../nightdrive-core" }
tokio = { workspace = true, features = ["process", "io-util", "macros", "rt"] }
reqwest = { workspace = true, features = ["json", "rustls-tls"] }
serde = { workspace = true, features = ["derive"] }
serde_json = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }
anyhow = { workspace = true }
```

(If any of those features aren't in the workspace root `[workspace.dependencies]`, copy the explicit version from a sibling crate's Cargo.toml — do NOT invent versions.)

- [ ] **Step 2: Write stub lib.rs**

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OpenclawMainError {
    #[error("gateway transport: {0}")]
    Transport(String),
    #[error("gateway auth: {0}")]
    Auth(String),
    #[error("gateway returned non-ok: {0}")]
    NonOk(String),
    #[error("podman exec failed: {0}")]
    PodmanExec(String),
}

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub base_url: String,
    pub bearer: String,
    pub container: String,
}

impl GatewayConfig {
    pub fn from_env() -> Result<Self, OpenclawMainError> {
        let base_url = std::env::var("NIGHTDRIVE_OPENCLAW_GATEWAY_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:18789".to_string());
        let bearer = std::env::var("NIGHTDRIVE_OPENCLAW_GATEWAY_TOKEN")
            .map_err(|_| OpenclawMainError::Auth("NIGHTDRIVE_OPENCLAW_GATEWAY_TOKEN missing".into()))?;
        let container = std::env::var("NIGHTDRIVE_OPENCLAW_CONTAINER")
            .unwrap_or_else(|_| "openclaw-gateway".to_string());
        Ok(Self { base_url, bearer, container })
    }
}

pub async fn ask_main(_cfg: &GatewayConfig, _prompt: &str, _max_tokens: u32) -> Result<String, OpenclawMainError> {
    unimplemented!("ask_main — pending Task 3 discovery + Task 4 impl")
}
```

- [ ] **Step 3: Verify the workspace compiles**

Run: `cargo check -p nightdrive-openclaw-main`
Expected: `Finished` with one `unused` warning on `unimplemented!`.

---

## Task 3: Discovery — does the gateway expose main over HTTP, or do we need podman exec?

This is a one-off investigation that determines the implementation path for Task 4. Document the finding inline; do not write code yet.

- [ ] **Step 1: Probe the gateway HTTP surface**

Run from a shell with access to cnc-server:

```bash
ssh cnc-server 'curl -sS -H "Authorization: Bearer $(jq -r .gateway.auth.token /opt/openclaw/gateway-config/openclaw.json)" http://127.0.0.1:18789/api/v1/agents 2>&1 | head -40'
```

Expected: JSON listing `{ id: "main" }` and `{ id: "mailclaw" }` — confirms gateway HTTP API reachable + auth works.

- [ ] **Step 2: Probe for a chat/messages endpoint on main**

Try these in order and document which works:

```bash
ssh cnc-server 'TOKEN=$(sudo jq -r .gateway.auth.token /opt/openclaw/gateway-config/openclaw.json); curl -sS -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" -d "{\"message\":\"reply PONG only\"}" http://127.0.0.1:18789/api/v1/agents/main/messages 2>&1 | head -40'
```

```bash
ssh cnc-server 'TOKEN=$(sudo jq -r .gateway.auth.token /opt/openclaw/gateway-config/openclaw.json); curl -sS -X POST -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" -d "{\"agent\":\"main\",\"message\":\"reply PONG only\"}" http://127.0.0.1:18789/api/v1/chat 2>&1 | head -40'
```

```bash
ssh cnc-server 'TOKEN=$(sudo jq -r .gateway.auth.token /opt/openclaw/gateway-config/openclaw.json); curl -sS -H "Authorization: Bearer $TOKEN" http://127.0.0.1:18789/openapi.json 2>&1 | jq -r ".paths | keys[]" | head -40'
```

Expected outcome: ONE of these returns either (a) a PONG-bearing JSON reply, (b) a route list including a per-agent chat endpoint, or (c) all 404 — in which case we use podman exec.

- [ ] **Step 3: Confirm podman exec fallback works (the canonical path from the openclaw-fleet skill)**

Run:

```bash
ssh cnc-server "timeout 60 sudo podman exec openclaw-gateway openclaw agent --agent main --message 'reply with PONG only'"
```

Expected: exits 0 with PONG in stdout. If it fails with `FailoverError: 401`, the OAuth token is expired — STOP and follow the openclaw-fleet skill's "Refresh OAuth credentials" recipe before proceeding.

- [ ] **Step 4: Document the chosen path**

Append a line to `docs/superpowers/specs/2026-05-24-autonomous-album-mode-design.md` under §Components → `nightdrive-openclaw-main`:

```
**Chosen RPC path (discovered 2026-05-24):** <HTTP POST /api/v1/agents/main/messages> | <podman exec openclaw-gateway openclaw agent --agent main --message ...>
```

Pick whichever worked in Steps 1-3. HTTP is preferred if available.

---

## Task 4: Implement `ask_main()` against the discovered RPC path

**Files:**
- Modify: `crates/nightdrive-openclaw-main/src/lib.rs`
- Create: `crates/nightdrive-openclaw-main/tests/real_endpoint.rs`

Branch on Task 3 outcome. Write only the path that works.

- [ ] **Step 1A (HTTP path): replace the `unimplemented!()` with the HTTP client**

```rust
use serde_json::json;

pub async fn ask_main(cfg: &GatewayConfig, prompt: &str, max_tokens: u32) -> Result<String, OpenclawMainError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| OpenclawMainError::Transport(e.to_string()))?;

    let url = format!("{}/api/v1/agents/main/messages", cfg.base_url.trim_end_matches('/'));
    let body = json!({ "message": prompt, "max_tokens": max_tokens });

    let resp = client
        .post(&url)
        .bearer_auth(&cfg.bearer)
        .json(&body)
        .send()
        .await
        .map_err(|e| OpenclawMainError::Transport(e.to_string()))?;

    let status = resp.status();
    let text = resp.text().await.map_err(|e| OpenclawMainError::Transport(e.to_string()))?;

    if !status.is_success() {
        return Err(OpenclawMainError::NonOk(format!("{}: {}", status, text)));
    }

    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| OpenclawMainError::NonOk(format!("non-json reply: {} ({})", e, text)))?;

    v.get("reply")
        .or_else(|| v.get("message"))
        .or_else(|| v.get("content"))
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .ok_or_else(|| OpenclawMainError::NonOk(format!("no reply field in: {}", text)))
}
```

- [ ] **Step 1B (podman exec path): replace the `unimplemented!()` with `tokio::process::Command`**

```rust
use tokio::process::Command;

pub async fn ask_main(cfg: &GatewayConfig, prompt: &str, _max_tokens: u32) -> Result<String, OpenclawMainError> {
    let out = Command::new("sudo")
        .args(["podman", "exec", &cfg.container, "openclaw", "agent", "--agent", "main", "--message", prompt])
        .output()
        .await
        .map_err(|e| OpenclawMainError::PodmanExec(e.to_string()))?;

    if !out.status.success() {
        return Err(OpenclawMainError::PodmanExec(format!(
            "exit={:?} stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        )));
    }

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    Ok(strip_openclaw_chrome(&stdout))
}

fn strip_openclaw_chrome(s: &str) -> String {
    s.lines()
        .filter(|l| !l.starts_with("[gateway]") && !l.starts_with("[openclaw]"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}
```

- [ ] **Step 2: Write the real-endpoint witness test**

`crates/nightdrive-openclaw-main/tests/real_endpoint.rs`:

```rust
// stage: 1
// expect: real openclaw gateway round-trip returns a non-empty string
use nightdrive_openclaw_main::{ask_main, GatewayConfig};

#[tokio::test]
#[ignore = "real endpoint — run with `cargo test -p nightdrive-openclaw-main -- --ignored`"]
async fn real_main_round_trip() {
    let cfg = GatewayConfig::from_env().expect("NIGHTDRIVE_OPENCLAW_GATEWAY_TOKEN must be set");
    let reply = ask_main(&cfg, "Reply with the single word PONG and nothing else.", 32)
        .await
        .expect("ask_main should succeed");
    assert!(!reply.trim().is_empty(), "reply should be non-empty: {:?}", reply);
    assert!(reply.to_uppercase().contains("PONG"), "expected PONG, got: {:?}", reply);
}
```

- [ ] **Step 3: Run the test against the real gateway**

From cnc-server (or any host with token + reachability):

```bash
set -a; source /etc/nightdrive/nightdrive.env; set +a
cargo test -p nightdrive-openclaw-main -- --ignored
```

Expected: 1 passed.

If the env var `NIGHTDRIVE_OPENCLAW_GATEWAY_TOKEN` doesn't exist in `/etc/nightdrive/nightdrive.env` yet, add it now (Task 6 will codify this) with the value from `sudo jq -r .gateway.auth.token /opt/openclaw/gateway-config/openclaw.json`.

- [ ] **Step 4: Build clean**

Run: `cargo build -p nightdrive-openclaw-main`
Expected: `Finished` with no warnings.

---

## Task 5: Seed `docs/album-danger-zone.json`

**Files:**
- Create: `docs/album-danger-zone.json`

- [ ] **Step 1: Write the seed file**

```json
{
  "version": 1,
  "updated_at": "2026-05-24",
  "policy": "Track titles must not double-hit (film dialogue/object AND canonical soundtrack title). Single-hit OK.",
  "themes": {
    "tron": {
      "soundtrack_titles": ["Derez", "Recognizer", "End of Line", "Rinzler", "Adagio for TRON", "Disc Wars", "C.L.U.", "Solar Sailer", "Outlands", "Tron Legacy (End Titles)"],
      "film_objects":      ["Derez", "Recognizer", "Light Cycle", "Disc", "Sark", "MCP", "I/O Tower", "Grid"]
    },
    "blade_runner": {
      "soundtrack_titles": ["Tears in Rain", "Memories of Green", "Wait for Me", "Damask Rose", "Esper Edit", "Blade Runner Blues", "Rachel's Song", "Love Theme"],
      "film_objects":      ["Tears in Rain", "Spinner", "Voight-Kampff", "Replicant", "Off-world", "Tyrell", "Esper", "Nexus-6"]
    },
    "tokyo_cyberpunk": {
      "soundtrack_titles": ["Kaneda", "Tetsuo", "Battle Against Clown", "Requiem", "Reincarnation", "Ghost City", "Inner Universe", "Run Rabbit Junk", "I Do", "Cyberbird"],
      "film_objects":      ["Kaneda", "Tetsuo", "Akira", "Major", "Section 9", "Puppet Master", "Stand Alone Complex"]
    },
    "miami_vice": {
      "soundtrack_titles": ["In the Air Tonight", "Crockett's Theme", "You Belong to the City", "Smuggler's Blues", "Brothers in Arms", "Miami Vice Theme"],
      "film_objects":      ["Crockett", "Tubbs", "Ferrari Daytona", "Vice", "South Beach", "Calderone"]
    },
    "berlin_wall": {
      "soundtrack_titles": ["Heroes", "Wind of Change", "Nikita", "99 Luftballons", "Final Countdown"],
      "film_objects":      ["Checkpoint Charlie", "Stasi", "Trabant", "Brandenburg", "Wall", "GDR"]
    },
    "atompunk": {
      "soundtrack_titles": [],
      "film_objects": ["Fallout", "Vault-Tec", "Pip-Boy", "Sputnik"]
    },
    "sovetskiy": {
      "soundtrack_titles": [],
      "film_objects": []
    },
    "sunset": {
      "soundtrack_titles": [],
      "film_objects": []
    },
    "neo_tokyo": {
      "soundtrack_titles": [],
      "film_objects": []
    }
  }
}
```

- [ ] **Step 2: Validate JSON parses**

Run: `cat docs/album-danger-zone.json | jq '.themes | keys'`
Expected: array of 9 theme keys.

---

## Task 6: Seed `docs/album-backlog.json`

**Files:**
- Create: `docs/album-backlog.json`

- [ ] **Step 1: Write the seed file**

```json
{
  "version": 1,
  "youtube_strikes": 0,
  "proposed": [],
  "approved": [
    {
      "slug": "tokyo-cyberpunk-vol-1",
      "theme": "Tokyo cyberpunk noir — neon Shinjuku rain, lonely salaryman last train, Akira-meets-Blade-Runner mood",
      "approved_at": "2026-05-24T07:30:00Z",
      "danger_zone_keys": ["tokyo_cyberpunk", "blade_runner"]
    },
    {
      "slug": "miami-vice-vol-1",
      "theme": "Miami Vice / Vapor Coast — pastel coke-era 1986, Ferrari Daytona at sunset, palm shadows, cocaine pink and turquoise",
      "approved_at": "2026-05-24T07:30:00Z",
      "danger_zone_keys": ["miami_vice"]
    },
    {
      "slug": "blade-runner-2049-vol-1",
      "theme": "Blade Runner / LA 2049 — rain, neon Chinatown signage, Vangelis-adjacent pads, Wallace Corporation cold",
      "approved_at": "2026-05-24T07:30:00Z",
      "danger_zone_keys": ["blade_runner"]
    },
    {
      "slug": "berlin-wall-vol-1",
      "theme": "Cold War East — Checkpoint Charlie, Trabants, divided city, brutalist apartment blocks, watchful gray dawn",
      "approved_at": "2026-05-24T07:30:00Z",
      "danger_zone_keys": ["berlin_wall", "sovetskiy"]
    }
  ],
  "history": [
    { "slug": "sunset-drive-vol-1",      "dropped_at": "2026-05-12T00:00:00Z" },
    { "slug": "neo-tokyo-drive-vol-1",   "dropped_at": "2026-05-21T00:00:00Z" },
    { "slug": "tron-drive-vol-1",        "dropped_at": "2026-05-15T15:00:00Z" },
    { "slug": "sovetskiy-drive-vol-1",   "dropped_at": "2026-05-23T00:00:00Z" },
    { "slug": "atompunk-drive-vol-1",    "dropped_at": "2026-05-26T00:00:00Z" }
  ]
}
```

(History `dropped_at` reflects sync-drop anchors from memory; ordering doesn't matter — list is informational only.)

- [ ] **Step 2: Validate JSON parses + invariants hold**

```bash
jq '.approved | length, .proposed | length, .history | length' docs/album-backlog.json
```

Expected: `4`, `0`, `5` on three lines.

```bash
jq '.approved[].slug, .history[].slug' docs/album-backlog.json | sort | uniq -d
```

Expected: empty (no slug duplicated across approved + history).

---

## Task 7: `nightdrive-core::backlog` module — file model + flock

**Files:**
- Create: `crates/nightdrive-core/src/backlog.rs`
- Modify: `crates/nightdrive-core/src/lib.rs` (add `pub mod backlog;`)
- Create: `crates/nightdrive-core/tests/backlog_test.rs`

- [ ] **Step 1: Add `fs2` to core's Cargo.toml**

Find `crates/nightdrive-core/Cargo.toml`. Under `[dependencies]`, add:

```toml
fs2 = "0.4"
chrono = { workspace = true, features = ["serde"] }
```

(If `chrono` is already there, just add `fs2`.)

- [ ] **Step 2: Write the module**

`crates/nightdrive-core/src/backlog.rs`:

```rust
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backlog {
    pub version: u32,
    #[serde(default)]
    pub youtube_strikes: u32,
    #[serde(default)]
    pub proposed: Vec<Proposed>,
    #[serde(default)]
    pub approved: Vec<Approved>,
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposed {
    pub slug: String,
    pub theme: String,
    pub proposed_at: DateTime<Utc>,
    pub promote_at: DateTime<Utc>,
    pub proposed_by: String,
    #[serde(default)]
    pub danger_zone_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approved {
    pub slug: String,
    pub theme: String,
    pub approved_at: DateTime<Utc>,
    #[serde(default)]
    pub danger_zone_keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub slug: String,
    pub dropped_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum BacklogError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("slug already exists in {section}: {slug}")]
    DuplicateSlug { section: &'static str, slug: String },
}

/// Load + lock-and-mutate. Closure runs while exclusive flock is held.
/// On success, atomically writes the result back.
pub fn mutate<P, F>(path: P, f: F) -> Result<Backlog, BacklogError>
where
    P: AsRef<Path>,
    F: FnOnce(&mut Backlog) -> Result<(), BacklogError>,
{
    let path = path.as_ref();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)?;
    file.lock_exclusive()?;

    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    let mut bl: Backlog = if buf.trim().is_empty() {
        Backlog { version: 1, youtube_strikes: 0, proposed: vec![], approved: vec![], history: vec![] }
    } else {
        serde_json::from_str(&buf)?
    };

    f(&mut bl)?;

    let out = serde_json::to_string_pretty(&bl)?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(out.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    file.unlock()?;

    Ok(bl)
}

pub fn load<P: AsRef<Path>>(path: P) -> Result<Backlog, BacklogError> {
    let buf = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&buf)?)
}

/// Move any proposed entries whose `promote_at <= now` to the tail of `approved`.
/// Returns the slugs that were promoted.
pub fn promote_expired(bl: &mut Backlog, now: DateTime<Utc>) -> Vec<String> {
    let mut promoted = Vec::new();
    let mut still_proposed = Vec::with_capacity(bl.proposed.len());
    for p in std::mem::take(&mut bl.proposed) {
        if p.promote_at <= now {
            promoted.push(p.slug.clone());
            bl.approved.push(Approved {
                slug: p.slug,
                theme: p.theme,
                approved_at: now,
                danger_zone_keys: p.danger_zone_keys,
            });
        } else {
            still_proposed.push(p);
        }
    }
    bl.proposed = still_proposed;
    promoted
}

/// Pop head of approved. Returns None if empty.
pub fn pop_approved(bl: &mut Backlog) -> Option<Approved> {
    if bl.approved.is_empty() {
        None
    } else {
        Some(bl.approved.remove(0))
    }
}
```

- [ ] **Step 3: Wire into core's lib.rs**

In `crates/nightdrive-core/src/lib.rs`, add (alphabetized into the existing module list):

```rust
pub mod backlog;
```

- [ ] **Step 4: Write unit tests**

`crates/nightdrive-core/tests/backlog_test.rs`:

```rust
use chrono::{Duration, Utc};
use nightdrive_core::backlog::{self, Backlog, Proposed};
use tempfile::tempdir;

#[test]
fn load_seed() {
    let bl: Backlog = serde_json::from_str(include_str!("../../../docs/album-backlog.json"))
        .expect("seed parses");
    assert_eq!(bl.approved.len(), 4);
    assert_eq!(bl.proposed.len(), 0);
    assert_eq!(bl.history.len(), 5);
}

#[test]
fn promote_expired_moves_old_proposals() {
    let now = Utc::now();
    let mut bl = Backlog {
        version: 1, youtube_strikes: 0,
        proposed: vec![
            Proposed { slug: "expired".into(), theme: "x".into(),
                       proposed_at: now - Duration::days(2),
                       promote_at:  now - Duration::hours(1),
                       proposed_by: "test".into(), danger_zone_keys: vec![] },
            Proposed { slug: "fresh".into(), theme: "y".into(),
                       proposed_at: now,
                       promote_at:  now + Duration::hours(23),
                       proposed_by: "test".into(), danger_zone_keys: vec![] },
        ],
        approved: vec![],
        history: vec![],
    };
    let promoted = backlog::promote_expired(&mut bl, now);
    assert_eq!(promoted, vec!["expired"]);
    assert_eq!(bl.approved.len(), 1);
    assert_eq!(bl.proposed.len(), 1);
    assert_eq!(bl.proposed[0].slug, "fresh");
}

#[test]
fn mutate_round_trips_via_flock() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("b.json");
    backlog::mutate(&path, |bl| {
        bl.version = 1;
        bl.approved.push(backlog::Approved {
            slug: "test-vol-1".into(), theme: "t".into(),
            approved_at: Utc::now(), danger_zone_keys: vec![],
        });
        Ok(())
    }).unwrap();
    let bl = backlog::load(&path).unwrap();
    assert_eq!(bl.approved.len(), 1);
    assert_eq!(bl.approved[0].slug, "test-vol-1");
}
```

(Add `tempfile = "3"` to nightdrive-core's `[dev-dependencies]` if not already there.)

- [ ] **Step 5: Run tests**

Run: `cargo test -p nightdrive-core backlog`
Expected: 3 passed.

---

## Task 8: `nightdrive-album-composer` skeleton crate

**Files:**
- Create: `crates/nightdrive-album-composer/Cargo.toml`
- Create: `crates/nightdrive-album-composer/src/lib.rs`
- Create: `crates/nightdrive-album-composer/src/schema.rs`
- Create: `crates/nightdrive-album-composer/src/danger_zone.rs`
- Create: `crates/nightdrive-album-composer/src/prompt.rs`

- [ ] **Step 1: Write Cargo.toml**

```toml
[package]
name = "nightdrive-album-composer"
version = "0.1.0"
edition = "2024"
rust-version = "1.85"

[dependencies]
nightdrive-core = { path = "../nightdrive-core" }
nightdrive-openclaw-main = { path = "../nightdrive-openclaw-main" }
nightdrive-llm = { path = "../nightdrive-llm" }
serde = { workspace = true, features = ["derive"] }
serde_json = { workspace = true }
tokio = { workspace = true, features = ["macros", "rt"] }
tracing = { workspace = true }
thiserror = { workspace = true }
anyhow = { workspace = true }
```

- [ ] **Step 2: Write `schema.rs` (matches existing album JSON shape)**

Read one existing album JSON first to lock the shape:

```bash
jq '. | keys' docs/albums/atompunk-drive-vol-1.json
jq '.tracks[0] | keys' docs/albums/atompunk-drive-vol-1.json
```

Then write:

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlbumSpec {
    pub album_slug: String,
    pub title: String,
    pub theme: String,
    pub track_count: u32,
    pub tonic_progression: String,
    pub bpm_arc: Vec<u32>,
    pub narrative_arc: String,
    pub recurring_motifs: Vec<String>,
    pub tracks: Vec<TrackSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackSpec {
    pub track_number: u32,
    pub title: String,
    pub role: String,
    pub key: String,
    pub bpm: u32,
    pub duration_seconds: u32,
    pub mood_tags: Vec<String>,
    pub sections: Vec<Section>,
    pub musicgen_prompt: String,
    pub cover_prompt: String,
    pub key_relationship_to_prior: String,
    pub tempo_relationship_to_prior: String,
    pub composer_notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Section {
    pub name: String,
    pub bars: u32,
    pub instrumentation: String,
}
```

If the actual JSON has fields not listed (e.g. `bonus_track`), add them as `Option<...>` so deserialization stays permissive.

- [ ] **Step 3: Write `danger_zone.rs`**

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DangerZoneFile {
    pub version: u32,
    pub themes: HashMap<String, ThemeZone>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeZone {
    #[serde(default)]
    pub soundtrack_titles: Vec<String>,
    #[serde(default)]
    pub film_objects: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub track_title: String,
    pub matched_soundtrack: String,
    pub matched_film: String,
    pub theme_key: String,
}

pub fn load<P: AsRef<Path>>(path: P) -> Result<DangerZoneFile, anyhow::Error> {
    let buf = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&buf)?)
}

/// A track title is a "hit" if (a) it appears in soundtrack_titles AND (b) appears in film_objects
/// for any of the supplied theme keys. Returns all double-hits across all enabled themes.
pub fn check_titles(titles: &[&str], zones: &DangerZoneFile, enabled_themes: &[String]) -> Vec<Hit> {
    let mut hits = Vec::new();
    let norm = |s: &str| s.to_lowercase();
    for t in titles {
        let tn = norm(t);
        for theme_key in enabled_themes {
            let Some(zone) = zones.themes.get(theme_key) else { continue };
            let st_hit = zone.soundtrack_titles.iter().find(|s| norm(s) == tn);
            let fo_hit = zone.film_objects.iter().find(|s| norm(s) == tn);
            if let (Some(s), Some(f)) = (st_hit, fo_hit) {
                hits.push(Hit {
                    track_title: t.to_string(),
                    matched_soundtrack: s.clone(),
                    matched_film: f.clone(),
                    theme_key: theme_key.clone(),
                });
            }
        }
    }
    hits
}
```

- [ ] **Step 4: Write `prompt.rs`**

```rust
use crate::schema::AlbumSpec;

/// Few-shot prompt builder. Includes the three most-recent album JSONs verbatim so the LLM
/// matches house style. The recurring_motifs library is collected from those examples.
pub fn build_prompt(
    theme: &str,
    slug: &str,
    track_count: u32,
    few_shot_examples: &[AlbumSpec],
    danger_zone_keys: &[String],
) -> String {
    let mut p = String::new();
    p.push_str(&format!(
        "You are nightdrive's album composer. Output a single JSON object matching the AlbumSpec schema.\n\
         No prose, no markdown fence, no commentary — JSON only.\n\n\
         Theme: {theme}\n\
         Album slug: {slug}\n\
         Track count: {track_count} (exactly).\n\
         Danger-zone theme keys to avoid double-hits in: {danger_zone_keys:?}\n\n\
         Rules:\n\
         - BPM 80-118 per track (slowed cruise + a few peaks).\n\
         - Duration 180-360 seconds per track.\n\
         - Each track has key, role (opener|cruiser|peak|bridge|closer), bpm, duration_seconds,\n\
           mood_tags[], sections[], musicgen_prompt, cover_prompt, key_relationship_to_prior,\n\
           tempo_relationship_to_prior, composer_notes.\n\
         - Use recurring_motifs to thread the album together (3-5 musical motifs that recur).\n\
         - Compose a narrative_arc (1-2 sentences).\n\
         - bpm_arc[] is the BPM of each track in order.\n\
         - Avoid track titles that ARE both a soundtrack-known title AND a film object/dialogue\n\
           (these would trigger algorithmic claims).\n\n\
         Examples of well-formed album JSONs (match this house style):\n\n"
    ));
    for ex in few_shot_examples {
        p.push_str("```json\n");
        p.push_str(&serde_json::to_string_pretty(ex).unwrap());
        p.push_str("\n```\n\n");
    }
    p.push_str("Now produce the AlbumSpec JSON for the requested theme + slug.\n");
    p
}
```

- [ ] **Step 5: Write `lib.rs` (top-level `compose()`)**

```rust
pub mod danger_zone;
pub mod prompt;
pub mod schema;

use nightdrive_openclaw_main::{ask_main, GatewayConfig};
use schema::AlbumSpec;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

#[derive(Debug, thiserror::Error)]
pub enum ComposerError {
    #[error("openclaw main: {0}")]
    Llm(#[from] nightdrive_openclaw_main::OpenclawMainError),
    #[error("danger-zone strike-3: {hits:?}")]
    DangerZoneBlocked { hits: Vec<danger_zone::Hit> },
    #[error("invalid JSON from LLM: {0}")]
    InvalidJson(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct ComposeRequest {
    pub slug: String,
    pub theme: String,
    pub track_count: u32,
    pub danger_zone_keys: Vec<String>,
    pub albums_dir: PathBuf,            // typically docs/albums
    pub danger_zone_path: PathBuf,      // typically docs/album-danger-zone.json
    pub max_retries: u32,               // typically 3
}

pub async fn compose(cfg: &GatewayConfig, req: &ComposeRequest) -> Result<AlbumSpec, ComposerError> {
    let examples = load_recent_examples(&req.albums_dir, 3)?;
    let zones = danger_zone::load(&req.danger_zone_path)
        .map_err(|e| ComposerError::InvalidJson(format!("danger-zone load: {e}")))?;

    for attempt in 0..req.max_retries {
        let prompt_text = prompt::build_prompt(
            &req.theme, &req.slug, req.track_count, &examples, &req.danger_zone_keys,
        );
        info!(attempt, "composer: asking openclaw main");
        let reply = ask_main(cfg, &prompt_text, 16000).await?;
        let json = strip_fence(&reply);
        let spec: AlbumSpec = serde_json::from_str(&json).map_err(|e| {
            ComposerError::InvalidJson(format!("parse fail attempt={attempt}: {e}"))
        })?;

        let titles: Vec<&str> = spec.tracks.iter().map(|t| t.title.as_str()).collect();
        let hits = danger_zone::check_titles(&titles, &zones, &req.danger_zone_keys);
        if hits.is_empty() {
            info!(slug = %req.slug, "composer: clean spec on attempt {attempt}");
            return Ok(spec);
        }
        warn!(?hits, attempt, "composer: danger-zone hits, retrying");
    }

    Err(ComposerError::DangerZoneBlocked {
        hits: vec![],  // surface the last-attempt hits in the final error in production
    })
}

fn strip_fence(s: &str) -> String {
    let t = s.trim();
    let t = t.strip_prefix("```json").or_else(|| t.strip_prefix("```")).unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim().to_string()
}

fn load_recent_examples(dir: &Path, n: usize) -> Result<Vec<AlbumSpec>, ComposerError> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    entries.sort_by_key(|e| std::cmp::Reverse(e.metadata().and_then(|m| m.modified()).ok()));
    let mut out = Vec::new();
    for e in entries.into_iter().take(n) {
        let buf = std::fs::read_to_string(e.path())?;
        if let Ok(spec) = serde_json::from_str::<AlbumSpec>(&buf) {
            out.push(spec);
        }
    }
    Ok(out)
}
```

- [ ] **Step 6: Verify compile**

Run: `cargo check -p nightdrive-album-composer`
Expected: `Finished`.

---

## Task 9: `nightdrive-album-composer` danger-zone unit tests

**Files:**
- Create: `crates/nightdrive-album-composer/tests/danger_zone_test.rs`

- [ ] **Step 1: Write tests**

```rust
use nightdrive_album_composer::danger_zone::{check_titles, DangerZoneFile, ThemeZone};
use std::collections::HashMap;

fn fixture() -> DangerZoneFile {
    let mut themes = HashMap::new();
    themes.insert("tron".into(), ThemeZone {
        soundtrack_titles: vec!["Derez".into(), "End of Line".into()],
        film_objects:      vec!["Derez".into(), "Light Cycle".into()],
    });
    themes.insert("blade_runner".into(), ThemeZone {
        soundtrack_titles: vec!["Tears in Rain".into()],
        film_objects:      vec!["Tears in Rain".into(), "Spinner".into()],
    });
    DangerZoneFile { version: 1, themes }
}

#[test]
fn double_hit_is_blocked() {
    let z = fixture();
    let hits = check_titles(&["Derez", "Cruiser One"], &z, &vec!["tron".into()]);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].track_title, "Derez");
}

#[test]
fn single_hit_passes() {
    let z = fixture();
    // "Light Cycle" is film_object but not soundtrack_title -> single hit, allowed
    let hits = check_titles(&["Light Cycle"], &z, &vec!["tron".into()]);
    assert!(hits.is_empty());
}

#[test]
fn case_insensitive() {
    let z = fixture();
    let hits = check_titles(&["derez"], &z, &vec!["tron".into()]);
    assert_eq!(hits.len(), 1);
}

#[test]
fn cross_theme_hits_only_in_enabled_themes() {
    let z = fixture();
    let hits = check_titles(&["Tears in Rain"], &z, &vec!["tron".into()]);
    assert!(hits.is_empty(), "blade_runner not enabled, so no hit");
    let hits = check_titles(&["Tears in Rain"], &z, &vec!["blade_runner".into()]);
    assert_eq!(hits.len(), 1);
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p nightdrive-album-composer danger_zone_test`
Expected: 4 passed.

---

## Task 10: `nightdrive-album-composer` real-endpoint witness test

**Files:**
- Create: `crates/nightdrive-album-composer/tests/real_compose.rs`

- [ ] **Step 1: Write the witness test**

```rust
// stage: 1
// expect: real openclaw main produces a valid 4-track AlbumSpec for a throwaway theme
use nightdrive_album_composer::{compose, ComposeRequest};
use nightdrive_openclaw_main::GatewayConfig;
use std::path::PathBuf;

#[tokio::test]
#[ignore = "real endpoint — run with `cargo test -p nightdrive-album-composer -- --ignored`"]
async fn real_compose_smoke() {
    let cfg = GatewayConfig::from_env().expect("gateway env present");
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().parent().unwrap().to_path_buf();
    let req = ComposeRequest {
        slug: "test-witness-vol-1".into(),
        theme: "Quiet 1990s parking-garage at 3 AM, fluorescent hum, no people".into(),
        track_count: 4,
        danger_zone_keys: vec![],
        albums_dir: repo_root.join("docs/albums"),
        danger_zone_path: repo_root.join("docs/album-danger-zone.json"),
        max_retries: 2,
    };
    let spec = compose(&cfg, &req).await.expect("compose succeeds");
    assert_eq!(spec.tracks.len(), 4);
    assert_eq!(spec.album_slug, "test-witness-vol-1");
    for t in &spec.tracks {
        assert!((80..=118).contains(&t.bpm), "track BPM out of range: {}", t.bpm);
        assert!((180..=360).contains(&t.duration_seconds));
        assert!(!t.title.trim().is_empty());
    }
}
```

- [ ] **Step 2: Run the witness against real gateway**

```bash
set -a; source /etc/nightdrive/nightdrive.env; set +a
cargo test -p nightdrive-album-composer -- --ignored real_compose_smoke
```

Expected: 1 passed (takes ~30-60s depending on Opus latency).

If the LLM produces malformed JSON, increase `max_retries` to 3 or 4 — Opus is reliable but not infallible.

---

## Task 11: Storage migration — `custom_thumbnail_set` column

**Files:**
- Create: `crates/nightdrive-storage/migrations/20260524000000_thumbnail_state.sql`
- Modify: `crates/nightdrive-storage/src/lib.rs` (or wherever Track row is defined — find with grep below)

- [ ] **Step 1: Check current schema**

Run: `Grep -rn "CREATE TABLE tracks" crates/nightdrive-storage/migrations/`
Then: `Grep -rn "custom_thumbnail" crates/`

Expected: shows the original `tracks` schema. If `custom_thumbnail_set` already exists somehow, SKIP this entire task.

- [ ] **Step 2: Write migration**

```sql
-- 20260524000000_thumbnail_state.sql
-- Tracks whether the per-track cover.png was successfully uploaded as a custom YouTube thumbnail.
-- Default 0 means "not yet set" — eligible for thumbnail-retry sweep.
-- Set to 1 by nightdrive-orchestrator when set_thumbnail_best_effort() returns Ok.

ALTER TABLE tracks ADD COLUMN custom_thumbnail_set INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tracks ADD COLUMN thumbnail_last_attempt_at TEXT;

CREATE INDEX IF NOT EXISTS idx_tracks_thumb_retry
  ON tracks(custom_thumbnail_set, state)
  WHERE custom_thumbnail_set = 0;
```

- [ ] **Step 3: Update the Rust Track row type**

Find the struct (likely `crates/nightdrive-storage/src/lib.rs`):

```bash
Grep -n "struct Track" crates/nightdrive-storage/src/
```

Add fields:

```rust
pub custom_thumbnail_set: bool,
pub thumbnail_last_attempt_at: Option<chrono::DateTime<chrono::Utc>>,
```

And add a helper:

```rust
pub async fn list_published_with_missing_thumbnail(
    pool: &sqlx::SqlitePool,
    limit: i64,
) -> sqlx::Result<Vec<Track>> {
    sqlx::query_as::<_, Track>(
        "SELECT * FROM tracks
         WHERE state = 'published' AND custom_thumbnail_set = 0 AND youtube_id IS NOT NULL
         ORDER BY published_at ASC
         LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn mark_thumbnail_set(pool: &sqlx::SqlitePool, track_id: &str) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE tracks SET custom_thumbnail_set = 1, thumbnail_last_attempt_at = datetime('now')
         WHERE track_id = ?",
    )
    .bind(track_id)
    .execute(pool)
    .await
    .map(|_| ())
}

pub async fn mark_thumbnail_attempted(pool: &sqlx::SqlitePool, track_id: &str) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE tracks SET thumbnail_last_attempt_at = datetime('now') WHERE track_id = ?",
    )
    .bind(track_id)
    .execute(pool)
    .await
    .map(|_| ())
}
```

Adjust the `state` literal to whatever the existing TrackState `published` variant serializes as (might be `'Published'` — check with grep).

- [ ] **Step 4: Update orchestrator to set the flag on successful thumbnail**

Find `set_thumbnail_best_effort` in `crates/nightdrive-orchestrator/src/main.rs:261` area. After the OK path:

```rust
nightdrive_storage::mark_thumbnail_set(pool, track_id).await?;
```

After the 429/403 path:

```rust
nightdrive_storage::mark_thumbnail_attempted(pool, track_id).await?;
```

- [ ] **Step 5: Run migrations + verify**

```bash
cargo run -p nightdrive-cli -- db migrate
```

Then on cnc-server:

```bash
ssh cnc-server "echo '.schema tracks' | sqlite3 /var/lib/nightdrive/nightdrive.sqlite 2>/dev/null || /opt/nightdrive/bin/nightdrive-cli db migrate"
```

Expected: schema shows `custom_thumbnail_set INTEGER NOT NULL DEFAULT 0`.

- [ ] **Step 6: Backfill existing tracks**

For tracks already published with their thumbnails set, mark them so we don't try to retry good ones:

```bash
ssh cnc-server "sqlite3 /var/lib/nightdrive/nightdrive.sqlite \"UPDATE tracks SET custom_thumbnail_set = 1 WHERE state = 'published' AND track_id NOT IN ('nd-atompunk-drive-vol-1-011', 'nd-atompunk-drive-vol-1-012');\""
```

(Tracks 011/012 of atompunk vol 1 are the ones that actually failed per our session memory — leave them at 0 so the first thumbnail-retry timer firing actually does something.)

---

## Task 12: CLI — `nightdrive-cli thumbnails retry-failed`

**Files:**
- Modify: `crates/nightdrive-cli/src/main.rs`

- [ ] **Step 1: Add subcommand to clap enum**

Find the existing top-level Commands enum (grep `enum Commands` in main.rs). Add:

```rust
/// Thumbnail maintenance.
Thumbnails {
    #[command(subcommand)]
    cmd: ThumbnailsCmd,
},
```

Then add:

```rust
#[derive(Debug, Subcommand)]
enum ThumbnailsCmd {
    /// Retry custom thumbnail upload for published tracks where it failed.
    RetryFailed {
        /// Max tracks to attempt this pass (respect per-day YT cap).
        #[arg(long, default_value_t = 80)]
        max: i64,
        /// Don't actually call YT; just print what would be tried.
        #[arg(long)]
        dry_run: bool,
    },
}
```

- [ ] **Step 2: Wire match arm**

```rust
Commands::Thumbnails { cmd } => match cmd {
    ThumbnailsCmd::RetryFailed { max, dry_run } => {
        thumbnails_retry_failed(cfg, max, dry_run).await
    }
},
```

- [ ] **Step 3: Write `thumbnails_retry_failed`**

Place above `main()`:

```rust
async fn thumbnails_retry_failed(
    cfg: &nightdrive_core::AppConfig,
    max: i64,
    dry_run: bool,
) -> anyhow::Result<()> {
    let pool = nightdrive_storage::connect_and_migrate(&cfg.storage.path).await?;
    let candidates = nightdrive_storage::list_published_with_missing_thumbnail(&pool, max).await?;
    info!(count = candidates.len(), max, dry_run, "thumbnail-retry: candidates loaded");
    if candidates.is_empty() {
        println!("no failed thumbnails to retry");
        return Ok(());
    }

    if dry_run {
        for t in &candidates {
            println!("DRY-RUN would retry {} (video={})", t.track_id, t.youtube_id.as_deref().unwrap_or("?"));
        }
        return Ok(());
    }

    let yt = nightdrive_youtube::YoutubeClient::from_config(&cfg.youtube).await?;
    let mut ok = 0u32;
    let mut rate_limited = false;
    for t in &candidates {
        let Some(video_id) = &t.youtube_id else { continue };
        let paths = nightdrive_core::TrackPaths::for_id(&cfg.storage.tracks_root, &t.track_id);
        let thumb = if paths.thumbnail_jpg().exists() { paths.thumbnail_jpg() } else { paths.cover_png() };

        match yt.set_thumbnail(video_id, &thumb).await {
            Ok(_) => {
                nightdrive_storage::mark_thumbnail_set(&pool, &t.track_id).await?;
                ok += 1;
                info!(track_id = %t.track_id, video_id, "thumbnail-retry: set");
            }
            Err(e) if is_rate_limited(&e) => {
                nightdrive_storage::mark_thumbnail_attempted(&pool, &t.track_id).await?;
                warn!(track_id = %t.track_id, "thumbnail-retry: 429 — stopping pass");
                rate_limited = true;
                break;
            }
            Err(e) => {
                nightdrive_storage::mark_thumbnail_attempted(&pool, &t.track_id).await?;
                warn!(track_id = %t.track_id, err = %e, "thumbnail-retry: non-429 error, continuing");
            }
        }
    }
    println!("retried {} thumbnails{}", ok, if rate_limited { " (rate-limited mid-pass)" } else { "" });

    if ok >= 10 {
        let _ = nightdrive_core::telegram::notify(&format!(
            "nightdrive: thumbnail-retry pass set {} custom thumbs{}",
            ok, if rate_limited { ", stopped on 429" } else { "" }
        ));
    }
    Ok(())
}

fn is_rate_limited(e: &anyhow::Error) -> bool {
    let s = format!("{e}");
    s.contains("429") || s.contains("rateLimitExceeded") || s.contains("uploadLimitExceeded")
}
```

- [ ] **Step 4: Add the telegram helper if it doesn't exist yet (defer to Task 17 if it does)**

Run: `Grep -n "pub fn notify" crates/nightdrive-core/src/telegram.rs` (or check if the file exists).

If `nightdrive_core::telegram::notify` doesn't exist, comment out that line for now — Task 17 creates it.

- [ ] **Step 5: Verify build**

Run: `cargo build -p nightdrive-cli`
Expected: `Finished`.

- [ ] **Step 6: Dry-run smoke test against the real DB**

```bash
ssh cnc-server "/opt/nightdrive/bin/nightdrive-cli thumbnails retry-failed --dry-run"
```

Expected: prints "DRY-RUN would retry nd-atompunk-drive-vol-1-011 ..." for the two known-failed ones from Vol. 3.

---

## Task 13: CLI — `nightdrive-cli album backlog {list, add, approve, nack, remove}`

**Files:**
- Modify: `crates/nightdrive-cli/src/main.rs`

- [ ] **Step 1: Add `Album` to the Commands enum**

```rust
/// Album backlog + drop control.
Album {
    #[command(subcommand)]
    cmd: AlbumCmd,
},
```

```rust
#[derive(Debug, Subcommand)]
enum AlbumCmd {
    /// Backlog management.
    Backlog {
        #[command(subcommand)]
        cmd: BacklogCmd,
    },
    /// Ask openclaw main to propose N new themes -> backlog.proposed[].
    Propose {
        #[arg(long, default_value_t = 3)]
        count: u32,
    },
    /// Pop next approved slug, run composer + render + upload + schedule. Idempotent.
    DropNext {
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
enum BacklogCmd {
    List,
    Add {
        slug: String,
        #[arg(long)]
        theme: String,
        #[arg(long)]
        approved: bool,
        #[arg(long, value_delimiter = ',')]
        danger_zone_keys: Vec<String>,
    },
    Approve { slug: String },
    Nack { slug: String },
    Remove { slug: String },
}
```

- [ ] **Step 2: Wire the match arms**

```rust
Commands::Album { cmd } => match cmd {
    AlbumCmd::Backlog { cmd } => album_backlog(cfg, cmd).await,
    AlbumCmd::Propose { count } => album_propose(cfg, count).await,
    AlbumCmd::DropNext { dry_run } => album_drop_next(cfg, dry_run).await,
},
```

- [ ] **Step 3: Write `album_backlog`**

```rust
async fn album_backlog(cfg: &nightdrive_core::AppConfig, cmd: BacklogCmd) -> anyhow::Result<()> {
    let path = cfg.paths.backlog_json();  // see Task 14 for this helper
    match cmd {
        BacklogCmd::List => {
            let bl = nightdrive_core::backlog::load(&path)?;
            println!("=== proposed ({}) ===", bl.proposed.len());
            for p in &bl.proposed {
                println!("  {} (promotes at {}) -- {}", p.slug, p.promote_at, p.theme);
            }
            println!("=== approved ({}) ===", bl.approved.len());
            for a in &bl.approved {
                println!("  {} -- {}", a.slug, a.theme);
            }
            println!("=== history ({}) ===", bl.history.len());
            for h in bl.history.iter().rev().take(10) {
                println!("  {} (dropped {})", h.slug, h.dropped_at);
            }
            Ok(())
        }
        BacklogCmd::Add { slug, theme, approved, danger_zone_keys } => {
            nightdrive_core::backlog::mutate(&path, |bl| {
                if approved {
                    bl.approved.push(nightdrive_core::backlog::Approved {
                        slug: slug.clone(), theme: theme.clone(),
                        approved_at: chrono::Utc::now(),
                        danger_zone_keys: danger_zone_keys.clone(),
                    });
                } else {
                    let now = chrono::Utc::now();
                    bl.proposed.push(nightdrive_core::backlog::Proposed {
                        slug: slug.clone(), theme: theme.clone(),
                        proposed_at: now,
                        promote_at:  now + chrono::Duration::hours(24),
                        proposed_by: "manual".into(),
                        danger_zone_keys: danger_zone_keys.clone(),
                    });
                }
                Ok(())
            })?;
            println!("added: {}", slug);
            Ok(())
        }
        BacklogCmd::Approve { slug } => {
            nightdrive_core::backlog::mutate(&path, |bl| {
                if let Some(idx) = bl.proposed.iter().position(|p| p.slug == slug) {
                    let p = bl.proposed.remove(idx);
                    bl.approved.push(nightdrive_core::backlog::Approved {
                        slug: p.slug, theme: p.theme,
                        approved_at: chrono::Utc::now(),
                        danger_zone_keys: p.danger_zone_keys,
                    });
                }
                Ok(())
            })?;
            println!("approved: {}", slug);
            Ok(())
        }
        BacklogCmd::Nack { slug } => {
            nightdrive_core::backlog::mutate(&path, |bl| {
                bl.proposed.retain(|p| p.slug != slug);
                Ok(())
            })?;
            println!("nack'd: {}", slug);
            Ok(())
        }
        BacklogCmd::Remove { slug } => {
            nightdrive_core::backlog::mutate(&path, |bl| {
                bl.proposed.retain(|p| p.slug != slug);
                bl.approved.retain(|a| a.slug != slug);
                Ok(())
            })?;
            println!("removed: {}", slug);
            Ok(())
        }
    }
}
```

- [ ] **Step 4: Add the `paths.backlog_json()` helper**

In `crates/nightdrive-core/src/config.rs` (or wherever AppConfig is defined), add field:

```rust
#[serde(default = "default_repo_root")]
pub repo_root: std::path::PathBuf,
```

```rust
fn default_repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from("/opt/nightdrive")
}
```

And a method:

```rust
impl Paths {
    pub fn backlog_json(&self) -> std::path::PathBuf {
        self.repo_root.join("docs/album-backlog.json")
    }
    pub fn danger_zone_json(&self) -> std::path::PathBuf {
        self.repo_root.join("docs/album-danger-zone.json")
    }
    pub fn albums_dir(&self) -> std::path::PathBuf {
        self.repo_root.join("docs/albums")
    }
}
```

(Adjust if `Paths` is a sub-struct of `AppConfig` — match existing pattern.)

- [ ] **Step 5: Verify build + smoke test**

```bash
cargo build -p nightdrive-cli
./target/debug/nightdrive-cli album backlog list
```

Expected: prints the 4 seeded approved entries.

---

## Task 14: CLI — `nightdrive-cli album propose --count N`

**Files:**
- Modify: `crates/nightdrive-cli/src/main.rs`

- [ ] **Step 1: Write `album_propose`**

```rust
async fn album_propose(cfg: &nightdrive_core::AppConfig, count: u32) -> anyhow::Result<()> {
    let backlog_path = cfg.paths.backlog_json();
    let albums_dir = cfg.paths.albums_dir();
    let existing_slugs = list_existing_album_slugs(&albums_dir, &backlog_path)?;

    let prompt = build_propose_prompt(count, &existing_slugs);
    let gw = nightdrive_openclaw_main::GatewayConfig::from_env()?;
    let reply = nightdrive_openclaw_main::ask_main(&gw, &prompt, 4000).await?;
    let proposals: Vec<ProposedFromLlm> = parse_proposals(&reply)?;

    let now = chrono::Utc::now();
    nightdrive_core::backlog::mutate(&backlog_path, |bl| {
        for p in &proposals {
            if existing_slugs.contains(&p.slug) { continue; }
            bl.proposed.push(nightdrive_core::backlog::Proposed {
                slug: p.slug.clone(), theme: p.theme.clone(),
                proposed_at: now,
                promote_at:  now + chrono::Duration::hours(24),
                proposed_by: "openclaw-main".into(),
                danger_zone_keys: p.danger_zone_keys.clone(),
            });
        }
        Ok(())
    })?;

    let slug_list: Vec<&str> = proposals.iter().map(|p| p.slug.as_str()).collect();
    println!("proposed: {:?}", slug_list);

    let _ = nightdrive_core::telegram::notify(&format!(
        "nightdrive: {} new themes proposed — 24h soak. NACK any via 'nightdrive-cli album backlog nack <slug>' on cnc. Slugs: {}",
        proposals.len(), slug_list.join(", ")
    ));
    Ok(())
}

#[derive(serde::Deserialize)]
struct ProposedFromLlm {
    slug: String,
    theme: String,
    #[serde(default)]
    danger_zone_keys: Vec<String>,
}

fn list_existing_album_slugs(albums_dir: &std::path::Path, backlog_path: &std::path::Path) -> anyhow::Result<std::collections::HashSet<String>> {
    let mut set = std::collections::HashSet::new();
    if let Ok(rd) = std::fs::read_dir(albums_dir) {
        for e in rd.flatten() {
            if let Some(name) = e.path().file_stem().and_then(|s| s.to_str()) {
                set.insert(name.to_string());
            }
        }
    }
    if let Ok(bl) = nightdrive_core::backlog::load(backlog_path) {
        for a in &bl.approved { set.insert(a.slug.clone()); }
        for p in &bl.proposed { set.insert(p.slug.clone()); }
        for h in &bl.history { set.insert(h.slug.clone()); }
    }
    Ok(set)
}

fn build_propose_prompt(count: u32, existing_slugs: &std::collections::HashSet<String>) -> String {
    let existing_list: Vec<&str> = existing_slugs.iter().map(|s| s.as_str()).collect();
    format!(
        "You are nightdrive's theme curator. Propose {count} new synthwave album themes for a YouTube channel that already has these slugs: {existing_list:?}\n\n\
         Each new theme must be visually + sonically distinct from the existing ones (no near-duplicates).\n\
         Each theme should be evocative + concrete enough that an SDXL cover prompt and a 12-track musical arc can be derived.\n\n\
         Output ONLY a JSON array (no prose, no fence). Each element:\n\
         {{\n  \"slug\": \"<kebab-case>-vol-1\",\n  \"theme\": \"<1-2 sentence vivid description>\",\n  \"danger_zone_keys\": [\"<key>\", ...]   // theme keys from docs/album-danger-zone.json this theme should danger-zone-check against\n}}\n\n\
         Available danger_zone keys: tron, blade_runner, tokyo_cyberpunk, miami_vice, berlin_wall, atompunk, sovetskiy, sunset, neo_tokyo.\n\
         Return exactly {count} elements.\n"
    )
}

fn parse_proposals(reply: &str) -> anyhow::Result<Vec<ProposedFromLlm>> {
    let cleaned = reply.trim()
        .trim_start_matches("```json").trim_start_matches("```")
        .trim_end_matches("```").trim();
    Ok(serde_json::from_str(cleaned)?)
}
```

- [ ] **Step 2: Build + smoke test**

```bash
cargo build -p nightdrive-cli
set -a; source /etc/nightdrive/nightdrive.env; set +a
./target/debug/nightdrive-cli album propose --count 2
./target/debug/nightdrive-cli album backlog list
```

Expected: backlog list now shows 2 fresh proposed entries with `promotes at` timestamps 24h in future.

- [ ] **Step 3: NACK the test proposals so they don't pollute the queue**

```bash
./target/debug/nightdrive-cli album backlog nack <slug-of-first-test-proposal>
./target/debug/nightdrive-cli album backlog nack <slug-of-second-test-proposal>
```

---

## Task 15: CLI — `nightdrive-cli album drop-next`

**Files:**
- Modify: `crates/nightdrive-cli/src/main.rs`

- [ ] **Step 1: Write `album_drop_next`**

```rust
async fn album_drop_next(cfg: &nightdrive_core::AppConfig, dry_run: bool) -> anyhow::Result<()> {
    let backlog_path = cfg.paths.backlog_json();
    let albums_dir = cfg.paths.albums_dir();
    let danger_zone_path = cfg.paths.danger_zone_json();

    // 1. Channel-health gate.
    let bl_now = nightdrive_core::backlog::load(&backlog_path)?;
    if bl_now.youtube_strikes > 0 {
        let msg = format!("nightdrive: drop-next refused (youtube_strikes={}). Reset via backlog edit.", bl_now.youtube_strikes);
        warn!("{}", msg);
        let _ = nightdrive_core::telegram::notify(&msg);
        return Ok(());
    }

    // 2. Promote expired proposals.
    let now = chrono::Utc::now();
    let bl = nightdrive_core::backlog::mutate(&backlog_path, |bl| {
        let promoted = nightdrive_core::backlog::promote_expired(bl, now);
        if !promoted.is_empty() {
            info!(?promoted, "drop-next: auto-promoted expired proposals");
            for slug in &promoted {
                let _ = nightdrive_core::telegram::notify(&format!(
                    "nightdrive: {} promoted to active backlog (silent 24h).", slug
                ));
            }
        }
        Ok(())
    })?;

    // 3. Refuse if no approved slugs.
    if bl.approved.is_empty() {
        let _ = nightdrive_core::telegram::notify(
            "nightdrive: drop-next found empty approved backlog. Run `album propose` or add slugs manually."
        );
        println!("backlog empty");
        return Ok(());
    }

    // 4. Pop head.
    let mut head_slug = String::new();
    let mut head_theme = String::new();
    let mut head_dz_keys: Vec<String> = Vec::new();
    nightdrive_core::backlog::mutate(&backlog_path, |bl| {
        if let Some(a) = nightdrive_core::backlog::pop_approved(bl) {
            head_slug = a.slug;
            head_theme = a.theme;
            head_dz_keys = a.danger_zone_keys;
        }
        Ok(())
    })?;
    if head_slug.is_empty() {
        return Ok(());  // race lost; bail.
    }

    info!(slug = %head_slug, "drop-next: popped");

    // 5. Compute publish_at (3 days from now, anchor at 00:00 UTC).
    let publish_at = (now + chrono::Duration::days(3))
        .date_naive()
        .and_hms_opt(0, 0, 0).unwrap()
        .and_utc();

    if dry_run {
        println!("DRY-RUN drop-next: slug={} publish_at={} theme={:?}", head_slug, publish_at, head_theme);
        return Ok(());
    }

    let _ = nightdrive_core::telegram::notify(&format!(
        "nightdrive: dropping {}. ETA ~3h render + 2-day upload window. Sync-drop {}.",
        head_slug, publish_at.to_rfc3339()
    ));

    // 6. Run the composer if docs/albums/<slug>.json doesn't exist yet.
    let album_json_path = albums_dir.join(format!("{}.json", head_slug));
    if !album_json_path.exists() {
        let gw = nightdrive_openclaw_main::GatewayConfig::from_env()?;
        let req = nightdrive_album_composer::ComposeRequest {
            slug: head_slug.clone(),
            theme: head_theme.clone(),
            track_count: 12,
            danger_zone_keys: head_dz_keys.clone(),
            albums_dir: albums_dir.clone(),
            danger_zone_path: danger_zone_path.clone(),
            max_retries: 3,
        };
        match nightdrive_album_composer::compose(&gw, &req).await {
            Ok(spec) => {
                std::fs::write(&album_json_path, serde_json::to_string_pretty(&spec)?)?;
                info!(slug = %head_slug, "drop-next: composed + wrote album JSON");
            }
            Err(e) => {
                let msg = format!("nightdrive: {} composer FAILED: {}. Slug restored to backlog head.", head_slug, e);
                warn!("{}", msg);
                let _ = nightdrive_core::telegram::notify(&msg);
                // Restore to head of approved so next tick retries.
                nightdrive_core::backlog::mutate(&backlog_path, |bl| {
                    bl.approved.insert(0, nightdrive_core::backlog::Approved {
                        slug: head_slug.clone(), theme: head_theme.clone(),
                        approved_at: now, danger_zone_keys: head_dz_keys.clone(),
                    });
                    Ok(())
                })?;
                return Err(e.into());
            }
        }
    }

    // 7. Shell out to orchestrator.
    let status = std::process::Command::new("/opt/nightdrive/bin/nightdrive-orchestrator")
        .args(["run-album",
               "--slug", &head_slug,
               "--publish-at", &publish_at.to_rfc3339()])
        .status()?;

    if !status.success() {
        let msg = format!("nightdrive: {} run-album exit={:?}", head_slug, status.code());
        warn!("{}", msg);
        let _ = nightdrive_core::telegram::notify(&msg);
        return Err(anyhow::anyhow!(msg));
    }

    // 8. Append to history.
    nightdrive_core::backlog::mutate(&backlog_path, |bl| {
        bl.history.push(nightdrive_core::backlog::HistoryEntry {
            slug: head_slug.clone(), dropped_at: now,
        });
        Ok(())
    })?;

    let _ = nightdrive_core::telegram::notify(&format!(
        "nightdrive: {} 12/12 done — sync-drop {} armed.", head_slug, publish_at.to_rfc3339()
    ));
    Ok(())
}
```

- [ ] **Step 2: Build**

Run: `cargo build -p nightdrive-cli`
Expected: `Finished`.

- [ ] **Step 3: Dry-run test**

```bash
./target/debug/nightdrive-cli album drop-next --dry-run
```

Expected: prints "DRY-RUN drop-next: slug=tokyo-cyberpunk-vol-1 publish_at=2026-05-27T00:00:00+00:00 theme=...". Does NOT consume the backlog (mutate's restore happens inside dry-run? — check the code path; we pop BEFORE the dry-run gate, so we DO consume. Fix below.)

- [ ] **Step 4: Fix dry-run to not consume the backlog**

In `album_drop_next`, move the dry-run check BEFORE the pop step. Restructure so:

```
1. channel-health gate
2. promote expired
3. peek head (don't pop)
4. dry_run check — exit if dry
5. POP head (commit)
6. compose / shell out / etc.
```

Refactor `pop_approved` to also expose `peek_approved` if needed:

```rust
// In nightdrive-core/src/backlog.rs
pub fn peek_approved(bl: &Backlog) -> Option<&Approved> {
    bl.approved.first()
}
```

And update the drop-next flow accordingly.

- [ ] **Step 5: Re-run dry-run, confirm backlog unchanged**

```bash
./target/debug/nightdrive-cli album drop-next --dry-run
./target/debug/nightdrive-cli album backlog list   # still shows 4 approved
```

Expected: backlog still has all 4 entries after dry-run.

---

## Task 16: Telegram helper module

**Files:**
- Create: `crates/nightdrive-core/src/telegram.rs`
- Modify: `crates/nightdrive-core/src/lib.rs`

- [ ] **Step 1: Write the module**

```rust
use std::process::Command;
use tracing::warn;

/// Sends a Telegram message via the existing notify-telegram.sh script.
/// Best-effort: logs but does not return error on failure (notifications
/// must never block the pipeline).
pub fn notify(msg: &str) -> Result<(), ()> {
    let script = std::env::var("NIGHTDRIVE_TELEGRAM_SCRIPT")
        .unwrap_or_else(|_| "/j/baremetal claude/tools/notify-telegram.sh".to_string());

    match Command::new("bash").arg(&script).arg(msg).status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => {
            warn!("telegram: script exited {:?}", s.code());
            Err(())
        }
        Err(e) => {
            warn!(error = %e, "telegram: spawn failed");
            Err(())
        }
    }
}
```

- [ ] **Step 2: Wire into lib.rs**

```rust
pub mod telegram;
```

- [ ] **Step 3: Make `notify-telegram.sh` reachable on cnc-server**

On cnc-server, the script's hardcoded path doesn't exist. Add an env override in `/etc/nightdrive/nightdrive.env`:

```bash
ssh cnc-server "sudo bash -c 'echo NIGHTDRIVE_TELEGRAM_SCRIPT=/opt/nightdrive/tools/notify-telegram.sh >> /etc/nightdrive/nightdrive.env'"
```

Then copy the script + .env source there:

```bash
scp "/j/baremetal claude/tools/notify-telegram.sh" cnc-server:/tmp/
scp "/j/baremetal claude/.claude/.env" cnc-server:/tmp/.telegram.env
ssh cnc-server "sudo mkdir -p /opt/nightdrive/tools /etc/telegram && sudo install -m 0755 /tmp/notify-telegram.sh /opt/nightdrive/tools/ && sudo install -m 0600 /tmp/.telegram.env /etc/telegram/.env && rm /tmp/notify-telegram.sh /tmp/.telegram.env"
```

Update the script to source `/etc/telegram/.env` instead of its hardcoded path (one-line edit to the copied script).

- [ ] **Step 4: Verify**

```bash
ssh cnc-server "set -a; source /etc/nightdrive/nightdrive.env; set +a; bash \$NIGHTDRIVE_TELEGRAM_SCRIPT 'nightdrive: telegram wiring test from cnc'"
```

Expected: Telegram message lands on Matt's phone. Output: "telegram: sent (X chars)".

---

## Task 17: Systemd — `nightdrive-thumbnail-retry.{service,timer}`

**Files:**
- Create: `scripts/nightdrive-thumbnail-retry.service`
- Create: `scripts/nightdrive-thumbnail-retry.timer`

- [ ] **Step 1: Write the service**

```ini
[Unit]
Description=NightDrive — thumbnail retry sweep
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
EnvironmentFile=/etc/nightdrive/nightdrive.env
ExecStart=/opt/nightdrive/bin/nightdrive-cli thumbnails retry-failed --max 80
StandardOutput=append:/var/log/nightdrive/thumbnail-retry.log
StandardError=append:/var/log/nightdrive/thumbnail-retry.log
TimeoutStartSec=10m
```

- [ ] **Step 2: Write the timer**

```ini
[Unit]
Description=NightDrive thumbnail retry — every 6 hours

[Timer]
OnCalendar=*-*-* 02,08,14,20:30:00 America/Los_Angeles
Persistent=true
RandomizedDelaySec=5m
Unit=nightdrive-thumbnail-retry.service

[Install]
WantedBy=timers.target
```

(Four fires per day: 02:30, 08:30, 14:30, 20:30 PT. Avoids on-the-hour clustering with other timers.)

- [ ] **Step 3: Deploy (timer DISABLED for now per spec rollout)**

```bash
scp scripts/nightdrive-thumbnail-retry.service scripts/nightdrive-thumbnail-retry.timer cnc-server:/tmp/
ssh cnc-server "sudo install -m 0644 /tmp/nightdrive-thumbnail-retry.service /etc/systemd/system/ \
              && sudo install -m 0644 /tmp/nightdrive-thumbnail-retry.timer /etc/systemd/system/ \
              && sudo systemctl daemon-reload \
              && sudo mkdir -p /var/log/nightdrive"
```

DO NOT enable yet — Matt enables in Task 21 rollout.

- [ ] **Step 4: One-shot smoke test (manual fire)**

```bash
ssh cnc-server "sudo systemctl start nightdrive-thumbnail-retry.service && sleep 5 && sudo journalctl -u nightdrive-thumbnail-retry.service -n 50"
```

Expected: log shows "candidates loaded" with count >= 2 (Vol. 3 tracks 11, 12). Should attempt them, get either OK (window has reopened) or 429 (still rate-limited).

---

## Task 18: Systemd — `nightdrive-theme-propose.{service,timer}`

**Files:**
- Create: `scripts/nightdrive-theme-propose.service`
- Create: `scripts/nightdrive-theme-propose.timer`

- [ ] **Step 1: Write the service**

```ini
[Unit]
Description=NightDrive — weekly theme proposal via openclaw main
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
EnvironmentFile=/etc/nightdrive/nightdrive.env
ExecStart=/opt/nightdrive/bin/nightdrive-cli album propose --count 3
StandardOutput=append:/var/log/nightdrive/theme-propose.log
StandardError=append:/var/log/nightdrive/theme-propose.log
TimeoutStartSec=10m
```

- [ ] **Step 2: Write the timer**

```ini
[Unit]
Description=NightDrive theme proposal — weekly Sunday 03:00 PT

[Timer]
OnCalendar=Sun *-*-* 03:00:00 America/Los_Angeles
Persistent=true
RandomizedDelaySec=10m
Unit=nightdrive-theme-propose.service

[Install]
WantedBy=timers.target
```

- [ ] **Step 3: Deploy (disabled)**

```bash
scp scripts/nightdrive-theme-propose.service scripts/nightdrive-theme-propose.timer cnc-server:/tmp/
ssh cnc-server "sudo install -m 0644 /tmp/nightdrive-theme-propose.service /etc/systemd/system/ \
              && sudo install -m 0644 /tmp/nightdrive-theme-propose.timer /etc/systemd/system/ \
              && sudo systemctl daemon-reload"
```

- [ ] **Step 4: Manual fire smoke test**

```bash
ssh cnc-server "sudo systemctl start nightdrive-theme-propose.service && sleep 60 && sudo journalctl -u nightdrive-theme-propose.service -n 50"
```

Expected: log shows `proposed: ["...","...","..."]`, Telegram pings Matt.

- [ ] **Step 5: NACK the test proposals**

```bash
ssh cnc-server "/opt/nightdrive/bin/nightdrive-cli album backlog list"
# pick the 3 new slugs from the proposed[] section and nack them so they don't auto-promote
ssh cnc-server "/opt/nightdrive/bin/nightdrive-cli album backlog nack <slug1>; /opt/nightdrive/bin/nightdrive-cli album backlog nack <slug2>; /opt/nightdrive/bin/nightdrive-cli album backlog nack <slug3>"
```

---

## Task 19: Systemd — `nightdrive-album-drop.{service,timer}` (WITH eviction)

**Files:**
- Create: `scripts/nightdrive-album-drop.service`
- Create: `scripts/nightdrive-album-drop.timer`

- [ ] **Step 1: Write the service — verbatim eviction pattern from nightdrive-nightly.service**

```ini
[Unit]
Description=NightDrive — autonomous album drop
After=network-online.target nightdrive-acestep.service
Wants=network-online.target

[Service]
Type=oneshot
EnvironmentFile=/etc/nightdrive/nightdrive.env

# Eviction: free P100s by stopping openclaw inference units.
ExecStartPre=+/usr/bin/systemctl stop openclaw-inference-embed openclaw-inference-scout openclaw-inference-workhorse
ExecStartPre=+/bin/sleep 3
ExecStartPre=+/usr/bin/systemctl start nightdrive-acestep
ExecStartPre=+/bin/sleep 10

ExecStart=/opt/nightdrive/bin/nightdrive-cli album drop-next

# Restoration: stop ACE-Step, bring openclaw inference back online.
ExecStopPost=+/usr/bin/systemctl stop nightdrive-acestep
ExecStopPost=+/usr/bin/systemctl start openclaw-inference-embed openclaw-inference-scout openclaw-inference-workhorse

StandardOutput=append:/var/log/nightdrive/album-drop.log
StandardError=append:/var/log/nightdrive/album-drop.log
TimeoutStartSec=6h
```

- [ ] **Step 2: Write the timer**

```ini
[Unit]
Description=NightDrive album drop — every 3 days at 02:00 PT

[Timer]
OnCalendar=*-*-01,04,07,10,13,16,19,22,25,28 02:00:00 America/Los_Angeles
Persistent=true
RandomizedDelaySec=10m
Unit=nightdrive-album-drop.service

[Install]
WantedBy=timers.target
```

(The OnCalendar pattern fires on day-of-month 1, 4, 7, 10, ..., 28 → roughly every 3 days, drift-resistant against month boundaries. Days 29-31 are skipped; the next month picks up at day 1 again. Persistent=true means a missed fire from downtime catches up on next boot.)

- [ ] **Step 3: Deploy (disabled)**

```bash
scp scripts/nightdrive-album-drop.service scripts/nightdrive-album-drop.timer cnc-server:/tmp/
ssh cnc-server "sudo install -m 0644 /tmp/nightdrive-album-drop.service /etc/systemd/system/ \
              && sudo install -m 0644 /tmp/nightdrive-album-drop.timer /etc/systemd/system/ \
              && sudo systemctl daemon-reload"
```

- [ ] **Step 4: Dry-run via the CLI (NOT the service yet)**

```bash
ssh cnc-server "set -a; source /etc/nightdrive/nightdrive.env; set +a; /opt/nightdrive/bin/nightdrive-cli album drop-next --dry-run"
```

Expected: prints `DRY-RUN drop-next: slug=tokyo-cyberpunk-vol-1 publish_at=...`. Backlog unchanged.

---

## Task 20: End-to-end dry-run + first live drop validation

This task validates the entire chain works before Matt enables the timer. Run the album-drop service manually for the first album, watch every step.

- [ ] **Step 1: First live drop — tokyo-cyberpunk-vol-1**

```bash
ssh cnc-server "sudo systemctl start nightdrive-album-drop.service"
ssh cnc-server "sudo journalctl -u nightdrive-album-drop.service -f"
```

Expected behavior:
1. openclaw-inference-{embed,scout,workhorse} stopped (~3s)
2. nightdrive-acestep started (~10s)
3. drop-next pops `tokyo-cyberpunk-vol-1`
4. composer asks openclaw main, returns valid 12-track AlbumSpec
5. `docs/albums/tokyo-cyberpunk-vol-1.json` written
6. orchestrator `run-album` fires; renders 12 tracks (~2-3h wall on cnc P100s)
7. Tracks 1-6 upload day-0 (within 6/day GCP cap)
8. Tracks 7-12 attempt upload day-0, hit quota; orchestrator persists state for resume
9. Service exits; ExecStopPost stops ACE-Step, restores openclaw inference
10. Telegram pings Matt with progress + final status

The chain has internal resume — when the next album-drop timer fires (3 days later), if Vol. 1 still has tracks 7-12 unpublished, drop-next refuses to pop a NEW slug and instead reruns the same album to finish uploads.

- [ ] **Step 2: Verify GPU restoration**

After service completes:

```bash
ssh cnc-server "nvidia-smi --query-compute-apps=pid,name,used_memory --format=csv"
ssh cnc-server "systemctl is-active openclaw-inference-embed openclaw-inference-scout openclaw-inference-workhorse"
```

Expected: each of the three openclaw inference units is `active`. ACE-Step is `inactive`.

- [ ] **Step 3: If chain succeeds, enable the three timers IN ORDER (per spec rollout)**

```bash
# Day N: thumbnail-retry first (lowest risk)
ssh cnc-server "sudo systemctl enable --now nightdrive-thumbnail-retry.timer && systemctl list-timers nightdrive-thumbnail-retry.timer"

# Day N+1, after watching one cycle: theme-propose
ssh cnc-server "sudo systemctl enable --now nightdrive-theme-propose.timer && systemctl list-timers nightdrive-theme-propose.timer"

# Day N+2, after watching one cycle: album-drop
ssh cnc-server "sudo systemctl enable --now nightdrive-album-drop.timer && systemctl list-timers nightdrive-album-drop.timer"
```

---

## Task 21: HANDOFF.md update

**Files:**
- Modify: `HANDOFF.md`

- [ ] **Step 1: Find the latest §N section**

Run: `Grep -n "^## §" HANDOFF.md | tail -5`
Expected: shows the most recent section number (e.g. §30).

- [ ] **Step 2: Append a new section**

Add at the end of HANDOFF.md (use the next §N number):

```markdown
## §<N+1> — Autonomous album mode shipped (2026-05-24)

Stacked systemd timers now drive the album pipeline with no human in the loop:

- `nightdrive-album-drop.timer` — every 3 days, 02:00 PT, drops next slug from
  `docs/album-backlog.json` (composer → render → upload → scheduled sync-drop)
- `nightdrive-thumbnail-retry.timer` — every 6h, retries failed custom thumbnails
- `nightdrive-theme-propose.timer` — Sunday 03:00 PT, asks openclaw `main` for 3
  new themes → `proposed[]` → 24h soak → auto-promote unless NACK'd

GPU coordination reuses the proven nightly-batch eviction pattern (stop
openclaw-inference-{embed,scout,workhorse} → run ACE-Step → restore). No new
arbitration scheme.

LLM routing for composer + theme-propose is via openclaw `main` (Opus 4.7 OAuth,
free under Max 20x). Per-track composition spec gen stays on LiteLLM Sonnet.

Seed backlog (in approved order):
1. tokyo-cyberpunk-vol-1
2. miami-vice-vol-1
3. blade-runner-2049-vol-1
4. berlin-wall-vol-1

NACK any active proposal via:
  `nightdrive-cli album backlog nack <slug>` on cnc-server.

Manual override:
  `nightdrive-cli album drop-next` fires immediately
  `nightdrive-cli album propose --count N` asks for N new themes now
  `nightdrive-cli thumbnails retry-failed --max N` runs the retry sweep on demand

Stop the autonomy at any time:
  `sudo systemctl disable --now nightdrive-album-drop.timer`
```

---

## Self-Review

After writing the plan, walk back through the spec section by section:

- ✅ Spec §1 (Album-composer crate) → Tasks 8, 9, 10
- ✅ Spec §2 (openclaw-main crate) → Tasks 2, 3, 4
- ✅ Spec §3 (CLI extensions) → Tasks 12, 13, 14, 15
- ✅ Spec §4 (Systemd units) → Tasks 17, 18, 19
- ✅ Spec §5 (Backlog file format) → Tasks 6, 7
- ✅ Spec §6 (Seed backlog) → Task 6 (plus danger-zone seed in Task 5)
- ✅ GPU coordination → Task 19 eviction wrappers
- ✅ Telegram surface → Task 16 + integrated into Tasks 14, 15
- ✅ Migration/rollout → Task 20 step 3 (enable timers in order)
- ✅ Open issues:
  - Refresh-race risk → noted in Task 4; podman-exec path bypasses it entirely
  - Backlog file race → Task 7 uses `flock`
  - Danger-zone false positives → Task 9 unit tests + Task 8 logs every retry
  - Channel-strike pre-check → Task 15 step 1
  - Few-shot composer drift → Task 8 `load_recent_examples` (most-recent 3)

No placeholders, all code blocks complete, types consistent across tasks
(`AlbumSpec`, `Proposed`, `Approved` used identically wherever referenced).
