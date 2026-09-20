//! Telemetry collectors — this machine only.
//!
//! Scope: CPU/GPU/board identity, peripherals enumerated over USB/PCI
//! (things physically plugged into *this* machine). That's it.
//!
//! What this module deliberately does NOT do: enumerate other devices on
//! the LAN, ARP-scan the network, read router/DHCP tables, or fingerprint
//! anything that isn't this host. Those devices belong to other people.
//!
//! Launch counts and usage stats were removed — they don't contribute
//! meaningfully to hardware-affinity data, and the overhead of tracking
//! them (logging every app open, persisting counts across daemon restarts)
//! wasn't worth it.
//!
//! USB identifiers are deliberately limited to vendor/product IDs and
//! descriptive manufacturer/product text. Bus numbers, device numbers,
//! MAC addresses, and serial numbers are not collected.

use serde::{Deserialize, Serialize};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeviceInventory {
    pub cpu_model: Option<String>,
    pub gpu_models: Vec<String>,
    pub board_vendor: Option<String>,
    pub board_model: Option<String>,

    /// USB peripherals attached to *this* host.
    ///
    /// Only stable vendor/product identifiers and descriptive text are
    /// retained. Bus/device numbers and serial numbers are discarded.
    pub usb_peripherals: Vec<UsbPeripheral>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsbPeripheral {
    /// USB vendor ID, e.g. "046d".
    pub vendor_id: String,

    /// USB product ID, e.g. "c52b".
    pub product_id: String,

    /// Human-readable manufacturer/product description, if available.
    ///
    /// This is descriptive metadata from lsusb, not a serial number.
    pub description: Option<String>,
}

pub fn collect_device_inventory() -> DeviceInventory {
    DeviceInventory {
        cpu_model: read_cpu_model(),
        gpu_models: read_gpu_models(),
        board_vendor: read_dmi("board_vendor"),
        board_model: read_dmi("board_name"),
        usb_peripherals: read_usb_peripherals(),
    }
}

fn read_cpu_model() -> Option<String> {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").ok()?;

    cpuinfo
        .lines()
        .find(|line| line.starts_with("model name"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, value)| value.trim().to_string())
}

fn read_gpu_models() -> Vec<String> {
    // `lspci` is in pciutils, which is part of the KibaOS base install.
    // Shell out rather than linking libpci to keep this collector
    // dependency-free and easy to strace-audit.
    let out = match Command::new("lspci").arg("-mm").output() {
        Ok(output) => output,
        Err(_) => return Vec::new(),
    };

    if !out.status.success() {
        return Vec::new();
    }

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| {
            line.contains("\"VGA compatible controller\"")
                || line.contains("\"3D controller\"")
                || line.contains("\"Display controller\"")
        })
        .filter_map(|line| {
            // lspci -mm format:
            // "00:02.0" "VGA compatible controller" "Intel Corporation" "..."
            //
            // The fifth quoted field is the device/product description.
            line.split('"').nth(5).map(str::to_string)
        })
        .collect()
}

fn read_dmi(field: &str) -> Option<String> {
    let path = format!("/sys/class/dmi/id/{field}");

    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| {
            !value.is_empty()
                && !value.eq_ignore_ascii_case("To Be Filled By O.E.M.")
        })
}

fn read_usb_peripherals() -> Vec<UsbPeripheral> {
    let out = match Command::new("lsusb").output() {
        Ok(output) => output,
        Err(_) => return Vec::new(),
    };

    if !out.status.success() {
        return Vec::new();
    }

    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_lsusb_line)
        .collect()
}

/// Parse one normal `lsusb` line.
///
/// Example input:
///
///     Bus 001 Device 004: ID 046d:c52b Logitech, Inc. Unifying Receiver
///
/// The following are deliberately discarded:
///
///     - Bus number
///     - Device number
///     - USB serial numbers
///
/// The resulting record contains only:
///
///     vendor_id = "046d"
///     product_id = "c52b"
///     description = "Logitech, Inc. Unifying Receiver"
fn parse_lsusb_line(line: &str) -> Option<UsbPeripheral> {
    let id_marker = line.find("ID ")?;
    let after_id = line[id_marker + 3..].trim();

    let mut fields = after_id.splitn(2, char::is_whitespace);

    let device_id = fields.next()?;

    let (vendor_id, product_id) = device_id.split_once(':')?;

    // USB IDs are exactly four hexadecimal characters each.
    if vendor_id.len() != 4
        || product_id.len() != 4
        || !vendor_id.chars().all(|c| c.is_ascii_hexdigit())
        || !product_id.chars().all(|c| c.is_ascii_hexdigit())
    {
        return None;
    }

    // Everything after the vendor:product identifier is descriptive text.
    // We deliberately do not attempt to parse or retain serial numbers.
    let description = fields
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    // Root hubs and generic Linux USB infrastructure aren't useful for
    // hardware-affinity analysis.
    if description
        .as_deref()
        .is_some_and(|description| {
            description.contains("Linux Foundation")
                || description.to_ascii_lowercase().contains("root hub")
        })
    {
        return None;
    }

    Some(UsbPeripheral {
        vendor_id: vendor_id.to_ascii_lowercase(),
        product_id: product_id.to_ascii_lowercase(),
        description,
    })
}