//! Dynamic zram management — creates and grows compressed swap devices in RAM.
//!
//! zram is a compressed block device in RAM. Unlike zswap (cache in front of disk swap),
//! zram IS the swap. Pages compressed into zram never touch disk.
//!
//! Strategy:
//! 1. Start with a small zram device (initial_mb from config, default 4GB)
//! 2. Each cycle, if zram is >80% full and free RAM is available, add another device
//! 3. Cap total zram at max_percent of physical RAM (default 50%)
//!
//! The kernel load-balances across multiple swap devices automatically.

use anyhow::{Context, Result};
use log::{debug, info, warn};
use std::fs;
use std::path::Path;

use crate::tuning::Tuning;

/// vm.swappiness while zram is the swap device. A zram fault decompresses in
/// microseconds, so the kernel may favour swapping anonymous pages over
/// dropping file cache. Restored to the original on shutdown.
const ZRAM_SWAPPINESS: u32 = 100;

/// Manages zram compressed swap devices.
pub struct ZramManager {
    /// zram device indices we've created.
    devices: Vec<u32>,
    /// Max total zram size in bytes (capped at percent of RAM).
    max_total_bytes: u64,
    /// Size of each zram device in bytes.
    device_size_bytes: u64,
    /// Total physical RAM in bytes.
    total_ram_bytes: u64,
}

/// Current zram statistics.
pub struct ZramStats {
    /// Total zram capacity across all devices (bytes).
    pub total_capacity: u64,
    /// Data stored in zram before compression (bytes).
    pub original_bytes: u64,
    /// Actual compressed size in RAM (bytes).
    pub compressed_bytes: u64,
    /// RAM saved (original - compressed).
    pub saved_bytes: u64,
    /// Compression ratio.
    pub ratio: f64,
    /// Number of active zram devices.
    pub device_count: usize,
}

impl ZramManager {
    pub fn new() -> Self {
        let total_ram = total_ram_bytes();
        // Start with 3% of RAM per device, cap total at 50% of RAM.
        let device_size = (total_ram * 3 / 100).max(512 * 1024 * 1024); // min 512MB
        let max_total = total_ram / 2;

        Self {
            devices: Vec::new(),
            max_total_bytes: max_total,
            device_size_bytes: device_size,
            total_ram_bytes: total_ram,
        }
    }

    /// Set up initial zram. Disables zswap (conflicts), creates first device.
    pub fn init(&mut self, tuning: &mut Tuning) -> Result<()> {
        // Check if zram module is available.
        if !Path::new("/sys/class/zram-control").exists() {
            // Try to load the module.
            let status = std::process::Command::new("modprobe")
                .arg("zram")
                .status();
            match status {
                Ok(s) if s.success() => info!("Loaded zram kernel module"),
                _ => anyhow::bail!("zram kernel module not available — cannot create compressed swap"),
            }
        }

        // Remove any existing zram swap devices we don't own, but don't touch them.
        // Just create our first device.
        self.add_device(tuning)?;

        // Disable zswap — it conflicts with zram. Only now that a zram device
        // exists: if creation failed above, the daemon falls back to zswap,
        // which must still be on. Restored on shutdown.
        match tuning.set("/sys/module/zswap/parameters/enabled", "N") {
            Ok(true) => debug!("Disabled zswap (using zram instead)"),
            Ok(false) => {}
            Err(e) => warn!("Could not disable zswap: {e:#}"),
        }

        // With zram, "swap" is compressed RAM — not disk. A page fault on zram
        // decompresses in microseconds (same as L3 cache miss). High swappiness is
        // safe and desirable: the kernel actively compresses cold pages, freeing RAM.
        // Android runs at 100-200 on every phone. We use 100.
        if let Err(e) = tuning.set("/proc/sys/vm/swappiness", &ZRAM_SWAPPINESS.to_string()) {
            warn!("Could not set vm.swappiness={ZRAM_SWAPPINESS}: {e:#}");
        }
        let swappiness = fs::read_to_string("/proc/sys/vm/swappiness")
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown".into());

        info!(
            "zram initialised: {} MiB device, max {} MiB total ({:.0}% of RAM), swappiness={swappiness}",
            self.device_size_bytes / (1024 * 1024),
            self.max_total_bytes / (1024 * 1024),
            (self.max_total_bytes as f64 / self.total_ram_bytes as f64) * 100.0,
        );

        Ok(())
    }

    /// Check if zram needs growing. Call once per cycle.
    /// Adds a new device if current zram is >80% full and there's enough free RAM.
    pub fn maybe_grow(&mut self, tuning: &mut Tuning) {
        if self.devices.is_empty() {
            return;
        }

        let stats = match self.stats() {
            Some(s) => s,
            None => return,
        };

        let current_total = stats.total_capacity;

        // Don't grow past max.
        if current_total >= self.max_total_bytes {
            return;
        }

        // Only grow if >80% full.
        if stats.original_bytes < current_total * 80 / 100 {
            return;
        }

        // Only grow if there's enough free RAM (need at least device_size free).
        let free = free_ram_bytes();
        let needed = self.device_size_bytes / 2; // compressed data uses ~half the device size
        if free < needed + 1024 * 1024 * 1024 {
            // Keep at least 1GB free.
            debug!("zram: would grow but only {} MiB free RAM", free / (1024 * 1024));
            return;
        }

        info!(
            "zram: {}% full, adding {} MiB device (free RAM: {} MiB)",
            stats.original_bytes * 100 / current_total.max(1),
            self.device_size_bytes / (1024 * 1024),
            free / (1024 * 1024),
        );

        if let Err(e) = self.add_device(tuning) {
            warn!("Failed to add zram device: {e}");
        }
    }

    /// Get current zram statistics across all our devices.
    pub fn stats(&self) -> Option<ZramStats> {
        if self.devices.is_empty() {
            return None;
        }

        let mut total_capacity: u64 = 0;
        let mut original: u64 = 0;
        let mut compressed: u64 = 0;

        for &idx in &self.devices {
            let base = format!("/sys/block/zram{idx}");
            total_capacity += read_sysfs_u64(&format!("{base}/disksize")).unwrap_or(0);

            // mm_stat format: orig_data_size compr_data_size mem_used_total ...
            if let Some(mm_stat) = read_sysfs_string(&format!("{base}/mm_stat")) {
                let fields: Vec<&str> = mm_stat.split_whitespace().collect();
                if fields.len() >= 2 {
                    original += fields[0].parse::<u64>().unwrap_or(0);
                    compressed += fields[1].parse::<u64>().unwrap_or(0);
                }
            }
        }

        let saved = original.saturating_sub(compressed);
        let ratio = if compressed > 0 {
            original as f64 / compressed as f64
        } else {
            0.0
        };

        Some(ZramStats {
            total_capacity,
            original_bytes: original,
            compressed_bytes: compressed,
            saved_bytes: saved,
            ratio,
            device_count: self.devices.len(),
        })
    }

    /// Shrink zram if we have more capacity than needed. Call once per cycle.
    /// Removes the newest device if it's <20% utilised and we have 2+ devices.
    pub fn maybe_shrink(&mut self, tuning: &mut Tuning) {
        if self.devices.len() < 2 {
            return;
        }

        let stats = match self.stats() {
            Some(s) => s,
            None => return,
        };

        // Only shrink if total utilisation is below 40%.
        if stats.original_bytes > stats.total_capacity * 40 / 100 {
            return;
        }

        // Check if the last device is empty/near-empty by looking at overall usage.
        // If removing one device would still leave enough capacity, remove it.
        let capacity_after = stats.total_capacity - self.device_size_bytes;
        if stats.original_bytes > capacity_after * 80 / 100 {
            return; // would be too full after removal
        }

        let idx = match self.devices.last() {
            Some(&idx) => idx,
            None => return,
        };

        info!(
            "zram: utilisation {:.0}%, removing zram{idx} ({} MiB)",
            (stats.original_bytes as f64 / stats.total_capacity.max(1) as f64) * 100.0,
            self.device_size_bytes / (1024 * 1024),
        );

        let dev = format!("/dev/zram{idx}");
        // swapoff migrates pages to other swap devices.
        let result = std::process::Command::new("swapoff")
            .arg(&dev)
            .output();

        match result {
            Ok(o) if o.status.success() => {
                if fs::write("/sys/class/zram-control/hot_remove", idx.to_string()).is_ok() {
                    tuning.forget_zram(idx);
                }
                self.devices.pop();
                info!(
                    "  Removed — {} devices remaining, {} MiB capacity",
                    self.devices.len(),
                    self.devices.len() as u64 * self.device_size_bytes / (1024 * 1024),
                );
            }
            _ => {
                warn!("  Could not swapoff zram{idx} — pages still in use, keeping it");
            }
        }
    }

    /// Create a new zram device and enable it as swap.
    fn add_device(&mut self, tuning: &mut Tuning) -> Result<()> {
        // Ask the kernel for a new zram device.
        let idx_str = fs::read_to_string("/sys/class/zram-control/hot_add")
            .context("creating zram device — is the zram module loaded?")?;
        let idx: u32 = idx_str.trim().parse()
            .with_context(|| format!("parsing zram device index: {idx_str:?}"))?;
        // Record it before using it, so a crash at any later step still
        // leaves it on the list for --restore-tuning / the next start.
        tuning.record_zram(idx);

        let dev = format!("/dev/zram{idx}");
        let sys = format!("/sys/block/zram{idx}");

        // Set compression algorithm (lz4 is fast, lzo is fallback).
        let algo_path = format!("{sys}/comp_algorithm");
        if fs::write(&algo_path, "lz4").is_err() {
            fs::write(&algo_path, "lzo").ok();
        }

        // Set device size.
        fs::write(format!("{sys}/disksize"), self.device_size_bytes.to_string())
            .with_context(|| format!("setting zram{idx} disksize"))?;

        // mkswap + swapon.
        let mkswap = std::process::Command::new("mkswap")
            .arg(&dev)
            .output()
            .context("running mkswap on zram device")?;
        if !mkswap.status.success() {
            anyhow::bail!("mkswap {} failed", dev);
        }

        let swapon = std::process::Command::new("swapon")
            .arg("-p") // priority
            .arg("100") // higher than disk swap
            .arg(&dev)
            .output()
            .context("running swapon on zram device")?;
        if !swapon.status.success() {
            anyhow::bail!("swapon {} failed", dev);
        }

        self.devices.push(idx);
        info!(
            "zram{idx}: {} MiB device active (total: {} devices, {} MiB capacity)",
            self.device_size_bytes / (1024 * 1024),
            self.devices.len(),
            self.devices.len() as u64 * self.device_size_bytes / (1024 * 1024),
        );

        Ok(())
    }

    /// Clean up all zram devices (for shutdown).
    pub fn cleanup(&mut self, tuning: &mut Tuning) {
        for &idx in self.devices.iter().rev() {
            let dev = format!("/dev/zram{idx}");
            // swapoff first.
            let off = std::process::Command::new("swapoff").arg(&dev).output();
            if !matches!(&off, Ok(o) if o.status.success()) {
                warn!("swapoff {dev} failed — pages may still be in use");
            }
            // Remove the device.
            match fs::write("/sys/class/zram-control/hot_remove", idx.to_string()) {
                Ok(()) => {
                    tuning.forget_zram(idx);
                    debug!("Removed zram{idx}");
                }
                Err(e) => warn!("Could not remove zram{idx}: {e} (left recorded for --restore-tuning)"),
            }
        }
        self.devices.clear();
    }

    pub fn has_devices(&self) -> bool {
        !self.devices.is_empty()
    }
}

fn total_ram_bytes() -> u64 {
    if let Some(kb) = read_meminfo_kb("MemTotal") {
        kb * 1024
    } else {
        // Fallback: 4GB
        4 * 1024 * 1024 * 1024
    }
}

fn free_ram_bytes() -> u64 {
    if let Some(kb) = read_meminfo_kb("MemAvailable") {
        kb * 1024
    } else {
        0
    }
}

fn read_meminfo_kb(key: &str) -> Option<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    for line in meminfo.lines() {
        if line.starts_with(key) {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 2 {
                return parts[1].parse().ok();
            }
        }
    }
    None
}

fn read_sysfs_u64(path: &str) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_sysfs_string(path: &str) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// Check if zram is supported.
pub fn is_supported() -> bool {
    Path::new("/sys/class/zram-control").exists()
        || std::process::Command::new("modprobe")
            .arg("--dry-run")
            .arg("zram")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
}
