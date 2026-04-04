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
    procs.sort_by(|a, b| b.anon_kib.cmp(&a.anon_kib));
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
/// Only returns regions above `min_region_kib`.
pub fn scan_memory_regions(pid: u32, min_region_kib: u64) -> Result<Vec<MemoryRegion>> {
    let smaps_path = format!("/proc/{pid}/smaps");
    let smaps = fs::read_to_string(&smaps_path)
        .with_context(|| format!("reading {smaps_path}"))?;

    let mut regions = Vec::new();
    let mut current_start: u64 = 0;
    let mut current_end: u64 = 0;
    let mut current_rss: u64 = 0;
    let mut is_anon = false;
    let mut in_region = false;

    for line in smaps.lines() {
        // Region header line: "7f1234000000-7f1235000000 rw-p 00000000 00:00 0"
        if let Some((start, end, anon)) = parse_region_header(line) {
            // Save the previous region if it qualifies.
            if in_region && is_anon && current_rss >= min_region_kib {
                regions.push(MemoryRegion {
                    pid,
                    start: current_start,
                    end: current_end,
                    rss_kib: current_rss,
                });
            }
            current_start = start;
            current_end = end;
            current_rss = 0;
            is_anon = anon;
            in_region = true;
        } else if in_region {
            // Look for Rss: line within region details.
            if let Some(rss) = parse_kib_line(line, "Rss:") {
                current_rss = rss;
            }
        }
    }

    // Don't forget the last region.
    if in_region && is_anon && current_rss >= min_region_kib {
        regions.push(MemoryRegion {
            pid,
            start: current_start,
            end: current_end,
            rss_kib: current_rss,
        });
    }

    Ok(regions)
}

/// Parse a smaps region header line.
/// Returns (start_addr, end_addr, is_anonymous).
fn parse_region_header(line: &str) -> Option<(u64, u64, bool)> {
    // Format: "7f1234000000-7f1235000000 rw-p 00000000 00:00 0       [heap]"
    // Anonymous regions have inode 0 and no pathname (or [heap], [stack]).
    let mut parts = line.split_whitespace();
    let range = parts.next()?;
    let _perms = parts.next()?;
    let _offset = parts.next()?;
    let _dev = parts.next()?;
    let inode = parts.next()?;

    let (start_str, end_str) = range.split_once('-')?;
    let start = u64::from_str_radix(start_str, 16).ok()?;
    let end = u64::from_str_radix(end_str, 16).ok()?;

    // Anonymous: inode is 0 (no backing file).
    // But exclude kernel special mappings that also have inode 0.
    let pathname = parts.next().unwrap_or("");
    let is_kernel_special = matches!(pathname, "[vdso]" | "[vvar]" | "[vsyscall]");
    let is_anon = inode == "0" && !is_kernel_special;

    Some((start, end, is_anon))
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
