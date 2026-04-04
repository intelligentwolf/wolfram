# Wolfram

[![GitHub Sponsors](https://img.shields.io/github/sponsors/wolfsoftwaresystemsltd?label=Sponsor&logo=GitHub)](https://github.com/sponsors/wolfsoftwaresystemsltd)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Proactive memory compression and process hibernation daemon for Linux.

Wolfram monitors process memory, identifies pages that haven't been accessed recently, and pushes them into zswap compression — **before** the system is under memory pressure. Applications keep running normally and never know it's happening.

Optionally, completely idle processes can be hibernated to disk using CRIU and transparently restored when someone connects to their port.

## How it works

Linux already has RAM compression (zswap) but it only kicks in reactively when memory is tight. Wolfram uses kernel APIs that almost nobody uses to do it proactively:

1. **Scan** `/proc` for processes with significant anonymous memory
2. **Mark** all their pages as idle via `/sys/kernel/mm/page_idle/bitmap`
3. **Wait** 30 seconds (configurable)
4. **Check** which pages are still idle — these haven't been touched
5. **Compress** idle pages using `process_madvise()`:
   - 70%+ idle → `MADV_COLD` (kernel compresses when convenient)
   - 90%+ idle for 3+ cycles → `MADV_PAGEOUT` (compress into zswap immediately)
6. Repeat every 60 seconds

A process using 2 GB where 1.5 GB is cold will have that 1.5 GB compressed to ~500 MB in RAM. The process still thinks it has 2 GB. When it touches a compressed page, the kernel decompresses it transparently in microseconds.

## Will this affect my applications?

**Compression (default mode): No.** This is completely safe for all applications.

- The kernel already compresses and decompresses pages under memory pressure — Wolfram just triggers it earlier and more intelligently
- Applications are never modified, paused, or signalled
- Compressed pages are decompressed transparently by the kernel when accessed
- Worst case: a single-digit microsecond delay when touching a page that was compressed
- No API changes, no LD_PRELOAD, no patching — works with any language and runtime
- If Wolfram crashes or is stopped, nothing changes — compressed pages stay compressed and continue to work normally

**Hibernation (opt-in, off by default): Use with care.**

CRIU-based hibernation kills the process after checkpointing it. When restored:
- Database connections will be stale (use a connection pooler like PgBouncer for best results)
- Timers will fire immediately to catch up
- File locks are lost
- Not all applications survive checkpoint/restore (GPU state, certain socket types)
- Test thoroughly with your specific applications before enabling in production

Hibernation is designed for scenarios like OpenSim grids where many region processes sit completely idle for hours or days.

## Requirements

- Linux kernel 5.10+ (for `process_madvise`)
- `CONFIG_IDLE_PAGE_TRACKING=y` in kernel config (for accurate idle detection; Wolfram works without it but with reduced accuracy)
- zswap enabled for in-RAM compression (without it, pages go to disk swap which is slower)
- Root privileges or `CAP_SYS_ADMIN` + `CAP_SYS_NICE`
- CRIU installed (only if using hibernation — optional)

### Check your kernel support

```bash
# Idle page tracking
ls /sys/kernel/mm/page_idle/bitmap

# zswap status
cat /sys/module/zswap/parameters/enabled

# Enable zswap (temporary)
echo Y | sudo tee /sys/module/zswap/parameters/enabled

# Enable zswap (permanent — add to kernel command line)
# zswap.enabled=1
```

## Installation

```bash
git clone https://github.com/wolfssoftware/wolfram.git
cd wolfram
sudo ./install.sh
```

This builds the release binary, installs it to `/usr/local/bin/wolfram`, creates the config file at `/etc/wolfram/config.toml`, and sets up a systemd service.

```bash
# Start the service
sudo systemctl start wolfram

# View live logs
sudo journalctl -u wolfram -f

# Check how much memory is being saved
cat /var/lib/wolfram/stats.json

# Stop
sudo systemctl stop wolfram

# Uninstall
sudo ./uninstall.sh
```

## Usage

```bash
# Run with defaults (compression only, 50 MiB threshold)
sudo wolfram

# Lower threshold to target smaller processes
sudo wolfram --min-anon 20

# More aggressive compression
sudo wolfram --cold-threshold 0.5 --pageout-threshold 0.7

# Enable hibernation with wake-on-connect
sudo wolfram --hibernate --port-proxy

# Dry run — see what would happen without doing anything
sudo wolfram --dry-run -v

# Verbose logging
sudo wolfram -vv

# Use a config file
sudo wolfram -c /etc/wolfram/config.toml
```

## Configuration

Copy `config.example.toml` to `/etc/wolfram/config.toml` (done automatically by `install.sh`).

| Option | Default | Description |
|--------|---------|-------------|
| `min_process_anon_mib` | 50 | Minimum anonymous memory (MiB) for a process to be considered |
| `min_region_kib` | 1024 | Minimum memory region size (KiB) to track |
| `idle_sample_secs` | 30 | Seconds to wait between marking pages and checking them |
| `cycle_secs` | 60 | Seconds between full scan cycles |
| `cold_threshold` | 0.7 | Idle fraction to trigger MADV_COLD |
| `pageout_threshold` | 0.9 | Idle fraction to trigger MADV_PAGEOUT |
| `pageout_after_cycles` | 3 | Consecutive idle cycles before escalating to PAGEOUT |
| `enable_hibernation` | false | Enable CRIU-based process hibernation |
| `hibernate_after_cycles` | 10 | Fully-idle cycles before hibernating a process |
| `checkpoint_dir` | /var/lib/wolfram/checkpoints | Where to store CRIU checkpoints |
| `enable_port_proxy` | false | Listen on hibernated process ports for wake-on-connect |
| `excluded_names` | systemd, wolfram, sshd | Process names to never touch |
| `excluded_pids` | 1 | PIDs to never touch |
| `dry_run` | false | Scan and report without modifying anything |

CLI arguments override config file values. Config file overrides defaults.

## Stats

Wolfram writes current memory savings to `/var/lib/wolfram/stats.json` once per cycle:

```json
{
  "timestamp": "2026-04-04T12:00:00Z",
  "uptime_secs": 7200,
  "cycles": 120,
  "zswap_stored_pages": 250000,
  "zswap_original_bytes": 1024000000,
  "zswap_compressed_bytes": 360000000,
  "zswap_saved_bytes": 664000000,
  "zswap_ratio": 2.84,
  "hibernated_processes": 0,
  "hibernated_saved_mib": 0,
  "total_saved_mib": 633,
  "lifetime_compressed_kib": 30720000
}
```

Quick check:
```bash
# Total MiB currently saved
jq .total_saved_mib /var/lib/wolfram/stats.json

# Compression ratio
jq .zswap_ratio /var/lib/wolfram/stats.json

# Watch it update
watch -n 60 cat /var/lib/wolfram/stats.json
```

## Architecture

```
src/
├── main.rs           Entry point, signal handling, system info
├── config.rs         TOML config file + clap CLI arguments
├── scanner.rs        Reads /proc for processes and memory regions
├── idle_tracker.rs   Page idle bitmap — mark pages, check which stayed idle
├── compressor.rs     process_madvise(MADV_COLD/MADV_PAGEOUT) syscall
├── hibernator.rs     CRIU checkpoint/restore + port proxy wake-on-connect
├── stats.rs          Writes memory savings to stats.json each cycle
└── daemon.rs         Main loop: scan → mark → wait → check → compress → stats
```

### Kernel APIs used

| API | Purpose | Required |
|-----|---------|----------|
| `/proc/[pid]/smaps` | Find anonymous memory regions | Yes |
| `/proc/[pid]/pagemap` | Translate virtual pages to physical frame numbers | Yes |
| `/sys/kernel/mm/page_idle/bitmap` | Mark/check page idle state | Recommended |
| `process_madvise()` (syscall 440) | Advise kernel about another process's pages | Yes |
| `pidfd_open()` (syscall 434) | Get a file descriptor for a process | Yes |
| `/sys/module/zswap/` | Check zswap status | Recommended |
| `/sys/kernel/debug/zswap/` | Read compression stats | Optional |

## Graduated memory management

Wolfram applies increasing levels of intervention:

| Level | Trigger | Action | RAM saved | Impact |
|-------|---------|--------|-----------|--------|
| **Cold** | 70%+ pages idle | `MADV_COLD` — deprioritize | Eventual | None — kernel compresses when convenient |
| **Page out** | 90%+ idle, 3+ cycles | `MADV_PAGEOUT` — compress now | Immediate | Microsecond decompression on access |
| **Hibernate** | All regions idle, 10+ cycles | CRIU checkpoint + kill | 100% of process | Seconds to restore; test your apps |

## Safety

- Wolfram never modifies process memory, code, or state
- Compression uses the same kernel path as normal memory pressure — pages are decompressed transparently
- Excluded processes (systemd, sshd, wolfram itself) are never touched
- On shutdown (SIGTERM/SIGINT), all hibernated processes are automatically restored
- Dry-run mode (`--dry-run`) lets you see what would happen without any changes
- The systemd service runs with minimal capabilities and filesystem restrictions

## Building from source

```bash
# Requires Rust 1.75+
cargo build --release
sudo ./target/release/wolfram --dry-run -v
```

## License

MIT

## Author

Wolf Software Systems Ltd
