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
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::compressor::{self, Action};
use crate::config::Config;
use crate::hibernator::Hibernator;
use crate::idle_tracker::{self, MarkedRegion, PageRun};
use crate::scanner::{self, ProcessInfo};
use crate::stats::StatsTracker;
use crate::tuning::Tuning;
use crate::zram::ZramManager;

/// Per-region state, keyed by (pid, region start).
#[derive(Default)]
struct RegionState {
    /// Consecutive cycles the region met the cold threshold.
    idle_cycles: u32,
    /// Pages last cycle's MADV_PAGEOUT actually moved to swap. Checked at the
    /// next mark: any of them resident again were refaulted.
    paged_out: Vec<PageRun>,
    paged_out_pages: u64,
    /// The region refaulted heavily; leave it alone until this cycle.
    backoff_until: u64,
}

struct RegionHistory {
    regions: HashMap<(u32, u64), RegionState>,
}

impl RegionHistory {
    fn new() -> Self {
        Self { regions: HashMap::new() }
    }

    fn entry(&mut self, pid: u32, start: u64) -> &mut RegionState {
        self.regions.entry((pid, start)).or_default()
    }

    /// Forget regions not seen this cycle (unmapped, or their process
    /// exited) — except a region still in back-off whose process is alive: a
    /// single missed scan (transient read failure, process briefly under the
    /// anon threshold) must not erase the back-off and get a hot region
    /// paged out again at once.
    fn retain_seen(&mut self, seen: &HashSet<(u32, u64)>, live_pids: &HashSet<u32>, cycle: u64) {
        self.regions.retain(|k, st| {
            seen.contains(k) || (st.backoff_until > cycle && live_pids.contains(&k.0))
        });
    }
}

/// Lifetime counters. "Paged out" counts only pages verified to be in swap
/// after MADV_PAGEOUT, not every byte we asked about; "refaulted" counts the
/// paged-out pages found back in RAM one cycle later.
#[derive(Default)]
pub struct Lifetime {
    pub paged_out_kib: u64,
    pub refaulted_kib: u64,
    pub cold_advised_kib: u64,
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
pub fn run(config: Config, shutdown: Arc<AtomicBool>, tuning: &mut Tuning) -> Result<()> {
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
    info!(
        "  refault back-off: {:.0}% of paged-out pages back within a cycle -> leave region alone {} cycles; skip GPU processes: {}",
        config.refault_backoff_threshold * 100.0,
        config.refault_backoff_cycles,
        config.skip_gpu_processes
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

    // Set up zram compressed swap (preferred) or fall back to zswap.
    let mut zram_mgr = ZramManager::new();
    if config.dry_run {
        info!("[dry-run] not creating zram devices or changing swappiness");
    } else if crate::zram::is_supported() {
        match zram_mgr.init(tuning) {
            Ok(()) => info!("Using zram for in-RAM compression (no disk)"),
            Err(e) => {
                warn!("Could not init zram: {e}");
                warn!("Falling back to zswap/swap");
            }
        }
    } else if compressor::zswap_enabled() {
        info!("zram not available — using zswap for compression");
    } else {
        warn!("Neither zram nor zswap available — MADV_PAGEOUT will have limited effect");
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
    let mut lifetime = Lifetime::default();
    let mut gpu_logged: HashSet<u32> = HashSet::new();
    let mut cycle_count: u64 = 0;

    let stats_tracker = StatsTracker::new(&config.state_dir());

    let min_anon_kib = config.min_process_anon_mib * 1024;
    let idle_sample = Duration::from_secs(config.idle_sample_secs);
    let cycle_interval = Duration::from_secs(config.cycle_secs);

    info!("Entering main loop — cycle interval: {}s", config.cycle_secs);

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
            &mut lifetime,
            &mut gpu_logged,
            cycle_count,
            &shutdown,
            &stats_tracker,
            &mut zram_mgr,
            tuning,
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

    // Graceful shutdown.
    if zram_mgr.has_devices() {
        info!("Shutting down — cleaning up zram devices...");
        zram_mgr.cleanup(tuning);
    }
    if hibernator.has_hibernated() {
        info!("Shutting down — restoring hibernated processes...");
        hibernator.restore_all();
    }

    info!(
        "Wolfram stopped. Lifetime: ~{} MiB paged out, ~{} MiB of it refaulted",
        lifetime.paged_out_kib / 1024,
        lifetime.refaulted_kib / 1024,
    );
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
    lifetime: &mut Lifetime,
    gpu_logged: &mut HashSet<u32>,
    cycle_count: u64,
    shutdown: &AtomicBool,
    stats_tracker: &StatsTracker,
    zram_mgr: &mut ZramManager,
    tuning: &mut Tuning,
) -> Result<()> {
    // Step 1: Scan for memory-heavy processes.
    info!("Cycle {cycle_count}: scanning processes...");
    let processes = scanner::scan_processes(min_anon_kib)?;

    if processes.is_empty() {
        info!("Cycle {cycle_count}: no processes above {} MiB threshold", config.min_process_anon_mib);
        region_history.retain_seen(&HashSet::new(), &HashSet::new(), cycle_count);
        let hstats = hibernator.stats();
        stats_tracker.write(cycle_count, lifetime, &hstats, zram_mgr);
        return Ok(());
    }

    info!(
        "Cycle {cycle_count}: {} processes above {} MiB anon",
        processes.len(),
        config.min_process_anon_mib,
    );

    let active_pids: Vec<u32> = processes.iter().map(|p| p.pid).collect();
    process_idle.prune(&active_pids);
    gpu_logged.retain(|pid| active_pids.contains(pid));

    // Filter to eligible processes.
    let eligible: Vec<&ProcessInfo> = processes
        .iter()
        .filter(|p| !is_excluded(p, config))
        .filter(|p| {
            if config.skip_gpu_processes && scanner::holds_gpu_device(p.pid) {
                if gpu_logged.insert(p.pid) {
                    info!(
                        "  Skipping {} (PID {}): holds a GPU/accelerator device open",
                        p.name, p.pid
                    );
                }
                false
            } else {
                true
            }
        })
        .collect();

    let mut seen: HashSet<(u32, u64)> = HashSet::new();
    let mut cycle = Lifetime::default();
    let mut cycle_actions: u64 = 0;
    let mut gone: HashSet<u32> = HashSet::new();

    // Track per-process: were ALL regions idle?
    let mut process_all_idle: HashMap<u32, bool> = HashMap::new();

    if use_idle_tracking {
        // Step 2: Mark all regions idle across all processes, and check
        // what last cycle's page-outs did.
        let mut all_marked: Vec<(u32, MarkedRegion)> = Vec::new();
        info!("  Scanning {} eligible processes...", eligible.len());
        for proc_info in &eligible {
            if shutdown.load(Ordering::Relaxed) {
                return Ok(());
            }

            let regions =
                match scanner::scan_memory_regions(proc_info.pid, config.min_region_kib) {
                    Ok(r) => r,
                    Err(_) => continue, // process vanished
                };

            for region in &regions {
                let marked = match idle_tracker::mark_idle(proc_info.pid, region) {
                    Ok(m) => m,
                    Err(e) => {
                        debug!("  mark_idle failed for PID {} region 0x{:x}: {e}", proc_info.pid, region.start);
                        continue;
                    }
                };
                seen.insert((proc_info.pid, region.start));
                let st = region_history.entry(proc_info.pid, region.start);
                if !st.paged_out.is_empty() {
                    let refaulted = idle_tracker::present_pages_in(&marked, &st.paged_out);
                    cycle.refaulted_kib += idle_tracker::pages_to_kib(refaulted);
                    let frac = refaulted as f64 / st.paged_out_pages.max(1) as f64;
                    if refaulted > 0 && frac >= config.refault_backoff_threshold {
                        st.backoff_until = cycle_count + u64::from(config.refault_backoff_cycles);
                        st.idle_cycles = 0;
                        info!(
                            "  {} (PID {}) region 0x{:x}: {:.0}% of the {} KiB paged out last cycle came straight back — leaving it alone for {} cycles",
                            proc_info.name,
                            proc_info.pid,
                            region.start,
                            frac * 100.0,
                            idle_tracker::pages_to_kib(st.paged_out_pages),
                            config.refault_backoff_cycles,
                        );
                    }
                    st.paged_out.clear();
                    st.paged_out_pages = 0;
                }
                all_marked.push((proc_info.pid, marked));
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

        // Step 4-5: Check idle and apply actions to the idle pages only.
        for (pid, marked) in &all_marked {
            if gone.contains(pid) {
                continue;
            }
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

            let st = region_history.entry(*pid, region.start);
            if cycle_count < st.backoff_until {
                continue;
            }

            let action = if frac >= config.cold_threshold {
                st.idle_cycles += 1;
                if frac >= config.pageout_threshold
                    && st.idle_cycles >= config.pageout_after_cycles
                {
                    Action::PageOut
                } else {
                    Action::Cold
                }
            } else {
                st.idle_cycles = 0;
                continue;
            };

            let idle_kib = idle_tracker::pages_to_kib(idle_result.idle_pages);
            debug!(
                "  PID {} region 0x{:x} ({} KiB resident, {:.0}% idle, {} idle runs) -> {:?}",
                pid, region.start, region.rss_kib, frac * 100.0, idle_result.idle_runs.len(), action,
            );

            if config.dry_run {
                debug!("  [dry-run] would apply {action:?} to {idle_kib} KiB");
                continue;
            }

            let outcome = match compressor::advise_runs(*pid, &idle_result.idle_runs, action) {
                Ok(o) => o,
                Err(e) => {
                    warn!("  {e:#}");
                    continue;
                }
            };
            if outcome.gone {
                gone.insert(*pid);
                continue;
            }
            if outcome.bytes == 0 {
                continue;
            }
            cycle_actions += 1;
            match action {
                Action::Cold => cycle.cold_advised_kib += outcome.bytes / 1024,
                Action::PageOut => {
                    let swapped = idle_tracker::swapped_runs(*pid, &idle_result.idle_runs)
                        .unwrap_or_default();
                    let pages = idle_tracker::total_pages(&swapped);
                    cycle.paged_out_kib += idle_tracker::pages_to_kib(pages);
                    st.paged_out = swapped;
                    st.paged_out_pages = pages;
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
                seen.insert((proc_info.pid, region.start));
                let st = region_history.entry(proc_info.pid, region.start);
                st.idle_cycles += 1;
                if st.idle_cycles < config.pageout_after_cycles + 2 {
                    continue;
                }
                if config.dry_run {
                    debug!("  [dry-run] would apply Cold to PID {} region 0x{:x}", proc_info.pid, region.start);
                    continue;
                }
                let whole = [PageRun {
                    start_vpage: region.start / 4096,
                    pages: (region.end - region.start) / 4096,
                }];
                match compressor::advise_runs(proc_info.pid, &whole, Action::Cold) {
                    Ok(o) if o.gone => break,
                    Ok(o) if o.bytes > 0 => {
                        cycle.cold_advised_kib += o.bytes / 1024;
                        cycle_actions += 1;
                    }
                    Ok(_) => {}
                    Err(e) => warn!("  {e:#}"),
                }
            }
        }
    }

    let live_pids: HashSet<u32> = active_pids.iter().copied().collect();
    region_history.retain_seen(&seen, &live_pids, cycle_count);

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

    lifetime.paged_out_kib += cycle.paged_out_kib;
    lifetime.refaulted_kib += cycle.refaulted_kib;
    lifetime.cold_advised_kib += cycle.cold_advised_kib;

    if cycle_actions > 0 || cycle.refaulted_kib > 0 {
        info!(
            "Cycle {cycle_count}: {cycle_actions} regions advised — paged out ~{} MiB, marked cold ~{} MiB, \
             refaulted since last cycle ~{} MiB; lifetime paged out ~{} MiB (refaulted ~{} MiB)",
            cycle.paged_out_kib / 1024,
            cycle.cold_advised_kib / 1024,
            cycle.refaulted_kib / 1024,
            lifetime.paged_out_kib / 1024,
            lifetime.refaulted_kib / 1024,
        );
    } else {
        debug!("Cycle {cycle_count}: no actions needed");
    }

    // Dynamic zram management — grow or shrink based on usage.
    zram_mgr.maybe_grow(tuning);
    zram_mgr.maybe_shrink(tuning);

    // Write stats file and log summary.
    let hstats = hibernator.stats();
    stats_tracker.write(cycle_count, lifetime, &hstats, zram_mgr);

    // Log compression stats from zram (preferred) or zswap (fallback).
    if let Some(zs) = zram_mgr.stats() {
        let saved_mib = zs.saved_bytes / (1024 * 1024);
        let original_mib = zs.original_bytes / (1024 * 1024);
        let compressed_mib = zs.compressed_bytes / (1024 * 1024);
        info!(
            "  Saving: {} MiB (zram: {} MiB -> {} MiB, ratio {:.1}:1, {} devices, hibernated: {} / {} MiB)",
            saved_mib + hstats.saved_mib,
            original_mib,
            compressed_mib,
            zs.ratio,
            zs.device_count,
            hstats.hibernated,
            hstats.saved_mib,
        );
    } else if let Some(zs) = compressor::zswap_stats() {
        let original_mib = (zs.stored_pages * 4096) / (1024 * 1024);
        let pool_mib = zs.pool_total_size / (1024 * 1024);
        let saved_mib = original_mib.saturating_sub(pool_mib);
        if saved_mib > 0 || hstats.hibernated > 0 {
            info!(
                "  Saving: {} MiB (zswap: {} MiB -> {} MiB, hibernated: {} / {} MiB)",
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
