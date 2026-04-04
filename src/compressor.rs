//! Compression engine — uses process_madvise() to push cold pages into zswap.
//!
//! process_madvise() (Linux 5.10+) lets us advise the kernel about another
//! process's memory without that process knowing:
//!
//! - MADV_COLD:    "deprioritize these pages" — kernel compresses them when convenient
//! - MADV_PAGEOUT: "reclaim these pages NOW" — forces them into zswap/swap immediately
//!
//! Requires CAP_SYS_NICE or same-uid as the target process.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

use crate::scanner::MemoryRegion;

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

/// Result of a compression action.
#[derive(Debug)]
pub struct CompressResult {
    pub success: bool,
}

/// Apply a memory advise action to a specific region of another process.
pub fn advise_region(pid: u32, region: &MemoryRegion, action: Action) -> Result<CompressResult> {
    let advice = match action {
        Action::Cold => MADV_COLD,
        Action::PageOut => MADV_PAGEOUT,
    };

    let region_size = region.end - region.start;

    let pidfd = open_pidfd(pid)
        .with_context(|| format!("opening pidfd for PID {pid}"))?;

    let iov = libc::iovec {
        iov_base: region.start as *mut libc::c_void,
        iov_len: region_size as usize,
    };

    let ret = unsafe {
        libc::syscall(
            SYS_PROCESS_MADVISE,
            pidfd,
            &iov as *const libc::iovec,
            1usize, // vlen: size_t
            advice as libc::c_int,
            0 as libc::c_uint,
        )
    };

    unsafe {
        libc::close(pidfd);
    }

    let success = ret >= 0;
    if !success {
        let err = std::io::Error::last_os_error();
        log::warn!(
            "process_madvise({pid}, {action:?}, 0x{:x}..0x{:x}) failed: {err}",
            region.start,
            region.end,
        );
    }

    Ok(CompressResult { success })
}

/// Open a pidfd for the given PID (pidfd_open syscall, Linux 5.3+).
fn open_pidfd(pid: u32) -> Result<i32> {
    let fd = unsafe { libc::syscall(SYS_PIDFD_OPEN, pid as libc::c_int, 0 as libc::c_uint) };

    if fd < 0 {
        anyhow::bail!(
            "pidfd_open({pid}) failed: {}",
            std::io::Error::last_os_error()
        );
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
