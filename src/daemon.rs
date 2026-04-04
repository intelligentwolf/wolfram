//! Daemon loop — ties scanner, idle tracker, and compressor together.
//!
//! Each cycle:
//! 1. Scan processes for those with significant anonymous memory
//! 2. For each process, scan memory regions and mark all pages idle (fast)
//! 3. Wait ONCE for the sample interval
//! 4. Check all marked pages to see which are still idle (fast)
//! 5. Apply MADV_COLD or MADV_PAGEOUT based on idle fraction and history
//! 6. Log stats and repeat

use anyhow::Result;
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::compressor::{self, Action};
use crate::config::Config;
use crate::hibernator::Hibernator;
use crate::idle_tracker::{self, MarkedRegion};
use crate::scanner::{self, ProcessInfo};
use crate::stats::StatsTracker;

/// Tracks how many consecutive cycles a region has been idle.
struct RegionHistory {
    idle_cycles: HashMap<(u32, u64), u32>,
}

impl RegionHistory {
    fn new() -> Self {
        Self {
            idle_cycles: HashMap::new(),
        }
    }

    fn record_idle(&mut self, pid: u32, start: u64) -> u32 {
        let count = self.idle_cycles.entry((pid, start)).or_insert(0);
        *count += 1;
        *count
    }

    fn record_active(&mut self, pid: u32, start: u64) {
        self.idle_cycles.remove(&(pid, start));
    }

    fn prune(&mut self, active_pids: &[u32]) {
        self.idle_cycles
            .retain(|&(pid, _), _| active_pids.contains(&pid));
    }
}

/// Tracks how many consecutive cycles a process has had all regions idle.
struct ProcessIdleTracker {
    idle_cycles: HashMap<u32, u32>,
}

impl ProcessIdleTracker {
    fn new() -> Self {
        Self {
            idle_cycles: HashMap::new(),
        }
    }

    fn record_idle(&mut self, pid: u32) -> u32 {
        let count = self.idle_cycles.entry(pid).or_insert(0);
        *count += 1;
        *count
    }

    fn record_active(&mut self, pid: u32) {
        self.idle_cycles.remove(&pid);
    }

    fn prune(&mut self, active_pids: &[u32]) {
        self.idle_cycles
            .retain(|pid, _| active_pids.contains(pid));
    }
}

/// Run the daemon. Loops until `shutdown` flag is set.
pub fn run(config: Config, shutdown: Arc<AtomicBool>) -> Result<()> {
    info!("Wolfram memory compression daemon starting");
    info!(
        "  min process anon: {} MiB, min region: {} KiB",
        config.min_process_anon_mib, config.min_region_kib
    );
    info!(
        "  idle sample: {}s, cycle: {}s",
        config.idle_sample_secs, config.cycle_secs
    );
    info!(
        "  cold threshold: {:.0}%, pageout threshold: {:.0}%, pageout after {} cycles",
        config.cold_threshold * 100.0,
        config.pageout_threshold * 100.0,
        config.pageout_after_cycles
    );

    let use_idle_tracking = idle_tracker::is_supported();
    if !use_idle_tracking {
        warn!("Idle page tracking not available (need CONFIG_IDLE_PAGE_TRACKING)");
        warn!("Will use process_madvise based on region age only");
    }

    if !compressor::is_supported() {
        error!("process_madvise not supported on this kernel (need Linux 5.10+)");
        anyhow::bail!("process_madvise not available");
    }

    if compressor::zswap_enabled() {
        info!("zswap is ENABLED — compressed pages stay in RAM");
    } else {
        warn!("zswap is NOT enabled — pages will go to disk swap (slower)");
        warn!("Consider: echo Y > /sys/module/zswap/parameters/enabled");
    }

    let mut hibernator = Hibernator::new(&config);
    let use_hibernation = config.enable_hibernation && crate::hibernator::is_supported();
    if use_hibernation {
        info!(
            "CRIU hibernation ENABLED — processes idle for {} cycles will be checkpointed",
            config.hibernate_after_cycles
        );
    } else if config.enable_hibernation {
        warn!("Hibernation requested but CRIU not found in PATH");
    }

    let mut region_history = RegionHistory::new();
    let mut process_idle = ProcessIdleTracker::new();
    let mut total_compressed_kib: u64 = 0;
    let mut cycle_count: u64 = 0;

    let stats_tracker = StatsTracker::new(config.checkpoint_dir.parent().unwrap_or(std::path::Path::new("/var/lib/wolfram")));

    let min_anon_kib = config.min_process_anon_mib * 1024;
    let idle_sample = Duration::from_secs(config.idle_sample_secs);
    let cycle_interval = Duration::from_secs(config.cycle_secs);

    while !shutdown.load(Ordering::Relaxed) {
        cycle_count += 1;
        let cycle_start = Instant::now();

        match run_cycle(
            &config,
            min_anon_kib,
            use_idle_tracking,
            use_hibernation,
            idle_sample,
            &mut region_history,
            &mut process_idle,
            &mut hibernator,
            &mut total_compressed_kib,
            cycle_count,
            &shutdown,
            &stats_tracker,
        ) {
            Ok(()) => {}
            Err(e) => {
                error!("Cycle {cycle_count} failed: {e:#}");
            }
        }

        // Sleep until next cycle, checking shutdown flag periodically.
        let elapsed = cycle_start.elapsed();
        if elapsed < cycle_interval {
            let remaining = cycle_interval - elapsed;
            sleep_interruptible(remaining, &shutdown);
        }
    }

    // Graceful shutdown: always restore any hibernated processes, even if
    // use_hibernation was toggled off after some processes were checkpointed.
    if hibernator.has_hibernated() {
        info!("Shutting down — restoring hibernated processes...");
        hibernator.restore_all();
    }

    info!("Wolfram stopped. Total lifetime compression: ~{} MiB", total_compressed_kib / 1024);
    Ok(())
}

fn sleep_interruptible(duration: Duration, shutdown: &AtomicBool) {
    let start = Instant::now();
    let check_interval = Duration::from_secs(1);
    while start.elapsed() < duration {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        let remaining = duration.saturating_sub(start.elapsed());
        std::thread::sleep(remaining.min(check_interval));
    }
}

#[allow(clippy::too_many_arguments)]
fn run_cycle(
    config: &Config,
    min_anon_kib: u64,
    use_idle_tracking: bool,
    use_hibernation: bool,
    idle_sample: Duration,
    region_history: &mut RegionHistory,
    process_idle: &mut ProcessIdleTracker,
    hibernator: &mut Hibernator,
    total_compressed_kib: &mut u64,
    cycle_count: u64,
    shutdown: &AtomicBool,
    stats_tracker: &StatsTracker,
) -> Result<()> {
    // Step 1: Scan for memory-heavy processes.
    let processes = scanner::scan_processes(min_anon_kib)?;

    if processes.is_empty() {
        debug!("Cycle {cycle_count}: no processes above threshold");
        return Ok(());
    }

    info!(
        "Cycle {cycle_count}: {} processes above {} MiB anon",
        processes.len(),
        config.min_process_anon_mib,
    );

    let active_pids: Vec<u32> = processes.iter().map(|p| p.pid).collect();
    region_history.prune(&active_pids);
    process_idle.prune(&active_pids);

    // Filter to eligible processes.
    let eligible: Vec<&ProcessInfo> = processes
        .iter()
        .filter(|p| !is_excluded(p, config))
        .collect();

    // Step 2: Mark all regions idle across all processes (fast — no sleeping).
    let mut all_marked: Vec<(u32, MarkedRegion)> = Vec::new();

    if use_idle_tracking {
        for proc_info in &eligible {
            let regions =
                match scanner::scan_memory_regions(proc_info.pid, config.min_region_kib) {
                    Ok(r) => r,
                    Err(_) => continue, // process vanished
                };

            for region in &regions {
                match idle_tracker::mark_idle(proc_info.pid, region) {
                    Ok(marked) => all_marked.push((proc_info.pid, marked)),
                    Err(e) => {
                        debug!("  mark_idle failed for PID {} region 0x{:x}: {e}", proc_info.pid, region.start);
                    }
                }
            }
        }

        debug!(
            "  Marked {} regions across {} processes, waiting {}s...",
            all_marked.len(),
            eligible.len(),
            idle_sample.as_secs(),
        );

        // Step 3: Wait ONCE for the sample interval.
        sleep_interruptible(idle_sample, shutdown);
        if shutdown.load(Ordering::Relaxed) {
            return Ok(());
        }
    }

    // Step 4-5: Check idle and apply actions.
    let mut cycle_compressed_kib: u64 = 0;
    let mut cycle_actions: u64 = 0;

    // Track per-process: were ALL regions idle?
    let mut process_all_idle: HashMap<u32, bool> = HashMap::new();

    if use_idle_tracking {
        for (pid, marked) in &all_marked {
            let idle_result = match idle_tracker::check_idle(marked) {
                Ok(r) => r,
                Err(e) => {
                    debug!("  check_idle failed for PID {pid}: {e}");
                    continue;
                }
            };

            let frac = idle_result.idle_fraction();
            let region = &idle_result.region;

            // Track whether this process has any active regions.
            let all_idle = process_all_idle.entry(*pid).or_insert(true);
            if frac < config.cold_threshold {
                *all_idle = false;
            }

            let action = if frac >= config.cold_threshold {
                let consecutive = region_history.record_idle(*pid, region.start);
                if frac >= config.pageout_threshold
                    && consecutive >= config.pageout_after_cycles
                {
                    Some(Action::PageOut)
                } else {
                    Some(Action::Cold)
                }
            } else {
                region_history.record_active(*pid, region.start);
                None
            };

            if let Some(action) = action {
                debug!(
                    "  PID {} region 0x{:x} ({} KiB, {:.0}% idle) -> {:?}",
                    pid, region.start, region.rss_kib, frac * 100.0, action,
                );

                match compressor::advise_region(*pid, region, action) {
                    Ok(result) if result.success => {
                        cycle_compressed_kib += region.rss_kib;
                        cycle_actions += 1;
                    }
                    _ => {}
                }
            }
        }
    } else {
        // No idle tracking — every region gets recorded as idle (conservative: only COLD).
        // Without idle tracking we cannot determine true idleness, so do NOT feed the
        // process-level hibernation tracker (process_all_idle stays empty for these PIDs).
        for proc_info in &eligible {
            let regions =
                match scanner::scan_memory_regions(proc_info.pid, config.min_region_kib) {
                    Ok(r) => r,
                    Err(_) => continue,
                };

            for region in &regions {
                let consecutive = region_history.record_idle(proc_info.pid, region.start);
                if consecutive >= config.pageout_after_cycles + 2 {
                    match compressor::advise_region(proc_info.pid, region, Action::Cold) {
                        Ok(result) if result.success => {
                            cycle_compressed_kib += region.rss_kib;
                            cycle_actions += 1;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // Check port proxies for incoming connections (wake-on-connect).
    if use_hibernation {
        hibernator.check_port_proxies();
    }

    // Step 6: Process-level hibernation decisions.
    if use_hibernation {
        for proc_info in &eligible {
            let all_idle = process_all_idle.get(&proc_info.pid).copied().unwrap_or(false);
            if all_idle {
                let consecutive = process_idle.record_idle(proc_info.pid);
                if consecutive >= config.hibernate_after_cycles {
                    info!(
                        "  Hibernating {} (PID {}) — idle for {consecutive} cycles, ~{} MiB",
                        proc_info.name, proc_info.pid, proc_info.anon_kib / 1024,
                    );
                    match hibernator.checkpoint(proc_info) {
                        Ok(()) => {
                            info!("    Checkpointed successfully");
                        }
                        Err(e) => {
                            warn!("    Checkpoint failed: {e}");
                        }
                    }
                }
            } else {
                process_idle.record_active(proc_info.pid);
            }
        }
    }

    *total_compressed_kib += cycle_compressed_kib;

    if cycle_actions > 0 {
        info!(
            "Cycle {cycle_count}: {cycle_actions} regions compressed, \
             ~{} MiB this cycle, ~{} MiB lifetime",
            cycle_compressed_kib / 1024,
            *total_compressed_kib / 1024,
        );
    } else {
        debug!("Cycle {cycle_count}: no actions needed");
    }

    // Write stats file and log summary.
    let hstats = hibernator.stats();
    stats_tracker.write(cycle_count, *total_compressed_kib, &hstats);

    if let Some(zs) = compressor::zswap_stats() {
        let original_mib = (zs.stored_pages * 4096) / (1024 * 1024);
        let pool_mib = zs.pool_total_size / (1024 * 1024);
        let saved_mib = original_mib.saturating_sub(pool_mib);
        if saved_mib > 0 || hstats.hibernated > 0 {
            info!(
                "  Saving: {} MiB (zswap: {} MiB compressed to {} MiB, hibernated: {} processes / {} MiB)",
                saved_mib + hstats.saved_mib,
                original_mib,
                pool_mib,
                hstats.hibernated,
                hstats.saved_mib,
            );
        }
    } else if hstats.hibernated > 0 {
        info!(
            "  Saving: {} MiB (hibernated: {} processes)",
            hstats.saved_mib, hstats.hibernated,
        );
    }

    Ok(())
}

fn is_excluded(proc_info: &ProcessInfo, config: &Config) -> bool {
    if config.excluded_pids.contains(&proc_info.pid) {
        return true;
    }
    config
        .excluded_names
        .iter()
        .any(|n| proc_info.name.contains(n.as_str()))
}
