//! Process scanner — reads /proc to find processes and their anonymous memory regions.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Information about a running process.
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    /// Resident Set Size in KiB (physical RAM currently used).
    pub rss_kib: u64,
    /// Anonymous memory in KiB (heap, stack, mmap — the compressible stuff).
    pub anon_kib: u64,
}

/// A contiguous anonymous memory region within a process.
#[derive(Debug, Clone)]
pub struct MemoryRegion {
    #[allow(dead_code)] // used in Debug output
    pub pid: u32,
    pub start: u64,
    pub end: u64,
    /// Resident anonymous pages in this region (KiB).
    pub rss_kib: u64,
}

/// Scan all processes, return those with significant anonymous memory.
pub fn scan_processes(min_anon_kib: u64) -> Result<Vec<ProcessInfo>> {
    let mut procs = Vec::new();

    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };

        // Skip PID 1 (init) and our own process.
        if pid <= 1 || pid == std::process::id() {
            continue;
        }

        match read_process_info(pid) {
            Ok(info) if info.anon_kib >= min_anon_kib => procs.push(info),
            Ok(_) => {} // below threshold
            Err(_) => {} // process vanished or permission denied — normal
        }
    }

    // Sort by anonymous memory descending — biggest targets first.
    procs.sort_by_key(|p| std::cmp::Reverse(p.anon_kib));
    Ok(procs)
}

/// Read a single process's info from /proc/[pid]/status and smaps_rollup.
fn read_process_info(pid: u32) -> Result<ProcessInfo> {
    let status_path = format!("/proc/{pid}/status");
    let status = fs::read_to_string(&status_path)
        .with_context(|| format!("reading {status_path}"))?;

    let name = parse_status_field(&status, "Name:")
        .unwrap_or_else(|| String::from("unknown"));
    let rss_kib = parse_status_kib(&status, "VmRSS:").unwrap_or(0);

    // smaps_rollup gives us aggregated anonymous memory without parsing every region.
    let rollup_path = format!("/proc/{pid}/smaps_rollup");
    let anon_kib = if Path::new(&rollup_path).exists() {
        let rollup = fs::read_to_string(&rollup_path).unwrap_or_default();
        parse_status_kib(&rollup, "Anonymous:").unwrap_or(0)
    } else {
        // Fallback: use RssAnon from status.
        parse_status_kib(&status, "RssAnon:").unwrap_or(0)
    };

    Ok(ProcessInfo {
        pid,
        name,
        rss_kib,
        anon_kib,
    })
}

/// Read /proc/[pid]/smaps to get individual anonymous memory regions.
/// Only returns private, unlocked, ordinary anonymous regions with at least
/// `min_region_kib` resident.
pub fn scan_memory_regions(pid: u32, min_region_kib: u64) -> Result<Vec<MemoryRegion>> {
    let smaps_path = format!("/proc/{pid}/smaps");
    let smaps = fs::read_to_string(&smaps_path)
        .with_context(|| format!("reading {smaps_path}"))?;
    Ok(parse_smaps(pid, &smaps, min_region_kib))
}

/// One smaps entry while its detail lines are being read.
struct PendingRegion {
    start: u64,
    end: u64,
    eligible: bool,
    rss_kib: u64,
    locked_kib: u64,
    bad_flags: bool,
}

impl PendingRegion {
    fn finish(self, pid: u32, min_region_kib: u64, out: &mut Vec<MemoryRegion>) {
        if self.eligible
            && !self.bad_flags
            && self.locked_kib == 0
            && self.rss_kib >= min_region_kib
        {
            out.push(MemoryRegion {
                pid,
                start: self.start,
                end: self.end,
                rss_kib: self.rss_kib,
            });
        }
    }
}

fn parse_smaps(pid: u32, smaps: &str, min_region_kib: u64) -> Vec<MemoryRegion> {
    let mut regions = Vec::new();
    let mut current: Option<PendingRegion> = None;

    for line in smaps.lines() {
        // Region header line: "7f1234000000-7f1235000000 rw-p 00000000 00:00 0"
        if let Some(h) = parse_region_header(line) {
            if let Some(prev) = current.take() {
                prev.finish(pid, min_region_kib, &mut regions);
            }
            current = Some(PendingRegion {
                start: h.start,
                end: h.end,
                eligible: h.anon && h.private,
                rss_kib: 0,
                locked_kib: 0,
                bad_flags: false,
            });
        } else if let Some(cur) = current.as_mut() {
            if let Some(rss) = parse_kib_line(line, "Rss:") {
                cur.rss_kib = rss;
            } else if let Some(locked) = parse_kib_line(line, "Locked:") {
                cur.locked_kib = locked;
            } else if let Some(flags) = line.strip_prefix("VmFlags:") {
                // Codes from proc_pid_smaps(5): sh = shared, lo = locked,
                // io = memory-mapped I/O, pf = pure PFN range,
                // ht = hugetlb. None of these are ordinary swappable
                // anonymous memory.
                cur.bad_flags = flags
                    .split_whitespace()
                    .any(|f| matches!(f, "sh" | "lo" | "io" | "pf" | "ht"));
            }
        }
    }

    if let Some(prev) = current.take() {
        prev.finish(pid, min_region_kib, &mut regions);
    }
    regions
}

struct RegionHeader {
    start: u64,
    end: u64,
    /// No backing file (inode 0) and not a kernel special mapping.
    anon: bool,
    /// MAP_PRIVATE ('p' in the 4th permission column). Shared mappings
    /// ('s': SysV shm, memfd, shared /dev/zero, driver buffers) are never
    /// touched.
    private: bool,
}

/// Parse a smaps region header line.
fn parse_region_header(line: &str) -> Option<RegionHeader> {
    // Format: "7f1234000000-7f1235000000 rw-p 00000000 00:00 0       [heap]"
    // Anonymous regions have inode 0 and no pathname (or [heap], [stack]).
    let mut parts = line.split_whitespace();
    let range = parts.next()?;
    let perms = parts.next()?;
    let _offset = parts.next()?;
    let _dev = parts.next()?;
    let inode = parts.next()?;

    let (start_str, end_str) = range.split_once('-')?;
    let start = u64::from_str_radix(start_str, 16).ok()?;
    let end = u64::from_str_radix(end_str, 16).ok()?;
    if perms.len() != 4 {
        return None;
    }

    // Anonymous: inode is 0 (no backing file).
    // But exclude kernel special mappings that also have inode 0.
    let pathname = parts.next().unwrap_or("");
    let is_kernel_special = matches!(pathname, "[vdso]" | "[vvar]" | "[vvar_vclock]" | "[vsyscall]");
    let anon = inode == "0" && !is_kernel_special;
    let private = perms.as_bytes()[3] == b'p';

    Some(RegionHeader { start, end, anon, private })
}

/// Does the process hold a GPU / accelerator device open? Device-visible
/// host buffers (SYCL/Level Zero USM, CUDA pinned/managed memory, ROCm) look
/// like ordinary private anonymous memory, so the only reliable signal is the
/// device fd. A process we cannot inspect counts as "no".
pub fn holds_gpu_device(pid: u32) -> bool {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    entries.flatten().any(|e| {
        fs::read_link(e.path())
            .ok()
            .and_then(|t| t.to_str().map(is_gpu_device_path))
            .unwrap_or(false)
    })
}

fn is_gpu_device_path(p: &str) -> bool {
    p.starts_with("/dev/dri/")
        || p == "/dev/kfd"
        || p.starts_with("/dev/nvidia")
        || p.starts_with("/dev/accel/")
}

fn parse_status_field(status: &str, key: &str) -> Option<String> {
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix(key) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn parse_status_kib(status: &str, key: &str) -> Option<u64> {
    parse_kib_line_from_text(status, key)
}

fn parse_kib_line_from_text(text: &str, key: &str) -> Option<u64> {
    for line in text.lines() {
        if let Some(val) = parse_kib_line(line, key) {
            return Some(val);
        }
    }
    None
}

fn parse_kib_line(line: &str, key: &str) -> Option<u64> {
    let rest = line.strip_prefix(key)?;
    let num_str = rest.trim().trim_end_matches(" kB").trim();
    num_str.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(header: &str, extra: &str) -> String {
        format!("{header}\nSize:               8192 kB\nRss:                8192 kB\n{extra}VmFlags: rd wr mr mw me ac\n")
    }

    #[test]
    fn private_anon_region_is_eligible() {
        let s = region("7f0000000000-7f0000800000 rw-p 00000000 00:00 0", "Locked:                0 kB\n");
        let r = parse_smaps(1, &s, 1024);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].rss_kib, 8192);
    }

    #[test]
    fn shared_mapping_is_skipped() {
        let s = region("7f0000000000-7f0000800000 rw-s 00000000 00:01 0", "");
        assert!(parse_smaps(1, &s, 1024).is_empty());
    }

    #[test]
    fn locked_region_is_skipped() {
        let s = region("7f0000000000-7f0000800000 rw-p 00000000 00:00 0", "Locked:             8192 kB\n");
        assert!(parse_smaps(1, &s, 1024).is_empty());
    }

    #[test]
    fn io_and_pfnmap_flags_are_skipped() {
        for flag in ["io", "pf", "lo", "ht", "sh"] {
            let s = format!(
                "7f0000000000-7f0000800000 rw-p 00000000 00:00 0\nRss:                8192 kB\nVmFlags: rd wr {flag}\n"
            );
            assert!(parse_smaps(1, &s, 1024).is_empty(), "flag {flag}");
        }
    }

    #[test]
    fn file_backed_region_is_skipped() {
        let s = region("7f0000000000-7f0000800000 rw-p 00000000 08:01 1234 /usr/lib/x.so", "");
        assert!(parse_smaps(1, &s, 1024).is_empty());
    }

    #[test]
    fn flags_of_one_region_do_not_leak_into_next() {
        let s = format!(
            "{}{}",
            region("7f0000000000-7f0000800000 rw-p 00000000 00:00 0", "Locked:             8192 kB\n"),
            region("7f0001000000-7f0001800000 rw-p 00000000 00:00 0", "Locked:                0 kB\n"),
        );
        let r = parse_smaps(1, &s, 1024);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].start, 0x7f0001000000);
    }

    #[test]
    fn gpu_device_paths() {
        assert!(is_gpu_device_path("/dev/dri/renderD128"));
        assert!(is_gpu_device_path("/dev/kfd"));
        assert!(is_gpu_device_path("/dev/nvidia0"));
        assert!(is_gpu_device_path("/dev/nvidia-uvm"));
        assert!(is_gpu_device_path("/dev/accel/accel0"));
        assert!(!is_gpu_device_path("/dev/null"));
        assert!(!is_gpu_device_path("anon_inode:i915.gem"));
    }
}
