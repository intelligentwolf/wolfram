//! Stats tracker — writes current memory savings to /var/lib/wolfram/stats.json once per cycle.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::compressor;
use crate::hibernator::HibernationStats;
use crate::zram::ZramManager;

const PAGE_SIZE_BYTES: u64 = 4096;

#[derive(Serialize)]
pub struct Stats {
    /// ISO 8601 timestamp of when stats were written.
    pub timestamp: String,
    /// Seconds since wolfram started.
    pub uptime_secs: u64,
    /// Number of completed cycles.
    pub cycles: u64,

    // -- Compression --
    /// Backend: "zram" or "zswap" or "none".
    pub compression_backend: String,
    /// Data stored before compression (bytes).
    pub original_bytes: u64,
    /// Actual compressed size in RAM (bytes).
    pub compressed_bytes: u64,
    /// RAM saved by compression (bytes).
    pub saved_bytes: u64,
    /// Compression ratio (e.g., 2.8 means 2.8:1).
    pub compression_ratio: f64,
    /// Number of zram devices (0 if using zswap).
    pub zram_devices: usize,

    // -- Hibernation (CRIU) --
    /// Number of processes currently hibernated.
    pub hibernated_processes: usize,
    /// RAM freed by hibernation (MiB).
    pub hibernated_saved_mib: u64,

    // -- Totals --
    /// Total RAM currently being saved (MiB) — compression + hibernation.
    pub total_saved_mib: u64,
    /// Total KiB pushed to compression over lifetime.
    pub lifetime_compressed_kib: u64,
}

pub struct StatsTracker {
    path: PathBuf,
    started: Instant,
}

impl StatsTracker {
    pub fn new(stats_dir: &Path) -> Self {
        std::fs::create_dir_all(stats_dir).ok();
        Self {
            path: stats_dir.join("stats.json"),
            started: Instant::now(),
        }
    }

    /// Write current stats to disk. Called once per daemon cycle.
    pub fn write(
        &self,
        cycle_count: u64,
        lifetime_compressed_kib: u64,
        hibernation: &HibernationStats,
        zram_mgr: &ZramManager,
    ) {
        let uptime_secs = self.started.elapsed().as_secs();

        // Try zram stats first (preferred), fall back to zswap.
        let (backend, original, compressed, saved, ratio, devices) =
            if let Some(zs) = zram_mgr.stats() {
                (
                    "zram".to_string(),
                    zs.original_bytes,
                    zs.compressed_bytes,
                    zs.saved_bytes,
                    zs.ratio,
                    zs.device_count,
                )
            } else if let Some(zs) = compressor::zswap_stats() {
                let orig = zs.stored_pages * PAGE_SIZE_BYTES;
                let comp = zs.pool_total_size;
                let sav = orig.saturating_sub(comp);
                let rat = if comp > 0 { orig as f64 / comp as f64 } else { 0.0 };
                ("zswap".to_string(), orig, comp, sav, rat, 0)
            } else {
                ("none".to_string(), 0, 0, 0, 0.0, 0)
            };

        let saved_mib = saved / (1024 * 1024);
        let total_saved_mib = saved_mib + hibernation.saved_mib;

        let stats = Stats {
            timestamp: now_iso8601(),
            uptime_secs,
            cycles: cycle_count,
            compression_backend: backend,
            original_bytes: original,
            compressed_bytes: compressed,
            saved_bytes: saved,
            compression_ratio: ratio,
            zram_devices: devices,
            hibernated_processes: hibernation.hibernated,
            hibernated_saved_mib: hibernation.saved_mib,
            total_saved_mib,
            lifetime_compressed_kib,
        };

        match serde_json::to_string_pretty(&stats) {
            Ok(json) => {
                let tmp = self.path.with_extension("tmp");
                if let Err(e) = std::fs::write(&tmp, &json) {
                    log::debug!("Could not write stats to {}: {e}", tmp.display());
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp, &self.path) {
                    log::debug!("Could not rename stats file: {e}");
                }
            }
            Err(e) => {
                log::debug!("Could not serialize stats: {e}");
            }
        }
    }
}

fn now_iso8601() -> String {
    let mut tv = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_REALTIME, &mut tv);
    }
    let secs = tv.tv_sec;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::gmtime_r(&secs, &mut tm);
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
}
