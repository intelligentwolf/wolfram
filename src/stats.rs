//! Stats tracker — writes current memory savings to /var/lib/wolfram/stats.json once per cycle.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::compressor;
use crate::hibernator::HibernationStats;

const PAGE_SIZE_BYTES: u64 = 4096;

#[derive(Serialize)]
pub struct Stats {
    /// ISO 8601 timestamp of when stats were written.
    pub timestamp: String,
    /// Seconds since wolfram started.
    pub uptime_secs: u64,
    /// Number of completed cycles.
    pub cycles: u64,

    // -- Compression (zswap) --
    /// Pages currently stored compressed in zswap.
    pub zswap_stored_pages: u64,
    /// What those pages would occupy uncompressed (bytes).
    pub zswap_original_bytes: u64,
    /// Actual compressed pool size in RAM (bytes).
    pub zswap_compressed_bytes: u64,
    /// RAM saved by zswap compression (bytes). original - compressed.
    pub zswap_saved_bytes: u64,
    /// Compression ratio (e.g., 2.8 means 2.8:1).
    pub zswap_ratio: f64,

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
        // Ensure directory exists (best-effort).
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
    ) {
        let uptime_secs = self.started.elapsed().as_secs();

        // Read zswap stats from kernel.
        let (zswap_stored, zswap_original, zswap_compressed, zswap_saved, zswap_ratio) =
            match compressor::zswap_stats() {
                Some(zs) => {
                    let original = zs.stored_pages * PAGE_SIZE_BYTES;
                    let compressed = zs.pool_total_size;
                    let saved = original.saturating_sub(compressed);
                    let ratio = if compressed > 0 {
                        original as f64 / compressed as f64
                    } else {
                        0.0
                    };
                    (zs.stored_pages, original, compressed, saved, ratio)
                }
                None => (0, 0, 0, 0, 0.0),
            };

        let zswap_saved_mib = zswap_saved / (1024 * 1024);
        let total_saved_mib = zswap_saved_mib + hibernation.saved_mib;

        let stats = Stats {
            timestamp: now_iso8601(),
            uptime_secs,
            cycles: cycle_count,
            zswap_stored_pages: zswap_stored,
            zswap_original_bytes: zswap_original,
            zswap_compressed_bytes: zswap_compressed,
            zswap_saved_bytes: zswap_saved,
            zswap_ratio,
            hibernated_processes: hibernation.hibernated,
            hibernated_saved_mib: hibernation.saved_mib,
            total_saved_mib,
            lifetime_compressed_kib,
        };

        match serde_json::to_string_pretty(&stats) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&self.path, &json) {
                    log::debug!("Could not write stats to {}: {e}", self.path.display());
                }
            }
            Err(e) => {
                log::debug!("Could not serialize stats: {e}");
            }
        }
    }
}

fn now_iso8601() -> String {
    // Read /proc/driver/rtc or use a simple approach with libc.
    let mut tv = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_REALTIME, &mut tv);
    }
    let secs = tv.tv_sec;

    // Convert to UTC broken-down time.
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
