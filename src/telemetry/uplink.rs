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
//!      local plaintext log the user can read with `journalctl` or `cat`.
//!      If it's not safe for the user to see, it's not safe to send.

use super::aggregate::AggregatedReport;
use anyhow::Result;
use std::io::Write;

const OUTBOUND_LOG_PATH: &str = "/var/log/kibad/telemetry-outbound.log";
const UPLOAD_ENDPOINT: &str = "https://api.hookbase.app/ingest/remi-mixo-1171db02/telemetry";

pub fn send_report(report: &AggregatedReport) -> Result<()> {
    let body = serde_json::to_string(report)?;

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
        .send_string(&body)?;

    Ok(())
}

fn log_outbound(body: &str) -> Result<()> {
    if let Some(parent) = std::path::Path::new(OUTBOUND_LOG_PATH).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(OUTBOUND_LOG_PATH)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    writeln!(f, "[{ts}] {body}")?;
    Ok(())
}
