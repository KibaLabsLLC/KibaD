//! Telemetry module: host hardware inventory, opt-in only.
//!
//! Structurally isolated from the rest of KibaD:
//!   - Only module that reads `consent::CONSENT_PATH`.
//!   - Only module with network egress.
//!   - Runs in its own thread with panic recovery, so a bug here can't
//!     take down AT-SPI tracking, the overlay, or anything else in main.
//!   - If consent is declined or the state file is missing/corrupt,
//!     `run()` returns immediately. No "collect but don't send" path.

mod aggregate;
mod collectors;
mod consent;
mod uplink;

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often a report is built and sent. Coarse on purpose — this is
/// trend data, not monitoring.
const REPORT_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60); // 6 h

/// Where the install salt lived before it moved to the per-user data dir.
/// Still read (never written) so existing installs keep their pseudonymous ID.
const LEGACY_SALT_PATH: &str = "/var/lib/kibad/telemetry-salt";

/// Entry point called from `main.rs`. Safe to call unconditionally;
/// the consent check inside decides whether anything actually runs.
pub fn run() {
    let scopes = consent::load();
    if !scopes.any() {
        tracing::info!("telemetry: no consent on file, module will not run");
        return;
    }

    tracing::info!(
        device_inventory = scopes.device_inventory,
        "telemetry: consent present, starting reporting loop"
    );

    let install_salt = load_or_create_install_salt();

    loop {
        let salt_clone = install_salt.clone();
        let result = std::panic::catch_unwind(move || report_once(&salt_clone));
        if let Err(payload) = result {
            tracing::error!(
                "telemetry: collection loop panicked, will retry next interval: {payload:?}"
            );
        }
        std::thread::sleep(REPORT_INTERVAL);
    }
}

fn report_once(install_salt: &str) {
    let device = collectors::collect_device_inventory();
    let period = current_period_label();
    let report = aggregate::build_report(install_salt, &period, device);

    if let Err(e) = uplink::send_report(&report) {
        tracing::warn!("telemetry: failed to send report this period: {e:#}");
    }
}

fn current_period_label() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Day-granularity bucket — coarse enough to avoid time-of-day patterns.
    format!("{}", secs / 86_400)
}

/// `$XDG_DATA_HOME/kibad/telemetry-salt`, or `~/.local/share/kibad/telemetry-salt`.
fn salt_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".local/share"))
        })?;
    Some(base.join("kibad").join("telemetry-salt"))
}

fn read_salt(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn persist_salt(path: &Path, salt: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(salt.as_bytes())
}

fn load_or_create_install_salt() -> String {
    let Some(path) = salt_path() else {
        tracing::warn!("telemetry: no HOME or XDG_DATA_HOME, install salt won't persist");
        return generate_salt();
    };

    // New location first, then the legacy system path.
    if let Some(existing) = read_salt(&path).or_else(|| read_salt(Path::new(LEGACY_SALT_PATH))) {
        return existing;
    }

    let salt = generate_salt();
    if let Err(e) = persist_salt(&path, &salt) {
        tracing::warn!(
            "telemetry: couldn't save install salt to {}: {e}",
            path.display()
        );
    }
    salt
}

fn generate_salt() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}
