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

use serde::{Deserialize, Serialize};
use std::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeviceInventory {
    pub cpu_model: Option<String>,
    pub gpu_models: Vec<String>,
    pub board_vendor: Option<String>,
    pub board_model: Option<String>,
    /// Vendor/product strings for USB peripherals attached to *this* host
    /// from `lsusb` — keyboards, mice, docks, webcams, etc.
    /// No MAC addresses or serial numbers: vendor:product descriptive
    /// strings only, so this can't track a specific physical unit.
    pub usb_peripherals: Vec<String>,
}

pub fn collect_device_inventory() -> DeviceInventory {
    DeviceInventory {
        cpu_model:      read_cpu_model(),
        gpu_models:     read_gpu_models(),
        board_vendor:   read_dmi("board_vendor"),
        board_model:    read_dmi("board_name"),
        usb_peripherals: read_usb_peripherals(),
    }
}

fn read_cpu_model() -> Option<String> {
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    cpuinfo
        .lines()
        .find(|l| l.starts_with("model name"))
        .and_then(|l| l.split(':').nth(1))
        .map(|s| s.trim().to_string())
}

fn read_gpu_models() -> Vec<String> {
    // `lspci` is in pciutils which is part of the KibaOS base install.
    // Shell out rather than linking libpci to keep this collector
    // dependency-free and easy to strace-audit.
    let out = match Command::new("lspci").arg("-mm").output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| {
            l.contains("\"VGA compatible controller\"")
                || l.contains("\"3D controller\"")
                || l.contains("\"Display controller\"")
        })
        .filter_map(|l| l.split('"').nth(5).map(|s| s.to_string()))
        .collect()
}

fn read_dmi(field: &str) -> Option<String> {
    let path = format!("/sys/class/dmi/id/{field}");
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "To Be Filled By O.E.M.")
}

fn read_usb_peripherals() -> Vec<String> {
    let out = match Command::new("lsusb").output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            // Typical line: "Bus 001 Device 004: ID 046d:c52b Logitech, Inc. ..."
            // We keep everything after "ID " — vendor:product + description.
            // No bus/device numbers (those are transient), no serial numbers.
            let idx = l.find("ID ")?;
            let rest = l[idx + 3..].trim().to_string();
            // Skip root hubs and generic USB hubs — not useful for brand data.
            if rest.contains("Linux Foundation") || rest.contains("root hub") {
                return None;
            }
            Some(rest)
        })
        .collect()
}
