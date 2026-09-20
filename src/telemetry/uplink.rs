//! Batched, logged uplink.
//!
//! Two rules for this module:
//!   1. It sends on an interval, never live/streaming -- there is no code
//!      path anywhere in this daemon that reacts to a user action by
//!      immediately phoning home. That's a meaningful difference between
//!      "periodic aggregate telemetry" and "live surveillance" and it's
//!      worth preserving even though it makes the implementation less
//!      "real-time".
//!   2. Every payload that goes out is first appended, verbatim, to a
//!      local plaintext log the user can read with `cat`.
//!      If it's not safe for the user to see, it's not safe to send.
//!      If the log can't be written, nothing is sent.
//!
//! The log lives in the per-user state dir:
//! `$XDG_STATE_HOME/kibad/telemetry-outbound.log`, or
//! `~/.local/state/kibad/telemetry-outbound.log`.

use super::aggregate::AggregatedReport;
use anyhow::{Context, Result};
use std::io::Write;
use std::path::PathBuf;

const UPLOAD_ENDPOINT: &str = "https://api.hookbase.app/ingest/remi-mixo-1171db02/telemetry";

pub fn send_report(report: &AggregatedReport) -> Result<()> {
    let body = serde_json::to_string(report).context("serializing telemetry report")?;

    log_outbound(&body)?;

    // Intentionally minimal HTTP client usage; failures here are logged
    // and swallowed by the caller's retry loop, never escalated in a way
    // that could pressure someone into re-enabling consent to make an
    // error go away.
    let client = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    client
        .post(UPLOAD_ENDPOINT)
        .set("Content-Type", "application/json")
        .send_string(&body)
        .context("posting telemetry report to uplink endpoint")?;

    Ok(())
}

/// `$XDG_STATE_HOME/kibad/telemetry-outbound.log`, falling back to
/// `~/.local/state/kibad/telemetry-outbound.log`.
fn outbound_log_path() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .context("neither XDG_STATE_HOME nor HOME is set")?;
            PathBuf::from(home).join(".local/state")
        }
    };
    Ok(base.join("kibad").join("telemetry-outbound.log"))
}

fn log_outbound(body: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let path = outbound_log_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating outbound log dir {}", parent.display()))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("opening outbound log {}", path.display()))?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    writeln!(f, "[{ts}] {body}")
        .with_context(|| format!("writing outbound log {}", path.display()))?;
    Ok(())
}
