use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::instrument;

#[derive(Debug, Error)]
pub enum OpenclawMainError {
    #[error("podman exec spawn failed: {0}")]
    SpawnIo(String),
    #[error("podman exec timeout after {0}s")]
    Timeout(u64),
    #[error("non-zero exit from openclaw agent: exit={exit:?}, stderr={stderr}")]
    AgentNonZero { exit: Option<i32>, stderr: String },
    #[error("unable to parse openclaw --json reply: {reason} (raw: {raw})")]
    ParseReply { reason: String, raw: String },
    #[error("reply missing $.result.payloads[0].text (raw: {0})")]
    MissingReplyField(String),
    #[error("config: {0}")]
    Config(String),
}

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    /// Container name for `podman exec`. Defaults to `openclaw-gateway`.
    pub container: String,
    /// Whether to wrap the podman call in `sudo`. Defaults to true (production cnc-server setup).
    /// Set NIGHTDRIVE_OPENCLAW_SUDO=0 to skip sudo (dev / when running as root already).
    pub use_sudo: bool,
    /// Timeout for the agent call in seconds. Defaults to 180.
    pub timeout_secs: u64,
}

impl GatewayConfig {
    pub fn from_env() -> Result<Self, OpenclawMainError> {
        let container = std::env::var("NIGHTDRIVE_OPENCLAW_CONTAINER")
            .unwrap_or_else(|_| "openclaw-gateway".to_string());
        let use_sudo = std::env::var("NIGHTDRIVE_OPENCLAW_SUDO")
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
            .unwrap_or(true);
        let timeout_secs = std::env::var("NIGHTDRIVE_OPENCLAW_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(180);
        Ok(Self { container, use_sudo, timeout_secs })
    }
}

/// Inline ESM wrapper written to the container's stdin via `node --input-type=module`.
///
/// Strategy: Linux's MAX_ARG_STRLEN (128 KB) caps each individual execve argument.
/// A 3-example album-composer prompt can exceed 290 KB.  Passing the prompt as
/// `--message <text>` hits that wall.
///
/// openclaw is a Node ESM binary that respawns itself on startup to inject
/// NODE_OPTIONS / CA-cert env vars.  That respawn re-issues the original argv,
/// so patching argv in a wrapper script still triggers E2BIG on the child spawn.
/// Setting OPENCLAW_NO_RESPAWN=1 tells openclaw to skip the respawn entirely.
///
/// Flow:
///   1. Rust writes `<prompt>\x00<esm_script>` to podman's stdin.
///   2. The sh -c payload splits on \x00, saves the prompt to a tempfile,
///      then pipes the ESM script to `node --input-type=module`.
///   3. The ESM script reads the tempfile, patches process.argv, and
///      dynamically imports /usr/local/bin/openclaw.
///   4. OPENCLAW_NO_RESPAWN=1 prevents openclaw from re-spawning with the
///      large argv, which would hit E2BIG again.
///
/// NOTE (2026-05-24): A WebSocket `chat.send` RPC path was investigated as a
/// cleaner alternative (no argv limit, no tempfile, no 5-layer spawn chain).
///
/// Protocol discovered:
///   URL: ws://127.0.0.1:18789/  (gateway port 18789, no sub-path)
///   Auth: Token-in-connect-params RPC (NOT a Bearer upgrade header).
///     1. Server sends  {type:"event", event:"connect.challenge", payload:{nonce,ts}}
///     2. Client sends  {type:"req", id:"1", method:"connect", params:{
///           minProtocol:4, maxProtocol:4,
///           client:{id:"cli", version:"1.0.0", platform:"linux", mode:"cli"},
///           scopes:["operator.admin",...], auth:{token:"<gateway_token>"}}}
///     3. Server replies {type:"res", id:"1", ok:true, payload:{type:"hello-ok",...}}
///     4. Client sends  {type:"req", id:"2", method:"chat.send", params:{
///           sessionKey:"agent:main:main", message:"<prompt>", idempotencyKey:"<uuid>"}}
///     5. Server streams {type:"event", event:"chat", payload:{state:"delta", deltaText:"..."}}
///        ... followed by {type:"event", event:"chat", payload:{state:"final",...}}
///
/// WHY IT DOESN'T WORK from an external client (nightdrive-orchestrator on cnc-server):
///   The gateway's `shouldClearUnboundScopesForMissingDeviceIdentity` function clears
///   all declared scopes for token-auth connections that lack device identity (keypair
///   pairing). This happens even for loopback connections. Since `chat.send` requires
///   `operator.write` scope and scopes are always empty after clearing, the call
///   returns `INVALID_REQUEST: missing scope: operator.write`.
///
///   The `openclaw agent` CLI works because it runs IN-PROCESS inside the container
///   and dispatches through the `[agent/cli-backend]` path, bypassing WS scope auth.
///
///   Workaround would require device identity (keypair + pairing flow), which is
///   disproportionate complexity for this use case.
///
/// VERDICT: The stdin-pipe ESM approach here IS the correct production path.
///   It handles 300 KB+ prompts cleanly (verified 2026-05-24 with real_compose_smoke).
///   The WS path is a dead end without device pairing support.
const WRAPPER_ESM: &str = r#"import { readFileSync } from "fs";
const msg = readFileSync("/tmp/nd_prompt_$.txt", "utf8");
process.argv = ["node", "/usr/local/bin/openclaw", "agent", "--agent", "main", "--message", msg, "--json"];
process.env.OPENCLAW_NO_RESPAWN = "1";
await import("/usr/local/bin/openclaw");
"#;

#[instrument(skip(cfg, prompt), fields(container = %cfg.container, prompt_len = prompt.len()))]
pub async fn ask_main(cfg: &GatewayConfig, prompt: &str) -> Result<String, OpenclawMainError> {
    use std::process::Stdio;

    // Build a unique tempfile name using a timestamp (no std::process::id inside
    // the container, but collision risk is negligible for sequential batch use).
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let tmpfile = format!("/tmp/nd_prompt_{ts}.txt");
    let esm = WRAPPER_ESM.replace("$", &ts.to_string());

    // The sh payload:
    //   1. Read until NUL byte → save as tempfile (the prompt).
    //   2. Read the rest       → pipe to `node --input-type=module` (the ESM wrapper).
    // We use Python's os.read loop (available in the container) to split on \x00,
    // since `read` in sh has no NUL-delimiter option.
    //
    // Simpler approach: send prompt first line-delimited, then ESM.
    // We use a sentinel line "---ESM---" that can't appear in a JSON prompt.
    // Patch (a): append `rm -f <tmpfile>` to sh payload so tempfile is cleaned up
    // even when openclaw exits non-zero (both the success and failure branches run cleanup).
    let sh_cmd = format!(
        r#"python3 -c "
import sys, os
data = sys.stdin.buffer.read()
sep = data.index(b'\x00')
prompt = data[:sep]
esm   = data[sep+1:]
open('{tmpfile}', 'wb').write(prompt)
import subprocess, sys
p = subprocess.run(['node','--input-type=module'], input=esm, env={{**os.environ,'OPENCLAW_NO_RESPAWN':'1'}})
os.unlink('{tmpfile}')
sys.exit(p.returncode)
""#,
        tmpfile = tmpfile
    );

    let mut cmd = if cfg.use_sudo {
        let mut c = Command::new("sudo");
        c.arg("podman");
        c
    } else {
        Command::new("podman")
    };
    // -i keeps stdin attached through podman exec to the container process.
    cmd.args(["exec", "-i", &cfg.container, "sh", "-c", &sh_cmd]);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| OpenclawMainError::SpawnIo(e.to_string()))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| OpenclawMainError::SpawnIo("no stdin handle".into()))?;

    // Write: <prompt bytes> NUL <esm bytes>
    let payload = {
        let mut v = Vec::with_capacity(prompt.len() + 1 + esm.len());
        v.extend_from_slice(prompt.as_bytes());
        v.push(0u8);
        v.extend_from_slice(esm.as_bytes());
        v
    };
    stdin
        .write_all(&payload)
        .await
        .map_err(|e| OpenclawMainError::SpawnIo(format!("write stdin: {e}")))?;
    drop(stdin); // close so the container process gets EOF

    let fut = child.wait_with_output();
    let out = tokio::time::timeout(std::time::Duration::from_secs(cfg.timeout_secs), fut)
        .await
        .map_err(|_| OpenclawMainError::Timeout(cfg.timeout_secs))?
        .map_err(|e| OpenclawMainError::SpawnIo(e.to_string()))?;

    // Patch (b): surface an actionable hint when OPENCLAW_NO_RESPAWN-related
    // signatures appear in stderr (e.g. if a future openclaw update removes the
    // undocumented flag).
    if !out.status.success() {
        let raw_stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let stderr = if raw_stderr.contains("OPENCLAW_NO_RESPAWN")
            || raw_stderr.contains("respawn")
            || raw_stderr.contains("E2BIG")
            || raw_stderr.contains("Argument list too long")
        {
            format!(
                "[hint: OPENCLAW_NO_RESPAWN / large-argv path may have broken — \
                 consider switching ask_main to the WebSocket chat.send RPC \
                 (see nightdrive-openclaw-main/src/lib.rs Class B TODO)] {raw_stderr}"
            )
        } else {
            raw_stderr
        };
        return Err(OpenclawMainError::AgentNonZero {
            exit: out.status.code(),
            stderr,
        });
    }

    let v: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| {
        OpenclawMainError::ParseReply {
            reason: e.to_string(),
            raw: String::from_utf8_lossy(&out.stdout).chars().take(500).collect(),
        }
    })?;

    v.pointer("/result/payloads/0/text")
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            OpenclawMainError::MissingReplyField(
                String::from_utf8_lossy(&out.stdout)
                    .chars()
                    .take(500)
                    .collect(),
            )
        })
}
