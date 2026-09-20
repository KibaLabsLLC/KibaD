//! Report assembly for telemetry.
//!
//! Takes the collected host inventory and wraps it in a versioned report
//! keyed by a pseudonymous install ID. The raw install salt never leaves
//! the machine; only a one-way hash of it is sent.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::collectors::DeviceInventory;

/// Bump when the shape of `AggregatedReport` changes so the server can
/// tell old and new payloads apart.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregatedReport {
    pub schema_version: u32,

    /// Pseudonymous, stable per-install identifier: SHA-256 of the local
    /// salt with a domain-separation prefix. Not reversible to the salt.
    pub install_id: String,

    /// Day-granularity bucket (days since the Unix epoch).
    pub period: String,

    /// KibaD version that produced this report.
    pub kibad_version: String,

    pub device: DeviceInventory,
}

pub fn build_report(
    install_salt: &str,
    period: &str,
    device: DeviceInventory,
) -> AggregatedReport {
    AggregatedReport {
        schema_version: SCHEMA_VERSION,
        install_id: derive_install_id(install_salt),
        period: period.to_string(),
        kibad_version: env!("CARGO_PKG_VERSION").to_string(),
        device,
    }
}

fn derive_install_id(install_salt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"kibad-telemetry-install-id-v1:");
    hasher.update(install_salt.as_bytes());
    hex::encode(hasher.finalize())
}
