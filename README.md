# Wolfram

[![GitHub Sponsors](https://img.shields.io/github/sponsors/wolfsoftwaresystemsltd?label=Sponsor&logo=GitHub)](https://github.com/sponsors/wolfsoftwaresystemsltd)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Proactive memory compression daemon for Linux. Wolfram identifies cold memory pages across all processes, compresses them in RAM using zram, and dynamically grows/shrinks its compressed pool based on demand. No disk I/O. Applications keep running normally and never know it's happening.

Optionally, completely idle processes can be hibernated to disk using CRIU and transparently restored when someone connects to their port.

## How it works

```
                    Physical RAM (128 GB)
    ┌──────────────────────────────────────────────┐
    │  Hot pages (actively used)         80 GB     │
    │  Cold pages (untouched)            48 GB ──┐ │
    │                                            │ │
    │  zram (compressed in RAM)     ┌────────────┘ │
    │  48 GB compressed to ~20 GB   │  20 GB       │
    │                               └──────────    │
    │  Free (reclaimed by wolfram)       28 GB     │
    └──────────────────────────────────────────────┘
```

Each cycle:

1. **Scan** `/proc` for processes with significant anonymous memory
2. **Mark** all their pages as idle via `/sys/kernel/mm/page_idle/bitmap`
3. **Wait** 30 seconds (configurable)
4. **Check** which pages are still idle — these haven't been touched
5. **Compress** idle pages using `process_madvise()`:
   - 50%+ idle → `MADV_COLD` (kernel deprioritizes, compresses when convenient)
   - 70%+ idle for 2+ cycles → `MADV_PAGEOUT` (compress into zram immediately)
6. **Adjust** zram pool — grow if >80% full and RAM is available, shrink if <40% used
7. Write stats to `/var/lib/wolfram/stats.json`
8. Repeat

### Dynamic zram

Wolfram creates and manages its own zram compressed swap devices entirely in RAM — no disk swap files needed.

- **Starts small**: creates an initial zram device (3% of RAM)
- **Grows on demand**: if zram is >80% full and free RAM exists, adds another device
- **Shrinks when idle**: if utilisation drops below 40%, removes the newest device
- **Caps at 50% of RAM**: never overcommits — always leaves headroom for active workloads
- **Uses lz4 compression**: fast, typically 2-3x compression ratio
- **Kernel load-balances** across multiple zram swap devices automatically
- **Zero disk I/O**: compressed pages never touch disk. Ever.

This is the same approach Android uses on every phone on the planet.

### CPU priority

Wolfram runs at the absolute lowest CPU and I/O priority:

- **nice 19** — lowest scheduling priority
- **SCHED_IDLE** — kernel only runs wolfram when nothing else wants the CPU
- **ionice idle** — only uses disk I/O when disk is idle

On a busy server or Raspberry Pi, wolfram will never compete with real workloads. It runs in the gaps when the CPU would otherwise be idle.

## Will this affect my applications?

**Compression (default mode): No.** This is completely safe for all applications.

- The kernel already compresses and decompresses pages under memory pressure — Wolfram just triggers it earlier and more intelligently
- Applications are never modified, paused, or signalled
- Compressed pages are decompressed transparently by the kernel when accessed
- Worst case: a single-digit microsecond delay when touching a page that was compressed
- No API changes, no LD_PRELOAD, no patching — works with any language and runtime (.NET, Java, Python, C++, anything)
- If Wolfram crashes or is stopped, compressed pages in zram continue to work normally
- On shutdown, wolfram cleans up its zram devices gracefully

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
- zram kernel module (most distributions include it; wolfram loads it automatically)
- `CONFIG_IDLE_PAGE_TRACKING=y` (for accurate idle detection; wolfram works without it but with reduced accuracy)
- Root privileges
- CRIU installed (only if using hibernation — optional)

## Installation

```bash
curl -fsSL https://raw.githubusercontent.com/wolfsoftwaresystemsltd/wolfram/master/install.sh | sudo sh
```

That's it. The installer:

1. **Auto-detects your platform** (x86_64, ARM64, ARMv7) and downloads the correct static binary
2. **Auto-detects your environment** and generates an optimised config:

| Environment | Detection | Tuning |
|-------------|-----------|--------|
| **Proxmox VE** | `/etc/pve` or `pvesh` | 5min cycles, 100MB+ processes, excludes pvedaemon/pveproxy/qemu/lxc-start/ceph |
| **Kubernetes** | `kubelet` present | 5min cycles, 100MB+ processes, excludes kubelet/kube-proxy/etcd/calico |
| **Docker host** | `dockerd` running | 3min cycles, 50MB+ processes, excludes dockerd/containerd |
| **Raspberry Pi** | `/proc/cpuinfo` | 5min cycles, 10MB+ processes, tuned for low memory |
| **Desktop** | Xorg/Wayland present | 2min cycles, excludes display server/audio/WM |
| **Server** | Default | 2min cycles, standard defaults |

3. **Loads the zram kernel module** if not already loaded
4. **Enables and starts** the systemd service immediately

No Rust toolchain needed. All builds are fully static (musl) — zero dependencies, works on any Linux distribution.

Re-running the installer upgrades the binary in place. Delete `/etc/wolfram/config.toml` before re-running if you want it to regenerate the config for your environment.

### After installation

```bash
journalctl -u wolfram -f              # View live logs
cat /var/lib/wolfram/stats.json       # Check memory savings
sudo systemctl stop wolfram            # Stop
sudo systemctl restart wolfram         # Restart after config change
```

### Build from source (optional)

```bash
git clone https://github.com/wolfsoftwaresystemsltd/wolfram.git
cd wolfram
cargo build --release
sudo ./install.sh   # uses local build instead of downloading
```

### Uninstall

```bash
curl -fsSL https://raw.githubusercontent.com/wolfsoftwaresystemsltd/wolfram/master/uninstall.sh | sudo sh
```

## Configuration

The installer generates `/etc/wolfram/config.toml` automatically based on your environment. Edit it and restart:

```bash
sudo nano /etc/wolfram/config.toml
sudo systemctl restart wolfram
```

| Option | Default | Description |
|--------|---------|-------------|
| `min_process_anon_mib` | 50-100 | Minimum anonymous memory (MiB) for a process to be considered |
| `min_region_kib` | 1024-4096 | Minimum memory region size (KiB) to track |
| `idle_sample_secs` | 30 | Seconds to sample idle pages |
| `cycle_secs` | 120-300 | Seconds between full scan cycles |
| `cold_threshold` | 0.5-0.7 | Idle fraction to trigger MADV_COLD |
| `pageout_threshold` | 0.7-0.9 | Idle fraction to trigger MADV_PAGEOUT |
| `pageout_after_cycles` | 2-3 | Consecutive idle cycles before escalating to PAGEOUT |
| `enable_hibernation` | false | Enable CRIU-based process hibernation |
| `hibernate_after_cycles` | 10 | Fully-idle cycles before hibernating a process |
| `enable_port_proxy` | false | Listen on hibernated process ports for wake-on-connect |
| `excluded_names` | (varies by environment) | Process names to never touch (substring match, max 15 chars) |
| `dry_run` | false | Scan and report without modifying anything |

CLI arguments override config file values. Config file overrides defaults.

## Stats

Wolfram writes current memory savings to `/var/lib/wolfram/stats.json` once per cycle:

```json
{
  "timestamp": "2026-04-04T12:00:00Z",
  "uptime_secs": 7200,
  "cycles": 24,
  "compression_backend": "zram",
  "original_bytes": 8589934592,
  "compressed_bytes": 3435973836,
  "saved_bytes": 5153960756,
  "compression_ratio": 2.5,
  "zram_devices": 2,
  "hibernated_processes": 0,
  "hibernated_saved_mib": 0,
  "total_saved_mib": 4915,
  "lifetime_compressed_kib": 30720000
}
```

Quick check:
```bash
# Total MiB currently saved
jq .total_saved_mib /var/lib/wolfram/stats.json

# Compression ratio
jq .compression_ratio /var/lib/wolfram/stats.json

# Watch it update
watch -n 60 cat /var/lib/wolfram/stats.json
```

## Architecture

```
src/
├── main.rs           Entry point, signal handling, priority setup
├── config.rs         TOML config file + clap CLI arguments
├── scanner.rs        Reads /proc for processes and memory regions
├── idle_tracker.rs   Page idle bitmap — mark pages, check which stayed idle
├── compressor.rs     process_madvise(MADV_COLD/MADV_PAGEOUT) syscall
├── zram.rs           Dynamic zram device management (create/grow/shrink/cleanup)
├── hibernator.rs     CRIU checkpoint/restore + port proxy wake-on-connect
├── stats.rs          Writes memory savings to stats.json each cycle
└── daemon.rs         Main loop: scan → mark → wait → check → compress → adjust zram → stats
```

### Kernel APIs used

| API | Purpose | Required |
|-----|---------|----------|
| `/proc/[pid]/smaps` | Find anonymous memory regions | Yes |
| `/proc/[pid]/pagemap` | Translate virtual pages to physical frame numbers | Yes |
| `/sys/kernel/mm/page_idle/bitmap` | Mark/check page idle state | Recommended |
| `process_madvise()` | Advise kernel about another process's pages | Yes |
| `pidfd_open()` | Get a file descriptor for a process | Yes |
| `/sys/class/zram-control` | Create/remove zram compressed swap devices | Yes (for zram) |
| `/sys/block/zramN/mm_stat` | Read per-device compression statistics | Yes (for zram) |

## Graduated memory management

Wolfram applies increasing levels of intervention:

| Level | Trigger | Action | RAM saved | Impact |
|-------|---------|--------|-----------|--------|
| **Cold** | 50%+ pages idle | `MADV_COLD` — deprioritize | Gradual | None — kernel compresses when convenient |
| **Page out** | 70%+ idle, 2+ cycles | `MADV_PAGEOUT` — compress into zram now | Immediate | Microsecond decompression on access |
| **zram grow** | zram >80% full, free RAM available | Add another zram device | More capacity | None — kernel handles it |
| **zram shrink** | zram <40% used, 2+ devices | Remove newest device | Returns RAM | None — kernel migrates pages |
| **Hibernate** | All regions idle, 10+ cycles | CRIU checkpoint + kill | 100% of process | Seconds to restore; test your apps |

## LXC / Containers / VMs

**Install Wolfram on the host, not inside containers.**

The kernel interfaces Wolfram uses (`/proc/[pid]/pagemap`, `/sys/kernel/mm/page_idle/bitmap`, `process_madvise()`) operate on physical page frames and are host-level — they're either unavailable or restricted inside containers. Running on the host means one Wolfram instance compresses cold pages across all your containers and VMs simultaneously.

This works with Docker, LXC (including unprivileged containers), Kubernetes pods, and QEMU/KVM guests (for the host-side QEMU process memory).

The installer automatically detects Proxmox, Docker, and Kubernetes environments and configures appropriate exclusions so container infrastructure processes are never touched.

## Safety

- **Non-destructive**: Wolfram never modifies process memory, code, or state. Compression uses the same kernel path as normal memory pressure.
- **Transparent**: Compressed pages are decompressed by the kernel on access. Applications never know.
- **Low priority**: Runs at nice 19 / SCHED_IDLE / ionice idle — never competes with real workloads for CPU or I/O.
- **Smart exclusions**: Infrastructure processes (systemd, sshd, dockerd, kubelet, etcd, pvedaemon, etc.) are excluded by default.
- **Graceful shutdown**: On SIGTERM/SIGINT, cleans up zram devices and restores any hibernated processes.
- **Dry-run mode**: `--dry-run` lets you see what would happen without any changes.
- **Dynamic sizing**: zram pool grows and shrinks automatically — never overcommits RAM.

## License

MIT

## Author

Wolf Software Systems Ltd
