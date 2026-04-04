mod compressor;
mod config;
mod daemon;
mod hibernator;
mod idle_tracker;
mod scanner;
mod stats;
mod zram;

use log::{error, info};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn main() {
    // Parse CLI args once and get both config and log level.
    let (cfg, log_level) = match config::load() {
        Ok(result) => result,
        Err(e) => {
            eprintln!("Failed to load config: {e:#}");
            std::process::exit(1);
        }
    };

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(&log_level)).init();

    info!(
        "Wolfram v{} — proactive memory compression daemon",
        env!("CARGO_PKG_VERSION")
    );

    // Check we're running as root.
    if !nix::unistd::geteuid().is_root() {
        error!("Wolfram needs root privileges (or CAP_SYS_ADMIN + CAP_SYS_NICE)");
        error!("Run with: sudo wolfram");
        std::process::exit(1);
    }

    if cfg.dry_run {
        info!("DRY RUN mode — will scan and report but not modify anything");
    }

    print_system_info();
    set_low_priority();
    tune_kernel();

    // Set up signal handler for graceful shutdown.
    let shutdown = Arc::new(AtomicBool::new(false));
    setup_signal_handlers(&shutdown);

    // Run the daemon.
    if let Err(e) = daemon::run(cfg, shutdown) {
        error!("Daemon error: {e:#}");
        std::process::exit(1);
    }
}

fn setup_signal_handlers(shutdown: &Arc<AtomicBool>) {
    // Store the shutdown flag in a global so signal handlers can access it.
    // We leak the Arc intentionally — it must live for the entire process.
    let ptr = Arc::into_raw(shutdown.clone()) as *mut AtomicBool;
    SHUTDOWN_FLAG.store(ptr, Ordering::SeqCst);

    unsafe {
        let action = nix::sys::signal::SigAction::new(
            nix::sys::signal::SigHandler::Handler(signal_handler),
            nix::sys::signal::SaFlags::empty(),
            nix::sys::signal::SigSet::empty(),
        );
        // These may fail in unusual environments; non-fatal.
        let _ = nix::sys::signal::sigaction(nix::sys::signal::Signal::SIGINT, &action);
        let _ = nix::sys::signal::sigaction(nix::sys::signal::Signal::SIGTERM, &action);
    }
}

static SHUTDOWN_FLAG: std::sync::atomic::AtomicPtr<AtomicBool> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

extern "C" fn signal_handler(_sig: libc::c_int) {
    let ptr = SHUTDOWN_FLAG.load(Ordering::SeqCst);
    if !ptr.is_null() {
        // Safety: ptr was created from Arc::into_raw and is valid for the process lifetime.
        unsafe { &*ptr }.store(true, Ordering::SeqCst);
    }
}

fn print_system_info() {
    if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
        for line in meminfo.lines() {
            if line.starts_with("MemTotal:") || line.starts_with("MemAvailable:") {
                info!("  {}", line.trim());
            }
        }
    }

    if let Ok(version) = std::fs::read_to_string("/proc/version") {
        let short = version
            .split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join(" ");
        info!("  Kernel: {short}");
    }

    info!(
        "  Idle page tracking: {}",
        if idle_tracker::is_supported() {
            "available"
        } else {
            "NOT available (need CONFIG_IDLE_PAGE_TRACKING)"
        }
    );
    info!(
        "  process_madvise: {}",
        if compressor::is_supported() {
            "available"
        } else {
            "NOT available (need Linux 5.10+)"
        }
    );
    info!(
        "  zswap: {}",
        if compressor::zswap_enabled() {
            "enabled"
        } else {
            "disabled"
        }
    );
    info!(
        "  CRIU: {}",
        if hibernator::is_supported() {
            "available"
        } else {
            "not found"
        }
    );
}


/// Set wolfram to lowest CPU and IO priority so it never starves real workloads.
fn set_low_priority() {
    // nice 19 — lowest CPU scheduling priority.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
    }

    // SCHED_IDLE — kernel only schedules us when nothing else wants the CPU.
    unsafe {
        let mut param: libc::sched_param = std::mem::zeroed();
        param.sched_priority = 0;
        libc::sched_setscheduler(0, libc::SCHED_IDLE, &param);
    }

    // ionice class 3 (idle) — only do IO when disk is idle.
    // ioprio_set(IOPRIO_WHO_PROCESS, 0, IOPRIO_CLASS_IDLE << 13)
    unsafe {
        libc::syscall(251 as libc::c_long, 1 as libc::c_int, 0 as libc::c_int, (3 << 13) as libc::c_int);
    }

    info!("  Priority: nice 19, SCHED_IDLE, ionice idle");
}

/// Tune kernel parameters for optimal memory compression.
/// Each sysctl is applied independently — if the file doesn't exist
/// on a given kernel/distro, it's silently skipped. We never fail
/// startup over a missing sysctl.
fn tune_kernel() {
    let tunings: &[(&str, &str, &str)] = &[
        // Reclaim stale filesystem cache faster. Default 100.
        // Higher = kernel drops cached files sooner, freeing pages for zram.
        (
            "/proc/sys/vm/vfs_cache_pressure",
            "200",
            "vfs_cache_pressure=200 (reclaim stale file cache faster)",
        ),
        // Stop proactive memory compaction. Default 20.
        // Compaction wastes CPU building huge pages from fragments.
        // With zram compression we don't need huge pages.
        (
            "/proc/sys/vm/compaction_proactiveness",
            "0",
            "compaction_proactiveness=0 (stop wasting CPU on defrag)",
        ),
        // Disable watermark boost. Default 15000.
        // The kernel over-reclaims by 150% "just in case" after a reclaim event.
        // With zram available, this just wastes effort.
        (
            "/proc/sys/vm/watermark_boost_factor",
            "0",
            "watermark_boost_factor=0 (no over-reclaim)",
        ),
    ];

    let mut applied = Vec::new();

    for &(path, value, description) in tunings {
        let sysctl = std::path::Path::new(path);
        if !sysctl.exists() {
            continue;
        }

        // Don't overwrite if already set to our value.
        if let Ok(current) = std::fs::read_to_string(sysctl) {
            if current.trim() == value {
                continue;
            }
        }

        match std::fs::write(sysctl, value) {
            Ok(()) => applied.push(description),
            Err(e) => {
                log::debug!("Could not set {path}: {e}");
            }
        }
    }

    if !applied.is_empty() {
        info!("  Kernel tuning:");
        for desc in &applied {
            info!("    {desc}");
        }
    }

    // Enable KSM (Kernel Same-page Merging) if available but not running.
    // KSM deduplicates identical pages across all processes — huge savings
    // when running many instances of the same runtime (.NET, Java, Node).
    let ksm_run = std::path::Path::new("/sys/kernel/mm/ksm/run");
    if ksm_run.exists() {
        let current = std::fs::read_to_string(ksm_run).unwrap_or_default();
        if current.trim() == "0" {
            match std::fs::write(ksm_run, "1") {
                Ok(()) => {
                    // Tune KSM scan speed — default is conservative (100 pages per 20ms).
                    // Increase to 1000 pages to find duplicates faster.
                    std::fs::write("/sys/kernel/mm/ksm/pages_to_scan", "1000").ok();
                    info!("  KSM: enabled (deduplicates identical pages across processes)");
                }
                Err(e) => {
                    log::debug!("Could not enable KSM: {e}");
                }
            }
        } else {
            let sharing = std::fs::read_to_string("/sys/kernel/mm/ksm/pages_sharing")
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
            if sharing > 0 {
                let saved_mib = sharing * 4 / 1024; // pages * 4KB / 1024
                info!("  KSM: already running ({} MiB deduplicated)", saved_mib);
            } else {
                info!("  KSM: already running");
            }
        }
    }
}
