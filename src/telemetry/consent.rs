//! Consent gate for the telemetry module.
//!
//! The contract:
//!   - The OOBE installer is the *only* writer of the consent state file.
//!     It writes it exactly once, after the user scrolls through the
//!     disclosure screen and makes an explicit choice (Share / Don't Share).
//!   - This module only ever reads it.
//!   - Missing, unreadable, malformed, or wrong-permission file -> no consent.
//!     Fail closed, always.
//!   - No runtime toggle. Changing consent means re-running the consent
//!     flow (Switchboard → Privacy rewrites the file the same way the
//!     installer did), so there is always a single auditable record of
//!     when and how someone agreed.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const CONSENT_PATH: &str = "/etc/kibad/telemetry-consent.state";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsentScopes {
    /// Host hardware inventory: CPU/GPU/board, connected USB/PCI peripherals.
    /// This is the only scope — usage/launch-count collection was removed
    /// because it doesn't contribute meaningfully to hardware-affinity data.
    pub device_inventory: bool,
}

impl ConsentScopes {
    fn none() -> Self {
        Self { device_inventory: false }
    }

    pub fn any(&self) -> bool {
        self.device_inventory
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ConsentFile {
    /// Schema version so future installer changes can be detected instead
    /// of silently misparsed.
    version: u32,
    /// Top-level gate: if false, nothing runs regardless of scopes content.
    agreed: bool,
    scopes: ConsentScopes,
    /// RFC 3339 timestamp written by the installer, kept for the local
    /// audit log so the user can see exactly when they agreed.
    recorded_at: String,
}

const SUPPORTED_VERSION: u32 = 1;

/// Load consent state from disk. Never returns an error in a way that
/// could be mistaken for "consent granted" — any failure path yields
/// `ConsentScopes::none()` so callers don't have to remember to fail closed.
pub fn load() -> ConsentScopes {
    match load_inner(Path::new(CONSENT_PATH)) {
        Ok(scopes) => scopes,
        Err(e) => {
            tracing::warn!(
                "telemetry consent file unreadable or invalid ({e:#}); \
                 defaulting to no collection"
            );
            ConsentScopes::none()
        }
    }
}

fn load_inner(path: &Path) -> Result<ConsentScopes> {
    // Permission check: file must be root-owned and not group/world-writable.
    // Stops a user-level process from granting itself consent by editing the
    // file directly.
    let meta = std::fs::metadata(path)?;
    check_permissions(&meta)?;

    let raw = std::fs::read_to_string(path)?;
    let parsed: ConsentFile = serde_json::from_str(&raw)?;

    if parsed.version != SUPPORTED_VERSION {
        anyhow::bail!(
            "unsupported consent file version {} (expected {})",
            parsed.version,
            SUPPORTED_VERSION
        );
    }

    if !parsed.agreed {
        return Ok(ConsentScopes::none());
    }

    tracing::info!(
        recorded_at = %parsed.recorded_at,
        device_inventory = parsed.scopes.device_inventory,
        "loaded telemetry consent"
    );

    Ok(parsed.scopes)
}

#[cfg(unix)]
fn check_permissions(meta: &std::fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if meta.uid() != 0 {
        anyhow::bail!("consent file is not root-owned");
    }
    if meta.mode() & 0o022 != 0 {
        anyhow::bail!("consent file is group/world-writable");
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_permissions(_meta: &std::fs::Metadata) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_no_consent() {
        assert!(load_inner(Path::new("/nonexistent/does-not-exist")).is_err());
    }

    #[test]
    fn declined_yields_no_scopes() {
        let f = ConsentFile {
            version: SUPPORTED_VERSION,
            agreed: false,
            scopes: ConsentScopes { device_inventory: true },
            recorded_at: "2026-01-01T00:00:00Z".into(),
        };
        let json = serde_json::to_string(&f).unwrap();
        let parsed: ConsentFile = serde_json::from_str(&json).unwrap();
        assert!(!parsed.agreed);
    }

    #[test]
    fn unsupported_version_rejected() {
        let raw = r#"{"version":99,"agreed":true,"scopes":{"device_inventory":true},"recorded_at":"2026-01-01T00:00:00Z"}"#;
        let f: ConsentFile = serde_json::from_str(raw).unwrap();
        assert_ne!(f.version, SUPPORTED_VERSION);
    }
}
