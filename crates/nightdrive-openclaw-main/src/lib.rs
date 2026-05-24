use thiserror::Error;
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

#[instrument(skip(cfg, prompt), fields(container = %cfg.container, prompt_len = prompt.len()))]
pub async fn ask_main(cfg: &GatewayConfig, prompt: &str) -> Result<String, OpenclawMainError> {
    let mut cmd = if cfg.use_sudo {
        let mut c = Command::new("sudo");
        c.arg("podman");
        c
    } else {
        Command::new("podman")
    };
    cmd.args(["exec", &cfg.container, "openclaw", "agent",
              "--agent", "main",
              "--message", prompt,
              "--json"]);
    cmd.stdin(std::process::Stdio::null());

    let fut = cmd.output();
    let out = tokio::time::timeout(std::time::Duration::from_secs(cfg.timeout_secs), fut)
        .await
        .map_err(|_| OpenclawMainError::Timeout(cfg.timeout_secs))?
        .map_err(|e| OpenclawMainError::SpawnIo(e.to_string()))?;

    if !out.status.success() {
        return Err(OpenclawMainError::AgentNonZero {
            exit: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
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
        .ok_or_else(|| OpenclawMainError::MissingReplyField(
            String::from_utf8_lossy(&out.stdout).chars().take(500).collect()
        ))
}
