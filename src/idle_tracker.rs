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

// Pagemap entry bits (proc_pid_pagemap(5)): 63 present in RAM, 62 in swap,
// 61 file-mapped or shared-anonymous page, 54-0 PFN when present.
const PM_PRESENT: u64 = 1 << 63;
const PM_SWAP: u64 = 1 << 62;
const PM_FILE: u64 = 1 << 61;
const PM_PFN_MASK: u64 = (1 << 55) - 1;

/// A run of consecutive virtual pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRun {
    pub start_vpage: u64,
    pub pages: u64,
}

impl PageRun {
    pub fn addr(&self) -> u64 {
        self.start_vpage * PAGE_SIZE
    }
    pub fn len_bytes(&self) -> u64 {
        self.pages * PAGE_SIZE
    }
}

/// Coalesce sorted, de-duplicated virtual page numbers into runs.
fn coalesce(vpages: impl IntoIterator<Item = u64>) -> Vec<PageRun> {
    let mut runs: Vec<PageRun> = Vec::new();
    for v in vpages {
        match runs.last_mut() {
            Some(r) if r.start_vpage + r.pages == v => r.pages += 1,
            _ => runs.push(PageRun { start_vpage: v, pages: 1 }),
        }
    }
    runs
}

pub fn total_pages(runs: &[PageRun]) -> u64 {
    runs.iter().map(|r| r.pages).sum()
}

pub fn pages_to_kib(pages: u64) -> u64 {
    pages * (PAGE_SIZE / 1024)
}

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
    /// The idle pages as virtual-page runs: the only pages worth advising.
    /// Advising the whole region would also push out its hot pages, which
    /// fault straight back in.
    pub idle_runs: Vec<PageRun>,
}

impl IdleResult {
    /// Fraction of resident pages that are idle (0.0 to 1.0).
    pub fn idle_fraction(&self) -> f64 {
        if self.resident_pages == 0 {
            return 0.0;
        }
        self.idle_pages as f64 / self.resident_pages as f64
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
            idle_runs: Vec::new(),
        });
    }

    let mut idle_file = File::open("/sys/kernel/mm/page_idle/bitmap")
        .context("opening page_idle bitmap for reading")?;

    // Read each 64-bit bitmap word once, in ascending offset order.
    let mut offsets: Vec<u64> = marked.pfns.iter().map(|&(_, pfn)| (pfn / 64) * 8).collect();
    offsets.sort_unstable();
    offsets.dedup();
    let mut words: HashMap<u64, u64> = HashMap::with_capacity(offsets.len());
    for offset in offsets {
        idle_file.seek(SeekFrom::Start(offset))?;
        let mut buf = [0u8; 8];
        idle_file.read_exact(&mut buf)?;
        words.insert(offset, u64::from_le_bytes(buf));
    }

    // pfns are in ascending virtual-page order (collect_pfns walks forward).
    let idle_vpages = marked.pfns.iter().filter_map(|&(vpage, pfn)| {
        let bits = words[&((pfn / 64) * 8)];
        (bits & (1u64 << (pfn % 64)) != 0).then_some(vpage)
    });
    let idle_runs = coalesce(idle_vpages);

    Ok(IdleResult {
        region: marked.region.clone(),
        resident_pages: marked.pfns.len() as u64,
        idle_pages: total_pages(&idle_runs),
        idle_runs,
    })
}

/// How many pages of `runs` were resident when `marked` was collected.
/// Called with the runs paged out last cycle, this is the refault count.
pub fn present_pages_in(marked: &MarkedRegion, runs: &[PageRun]) -> u64 {
    let mut count = 0;
    let mut ri = 0;
    for &(vpage, _) in &marked.pfns {
        while ri < runs.len() && runs[ri].start_vpage + runs[ri].pages <= vpage {
            ri += 1;
        }
        if ri == runs.len() {
            break;
        }
        if vpage >= runs[ri].start_vpage {
            count += 1;
        }
    }
    count
}

/// Of the pages in `runs`, those now out in swap (bit 62 set, not present).
/// Called right after MADV_PAGEOUT, this is what really left RAM: the
/// kernel skips pages it cannot reclaim (shared, pinned, under writeback).
pub fn swapped_runs(pid: u32, runs: &[PageRun]) -> Result<Vec<PageRun>> {
    let pagemap_path = format!("/proc/{pid}/pagemap");
    let mut pagemap = File::open(&pagemap_path)
        .with_context(|| format!("opening {pagemap_path}"))?;
    let mut swapped = Vec::new();
    for run in runs {
        pagemap.seek(SeekFrom::Start(run.start_vpage * PAGEMAP_ENTRY_SIZE))?;
        let mut buf = vec![0u8; (run.pages * PAGEMAP_ENTRY_SIZE) as usize];
        if pagemap.read_exact(&mut buf).is_err() {
            break; // process vanished or unmapped the range
        }
        for (i, chunk) in buf.chunks_exact(8).enumerate() {
            let entry = u64::from_le_bytes(chunk.try_into().unwrap());
            if entry & PM_SWAP != 0 && entry & PM_PRESENT == 0 {
                swapped.push(run.start_vpage + i as u64);
            }
        }
    }
    Ok(coalesce(swapped))
}

// Max pagemap read per chunk: 8MB buffer = 1M pages = 4GB of virtual address space.
const MAX_PAGEMAP_READ: u64 = 1024 * 1024; // pages per chunk

// Max PFNs to collect per region. Caps memory at ~8MB per region (512K * 16 bytes).
// Regions beyond this are sampled, not fully tracked.
const MAX_PFNS_PER_REGION: usize = 512 * 1024;

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
    while page_offset < end_page && pfns.len() < MAX_PFNS_PER_REGION {
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

            // Skip file / shared-anonymous pages: only exclusive anonymous
            // memory is a page-out candidate.
            if entry & PM_PRESENT != 0 && entry & PM_FILE == 0 {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesce_runs() {
        let r = coalesce([1, 2, 3, 7, 8, 10]);
        assert_eq!(
            r,
            vec![
                PageRun { start_vpage: 1, pages: 3 },
                PageRun { start_vpage: 7, pages: 2 },
                PageRun { start_vpage: 10, pages: 1 },
            ]
        );
        assert_eq!(total_pages(&r), 6);
        assert!(coalesce(std::iter::empty()).is_empty());
    }

    #[test]
    fn present_pages_counts_only_inside_runs() {
        let region = MemoryRegion { pid: 1, start: 0, end: 100 * PAGE_SIZE, rss_kib: 0 };
        let marked = MarkedRegion {
            region,
            pfns: [0u64, 1, 2, 5, 6, 9, 20].iter().map(|&v| (v, 1000 + v)).collect(),
        };
        let runs = [
            PageRun { start_vpage: 1, pages: 2 }, // 1,2 -> 2 present
            PageRun { start_vpage: 4, pages: 3 }, // 4,5,6 -> 2 present
            PageRun { start_vpage: 15, pages: 3 }, // none
        ];
        assert_eq!(present_pages_in(&marked, &runs), 4);
        assert_eq!(present_pages_in(&marked, &[]), 0);
    }
}
