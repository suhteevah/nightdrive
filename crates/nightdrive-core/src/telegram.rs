//! Best-effort Telegram notification via the existing notify-telegram.sh script.
//! Never blocks or fails the caller — logs on failure and returns Err(()) so the
//! caller can decide to count failures if needed.

use std::process::Command;
use tracing::warn;

/// Sends a Telegram message via the notify script. Path overridable via
/// NIGHTDRIVE_TELEGRAM_SCRIPT env var (default `/j/baremetal claude/tools/notify-telegram.sh`
/// for kokonoe dev; `/opt/nightdrive/tools/notify-telegram.sh` is the cnc convention).
///
/// Returns Ok(()) on script exit 0, Err(()) on any failure. Never panics. The
/// caller is expected to log-and-continue — notification failures must not
/// break pipeline progress.
pub fn notify(msg: &str) -> Result<(), ()> {
    let script = std::env::var("NIGHTDRIVE_TELEGRAM_SCRIPT")
        .unwrap_or_else(|_| "/j/baremetal claude/tools/notify-telegram.sh".to_string());

    match Command::new("bash").arg(&script).arg(msg).status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => {
            warn!(script = %script, exit = ?s.code(), "telegram: script exited non-zero");
            Err(())
        }
        Err(e) => {
            warn!(script = %script, error = %e, "telegram: spawn failed");
            Err(())
        }
    }
}
