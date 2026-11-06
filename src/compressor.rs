//! Compression engine — uses process_madvise() to push cold pages into zswap.
//!
//! process_madvise() (Linux 5.10+) lets us advise the kernel about another
//! process's memory without that process knowing:
//!
//! - MADV_COLD:    "deprioritize these pages" — kernel compresses them when convenient
//! - MADV_PAGEOUT: "reclaim these pages NOW" — forces them into zswap/swap immediately
//!
//! Requires CAP_SYS_NICE or same-uid as the target process.

use anyhow::Result;
use std::fs;
use std::path::Path;

use crate::idle_tracker::PageRun;

// Syscall numbers vary by architecture.
// Sources: arch/x86/entry/syscalls/syscall_64.tbl,
//          arch/arm/tools/syscall.tbl,
//          include/uapi/asm-generic/unistd.h

#[cfg(target_arch = "x86_64")]
const SYS_PROCESS_MADVISE: libc::c_long = 440;
#[cfg(target_arch = "x86_64")]
const SYS_PIDFD_OPEN: libc::c_long = 434;

#[cfg(target_arch = "aarch64")]
const SYS_PROCESS_MADVISE: libc::c_long = 440;
#[cfg(target_arch = "aarch64")]
const SYS_PIDFD_OPEN: libc::c_long = 434;

// ARM (32-bit): from arch/arm/tools/syscall.tbl
#[cfg(target_arch = "arm")]
const SYS_PROCESS_MADVISE: libc::c_long = 440;
#[cfg(target_arch = "arm")]
const SYS_PIDFD_OPEN: libc::c_long = 434;

const MADV_COLD: i32 = 20;
const MADV_PAGEOUT: i32 = 21;

/// Compression action to take on a region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Deprioritize — kernel will compress/reclaim when it feels like it.
    Cold,
    /// Force reclaim NOW — pages go straight into zswap/swap.
    PageOut,
}

/// What one `advise_runs` call achieved.
#[derive(Debug, Default)]
pub struct AdviseOutcome {
    /// Bytes the kernel accepted the advice for.
    pub bytes: u64,
    /// The target exited (ESRCH) — stop working on it this cycle.
    pub gone: bool,
}

/// process_madvise() takes at most IOV_MAX iovecs per call
/// (process_madvise(2)); Linux defines it as UIO_MAXIOV = 1024
/// (include/uapi/linux/uio.h:46), used if sysconf cannot answer.
fn iov_max() -> usize {
    let v = unsafe { libc::sysconf(libc::_SC_IOV_MAX) };
    if v > 0 { v as usize } else { 1024 }
}

/// Apply `action` to the given page runs of another process, batching the
/// runs into as few process_madvise() calls as IOV_MAX allows.
pub fn advise_runs(pid: u32, runs: &[PageRun], action: Action) -> Result<AdviseOutcome> {
    let mut outcome = AdviseOutcome::default();
    if runs.is_empty() {
        return Ok(outcome);
    }
    let advice = match action {
        Action::Cold => MADV_COLD,
        Action::PageOut => MADV_PAGEOUT,
    };

    let pidfd = match open_pidfd(pid) {
        Ok(fd) => fd,
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {
            outcome.gone = true;
            return Ok(outcome);
        }
        Err(e) => return Err(anyhow::anyhow!("pidfd_open({pid}) failed: {e}")),
    };

    let iovs: Vec<libc::iovec> = runs
        .iter()
        .map(|r| libc::iovec {
            iov_base: r.addr() as *mut libc::c_void,
            iov_len: r.len_bytes() as usize,
        })
        .collect();
    let max = iov_max();

    // process_madvise(2) handles the iovecs in order and stops at the first
    // one that fails, returning the bytes done before it (or -1 if the very
    // first one failed). So after a short return, skip the iovec it stopped
    // on and carry on with the rest — one unmapped range must not cost us
    // every range after it.
    let mut i = 0;
    while i < iovs.len() {
        let batch = &iovs[i..(i + max).min(iovs.len())];
        let ret = unsafe {
            libc::syscall(
                SYS_PROCESS_MADVISE,
                pidfd,
                batch.as_ptr(),
                batch.len(), // vlen: size_t
                advice as libc::c_int,
                0 as libc::c_uint,
            )
        };

        let done = if ret >= 0 {
            let ret = ret as u64;
            outcome.bytes += ret;
            // Count whole iovecs covered by `ret`.
            let mut sum = 0u64;
            let mut n = 0;
            while n < batch.len() && sum + batch[n].iov_len as u64 <= ret {
                sum += batch[n].iov_len as u64;
                n += 1;
            }
            if n == batch.len() {
                i += n;
                continue;
            }
            n
        } else {
            0
        };

        // batch[done] failed. Find out why, if the call itself errored;
        // a short positive return carries no errno, so probe the one range.
        let err = if ret < 0 {
            std::io::Error::last_os_error()
        } else {
            let one = unsafe {
                libc::syscall(SYS_PROCESS_MADVISE, pidfd, &batch[done] as *const libc::iovec, 1usize, advice as libc::c_int, 0 as libc::c_uint)
            };
            if one >= 0 {
                // Succeeded on retry (mapping changed back) — count it.
                outcome.bytes += one as u64;
                i += done + 1;
                continue;
            }
            std::io::Error::last_os_error()
        };
        match err.raw_os_error() {
            // Process exited.
            Some(libc::ESRCH) => {
                outcome.gone = true;
                break;
            }
            // madvise(2): "Addresses in the specified range are not
            // currently mapped" — the process unmapped or remapped this
            // memory after we read smaps. A race, not memory pressure.
            Some(libc::ENOMEM) => {
                log::debug!(
                    "process_madvise({pid}, {action:?}): range 0x{:x} no longer mapped (process changed its mappings since the scan)",
                    batch[done].iov_base as u64,
                );
                i += done + 1;
            }
            // Anything else (EPERM, EINVAL, ...) would fail the same way for
            // every remaining range of this process.
            _ => {
                log::warn!("process_madvise({pid}, {action:?}) failed: {err}");
                break;
            }
        }
    }

    unsafe {
        libc::close(pidfd);
    }
    Ok(outcome)
}

/// Open a pidfd for the given PID (pidfd_open syscall, Linux 5.3+).
fn open_pidfd(pid: u32) -> std::io::Result<i32> {
    let fd = unsafe { libc::syscall(SYS_PIDFD_OPEN, pid as libc::c_int, 0 as libc::c_uint) };

    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(fd as i32)
}

/// Check if process_madvise is supported on this kernel.
pub fn is_supported() -> bool {
    let pidfd = match open_pidfd(std::process::id()) {
        Ok(fd) => fd,
        Err(_) => return false,
    };

    let iov = libc::iovec {
        iov_base: std::ptr::null_mut(),
        iov_len: 0,
    };

    let ret = unsafe {
        libc::syscall(
            SYS_PROCESS_MADVISE,
            pidfd,
            &iov as *const libc::iovec,
            1usize, // vlen: size_t
            MADV_COLD as libc::c_int,
            0 as libc::c_uint,
        )
    };

    unsafe {
        libc::close(pidfd);
    }

    if ret < 0 {
        let err = std::io::Error::last_os_error();
        err.raw_os_error() != Some(libc::ENOSYS)
    } else {
        true
    }
}

/// Check if zswap is enabled.
pub fn zswap_enabled() -> bool {
    let path = Path::new("/sys/module/zswap/parameters/enabled");
    match fs::read_to_string(path) {
        Ok(val) => val.trim() == "Y",
        Err(_) => false,
    }
}

/// Current zswap statistics.
#[derive(Debug)]
pub struct ZswapStats {
    pub stored_pages: u64,
    pub pool_total_size: u64,
}

pub fn zswap_stats() -> Option<ZswapStats> {
    let stored = read_sysfs_u64("/sys/kernel/debug/zswap/stored_pages")?;
    let pool = read_sysfs_u64("/sys/kernel/debug/zswap/pool_total_size")?;
    Some(ZswapStats {
        stored_pages: stored,
        pool_total_size: pool,
    })
}

fn read_sysfs_u64(path: &str) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}
