//! Idle page tracker — uses /proc/[pid]/pagemap and /sys/kernel/mm/page_idle/bitmap
//! to determine which pages in a process haven't been accessed recently.
//!
//! Usage pattern (the caller controls timing):
//! 1. `mark_idle()` for all regions across all processes (fast)
//! 2. Sleep once for the sample interval
//! 3. `check_idle()` for all regions (fast)
//! 4. Act on the results
//!
//! Requires CAP_SYS_ADMIN or root to read pagemap and write to page_idle.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};

use crate::scanner::MemoryRegion;

const PAGE_SIZE: u64 = 4096;
const PAGEMAP_ENTRY_SIZE: u64 = 8;

// Pagemap entry bits (Documentation/admin-guide/mm/pagemap.rst).
const PM_PRESENT: u64 = 1 << 63;
const PM_PFN_MASK: u64 = (1 << 55) - 1;

/// Physical page frame numbers collected for a region.
/// Store this between mark and check calls.
pub struct MarkedRegion {
    pub region: MemoryRegion,
    /// (virtual_page, physical_frame_number) pairs for resident pages.
    pub pfns: Vec<(u64, u64)>,
}

/// Result of idle-checking a memory region.
#[derive(Debug, Clone)]
pub struct IdleResult {
    pub region: MemoryRegion,
    /// Number of pages that are resident in RAM.
    pub resident_pages: u64,
    /// Number of resident pages that were idle (not accessed since last mark).
    pub idle_pages: u64,
}

impl IdleResult {
    /// Fraction of resident pages that are idle (0.0 to 1.0).
    pub fn idle_fraction(&self) -> f64 {
        if self.resident_pages == 0 {
            return 0.0;
        }
        self.idle_pages as f64 / self.resident_pages as f64
    }

    /// KiB of memory that is idle and could be compressed.
    #[allow(dead_code)] // public API for future use
    pub fn idle_kib(&self) -> u64 {
        self.idle_pages * (PAGE_SIZE / 1024)
    }
}

/// Collect PFNs and mark pages as idle for a region.
/// Returns a `MarkedRegion` to pass to `check_idle` after waiting.
pub fn mark_idle(pid: u32, region: &MemoryRegion) -> Result<MarkedRegion> {
    let pfns = collect_pfns(pid, region)?;

    if !pfns.is_empty() {
        write_idle_bits(&pfns)?;
    }

    Ok(MarkedRegion {
        region: region.clone(),
        pfns,
    })
}

/// Check which pages are still idle. Call after `mark_idle` + a waiting period.
pub fn check_idle(marked: &MarkedRegion) -> Result<IdleResult> {
    if marked.pfns.is_empty() {
        return Ok(IdleResult {
            region: marked.region.clone(),
            resident_pages: 0,
            idle_pages: 0,
        });
    }

    let mut idle_file = File::open("/sys/kernel/mm/page_idle/bitmap")
        .context("opening page_idle bitmap for reading")?;

    // Batch reads by bitmap offset for efficiency.
    let mut by_offset: HashMap<u64, Vec<u64>> = HashMap::new();
    for &(_vpage, pfn) in &marked.pfns {
        let offset = (pfn / 64) * 8;
        by_offset.entry(offset).or_default().push(pfn);
    }

    // Sort offsets so we seek forward through the file.
    let mut offsets: Vec<u64> = by_offset.keys().copied().collect();
    offsets.sort();

    let mut idle_count: u64 = 0;
    for offset in offsets {
        idle_file.seek(SeekFrom::Start(offset))?;
        let mut buf = [0u8; 8];
        idle_file.read_exact(&mut buf)?;
        let bits = u64::from_le_bytes(buf);

        for &pfn in &by_offset[&offset] {
            let bit = pfn % 64;
            if bits & (1u64 << bit) != 0 {
                idle_count += 1;
            }
        }
    }

    Ok(IdleResult {
        region: marked.region.clone(),
        resident_pages: marked.pfns.len() as u64,
        idle_pages: idle_count,
    })
}

// Max pagemap read per region: 8MB buffer = 1M pages = 4GB of virtual address space.
// Regions larger than this are read in chunks. This prevents multi-GB allocations
// for processes with huge virtual address spaces (common on large-RAM servers).
const MAX_PAGEMAP_READ: u64 = 1024 * 1024; // pages per chunk

/// Collect physical page frame numbers for a region by reading pagemap.
fn collect_pfns(pid: u32, region: &MemoryRegion) -> Result<Vec<(u64, u64)>> {
    let pagemap_path = format!("/proc/{pid}/pagemap");
    let mut pagemap = File::open(&pagemap_path)
        .with_context(|| format!("opening {pagemap_path} (need root or CAP_SYS_ADMIN)"))?;

    let start_page = region.start / PAGE_SIZE;
    let end_page = region.end / PAGE_SIZE;
    let total_pages = end_page - start_page;

    if total_pages == 0 {
        return Ok(Vec::new());
    }

    // Estimate: RSS pages / virtual pages gives residency ratio.
    // If virtual span is huge but RSS is tiny, most reads will be zeros.
    // Skip regions where virtual span is >100x the RSS — not worth the I/O.
    let rss_pages = region.rss_kib * 1024 / PAGE_SIZE;
    if rss_pages > 0 && total_pages > rss_pages * 100 {
        return Ok(Vec::new());
    }

    let mut pfns = Vec::with_capacity(rss_pages.min(65536) as usize);

    // Read pagemap in chunks to avoid huge allocations.
    let mut page_offset = start_page;
    while page_offset < end_page {
        let chunk_pages = (end_page - page_offset).min(MAX_PAGEMAP_READ);
        let file_offset = page_offset * PAGEMAP_ENTRY_SIZE;
        pagemap.seek(SeekFrom::Start(file_offset))?;

        let buf_size = (chunk_pages * PAGEMAP_ENTRY_SIZE) as usize;
        let mut buf = vec![0u8; buf_size];

        if pagemap.read_exact(&mut buf).is_err() {
            break; // process vanished mid-read
        }

        for i in 0..chunk_pages {
            let entry_offset = (i * PAGEMAP_ENTRY_SIZE) as usize;
            let entry = u64::from_le_bytes(
                buf[entry_offset..entry_offset + 8]
                    .try_into()
                    .unwrap(),
            );

            if entry & PM_PRESENT != 0 {
                let pfn = entry & PM_PFN_MASK;
                if pfn > 0 {
                    pfns.push((page_offset + i, pfn));
                }
            }
        }

        page_offset += chunk_pages;
    }

    Ok(pfns)
}

/// Write idle bits for a set of PFNs.
fn write_idle_bits(pfns: &[(u64, u64)]) -> Result<()> {
    let mut idle_file = OpenOptions::new()
        .write(true)
        .open("/sys/kernel/mm/page_idle/bitmap")
        .context("opening page_idle bitmap (need root and CONFIG_IDLE_PAGE_TRACKING)")?;

    let mut by_offset: HashMap<u64, u64> = HashMap::new();
    for &(_vpage, pfn) in pfns {
        let offset = (pfn / 64) * 8;
        let bit = pfn % 64;
        *by_offset.entry(offset).or_insert(0) |= 1u64 << bit;
    }

    // Sort offsets for sequential writes.
    let mut offsets: Vec<(u64, u64)> = by_offset.into_iter().collect();
    offsets.sort_by_key(|&(off, _)| off);

    for (offset, bits) in offsets {
        idle_file.seek(SeekFrom::Start(offset))?;
        idle_file.write_all(&bits.to_le_bytes())?;
    }

    Ok(())
}

/// Check if the kernel has idle page tracking support.
pub fn is_supported() -> bool {
    std::path::Path::new("/sys/kernel/mm/page_idle/bitmap").exists()
}
