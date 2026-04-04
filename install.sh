#!/bin/sh
set -e

# Wolfram installer — downloads a pre-built binary (or builds from source) and
# installs as a systemd service.

REPO="wolfsoftwaresystemsltd/wolfram"
INSTALL_BIN="/usr/local/bin/wolfram"
SERVICE_FILE="/etc/systemd/system/wolfram.service"
CONFIG_DIR="/etc/wolfram"
DATA_DIR="/var/lib/wolfram"
CHECKPOINT_DIR="$DATA_DIR/checkpoints"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Colours for output (disabled if not a terminal).
if [ -t 1 ]; then
    GREEN='\033[0;32m'
    RED='\033[0;31m'
    YELLOW='\033[1;33m'
    NC='\033[0m'
else
    GREEN=''
    RED=''
    YELLOW=''
    NC=''
fi

info()  { printf "${GREEN}[+]${NC} %s\n" "$1"; }
warn()  { printf "${YELLOW}[!]${NC} %s\n" "$1"; }
error() { printf "${RED}[x]${NC} %s\n" "$1"; exit 1; }

# Must be root.
if [ "$(id -u)" -ne 0 ]; then
    error "This script must be run as root (sudo ./install.sh)"
fi

# Detect platform.
detect_platform() {
    ARCH=$(uname -m)
    case "$ARCH" in
        x86_64|amd64)   PLATFORM="x86_64" ;;
        aarch64|arm64)   PLATFORM="aarch64" ;;
        armv7l|armhf)    PLATFORM="armv7" ;;
        *)               PLATFORM="" ;;
    esac

    # Always use musl (static) builds — they work on every Linux distro
    # regardless of glibc version. Zero dependencies.
    case "$PLATFORM" in
        x86_64)   ARTIFACT="wolfram-x86_64-linux-musl" ;;
        aarch64)  ARTIFACT="wolfram-aarch64-linux-musl" ;;
        armv7)    ARTIFACT="wolfram-armv7-linux-musleabihf" ;;
        *)        ARTIFACT="" ;;
    esac
}

# Try to download a pre-built binary from GitHub releases.
download_binary() {
    if [ -z "$ARTIFACT" ]; then
        return 1
    fi

    # Need curl or wget.
    if command -v curl >/dev/null 2>&1; then
        DOWNLOAD="curl -fsSL"
        DOWNLOAD_OUT="curl -fsSL -o"
    elif command -v wget >/dev/null 2>&1; then
        DOWNLOAD="wget -qO-"
        DOWNLOAD_OUT="wget -qO"
    else
        return 1
    fi

    # Get latest release tag.
    info "Checking for latest release..."
    LATEST=$($DOWNLOAD "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null | \
        grep '"tag_name"' | head -1 | sed 's/.*"tag_name": *"//;s/".*//')

    if [ -z "$LATEST" ]; then
        warn "Could not fetch latest release"
        return 1
    fi

    info "Latest release: $LATEST"
    info "Downloading ${ARTIFACT}.tar.gz..."

    URL="https://github.com/${REPO}/releases/download/${LATEST}/${ARTIFACT}.tar.gz"
    TMPDIR=$(mktemp -d)

    if $DOWNLOAD_OUT "$TMPDIR/wolfram.tar.gz" "$URL" 2>/dev/null; then
        tar xzf "$TMPDIR/wolfram.tar.gz" -C "$TMPDIR"
        if [ -f "$TMPDIR/wolfram" ]; then
            BINARY="$TMPDIR/wolfram"
            info "Downloaded pre-built binary for $ARCH (static)"
            return 0
        fi
    fi

    rm -rf "$TMPDIR"
    warn "Download failed"
    return 1
}

# Build from source as fallback.
build_from_source() {
    if ! command -v cargo >/dev/null 2>&1; then
        error "No pre-built binary available for your platform and cargo not found.\nInstall Rust first: https://rustup.rs"
    fi

    info "Building from source..."
    cd "$SCRIPT_DIR"
    sudo -u "${SUDO_USER:-$(logname)}" cargo build --release 2>&1
    BINARY="$SCRIPT_DIR/target/release/wolfram"

    if [ ! -f "$BINARY" ]; then
        error "Build failed — binary not found at $BINARY"
    fi

    info "Built from source"
}

# Detect environment and generate optimised config.
generate_config() {
    # Detect what kind of system this is.
    ENV_TYPE="server"
    # Base exclusions — always present. Note: kernel truncates /proc/[pid]/status Name: to
    # 15 characters, so exclusion strings longer than 15 chars will never match.
    ENV_EXCLUDES='"systemd", "wolfram", "sshd", "dockerd", "containerd", "containerd-shim", "kubelet", "kube-proxy", "kube-apiserver", "kube-controller", "kube-scheduler", "etcd"'
    MIN_ANON=50
    MIN_REGION=4096
    CYCLE_SECS=120
    IDLE_SAMPLE=30
    COLD=0.7
    PAGEOUT=0.9
    PAGEOUT_CYCLES=3

    # Desktop detection first — a dev machine with Docker is still a desktop.
    # Check for running display server processes (env vars are lost under sudo).
    IS_DESKTOP=false
    if pgrep -x 'Xorg|Xwayland|gnome-shell|kwin_wayland|plasmashell|sway|hyprland|mutter' >/dev/null 2>&1; then
        IS_DESKTOP=true
    elif [ -n "$DISPLAY" ] || [ -n "$WAYLAND_DISPLAY" ]; then
        IS_DESKTOP=true
    elif [ -d /usr/share/xsessions ] || [ -d /usr/share/wayland-sessions ]; then
        IS_DESKTOP=true
    fi

    # Proxmox VE (LXC/QEMU host) — check before desktop since Proxmox has X libs
    if [ -f /usr/bin/pvesh ] || [ -d /etc/pve ]; then
        ENV_TYPE="proxmox"
        MIN_ANON=100
        MIN_REGION=4096
        CYCLE_SECS=300
        COLD=0.5
        PAGEOUT=0.7
        PAGEOUT_CYCLES=2
        ENV_EXCLUDES="$ENV_EXCLUDES"', "pvedaemon", "pveproxy", "pvestatd", "pvefw", "pmxcfs", "corosync", "lxc-start", "qemu-system", "ceph-osd", "ceph-mon"'
        info "Detected Proxmox VE — configuring for LXC/VM host"

    # Kubernetes node
    elif command -v kubelet >/dev/null 2>&1 || [ -d /var/lib/kubelet ]; then
        ENV_TYPE="kubernetes"
        MIN_ANON=100
        MIN_REGION=4096
        CYCLE_SECS=300
        COLD=0.6
        PAGEOUT=0.8
        PAGEOUT_CYCLES=3
        ENV_EXCLUDES="$ENV_EXCLUDES"', "flanneld", "calico-node", "cilium"'
        info "Detected Kubernetes node — configuring for K8s workloads"

    # Raspberry Pi (before desktop — Pi can have a desktop too)
    elif grep -qi 'raspberry\|bcm2' /proc/cpuinfo 2>/dev/null; then
        ENV_TYPE="rpi"
        MIN_ANON=10
        MIN_REGION=1024
        CYCLE_SECS=300
        IDLE_SAMPLE=20
        COLD=0.5
        PAGEOUT=0.7
        PAGEOUT_CYCLES=3
        info "Detected Raspberry Pi — configuring for low-memory system"

    # Desktop (check before Docker — a dev machine with Docker is still a desktop)
    elif [ "$IS_DESKTOP" = true ]; then
        ENV_TYPE="desktop"
        MIN_ANON=50
        MIN_REGION=2048
        CYCLE_SECS=120
        COLD=0.7
        PAGEOUT=0.9
        PAGEOUT_CYCLES=3
        ENV_EXCLUDES="$ENV_EXCLUDES"', "Xorg", "Xwayland", "gnome-shell", "kwin_wayland", "kwin_x11", "plasmashell", "sway", "hyprland", "mutter", "pipewire", "pulseaudio", "wireplumber", "mutter"'
        info "Detected desktop environment — excluding display server and audio"

    # Docker host (headless server only — desktops with Docker handled above)
    elif command -v dockerd >/dev/null 2>&1 || [ -f /run/docker.pid ]; then
        ENV_TYPE="docker"
        MIN_ANON=50
        MIN_REGION=4096
        CYCLE_SECS=180
        COLD=0.6
        PAGEOUT=0.8
        PAGEOUT_CYCLES=2
        info "Detected Docker host — configuring for container workloads"

    else
        info "Detected generic server — using standard config"
    fi

    info "Writing config to $CONFIG_DIR/config.toml ($ENV_TYPE profile)"
    cat > "$CONFIG_DIR/config.toml" << CONFIGEOF
# Wolfram configuration — auto-generated for: $ENV_TYPE
# Edit to taste, then: systemctl restart wolfram

min_process_anon_mib = $MIN_ANON
min_region_kib = $MIN_REGION
idle_sample_secs = $IDLE_SAMPLE
cycle_secs = $CYCLE_SECS

cold_threshold = $COLD
pageout_threshold = $PAGEOUT
pageout_after_cycles = $PAGEOUT_CYCLES

enable_hibernation = false
hibernate_after_cycles = 10
checkpoint_dir = "/var/lib/wolfram/checkpoints"
enable_port_proxy = false

excluded_names = [$ENV_EXCLUDES]
excluded_pids = [1]
dry_run = false
CONFIGEOF
}

# --- Main ---

detect_platform
info "Detected platform: ${ARCH}"

# Try pre-built binary first, fall back to building from source.
if ! download_binary; then
    warn "No pre-built binary available — building from source"
    build_from_source
fi

# Stop existing service if running.
if systemctl is-active --quiet wolfram 2>/dev/null; then
    info "Stopping wolfram service (may take up to a minute if mid-cycle)..."
    systemctl stop wolfram
fi

# Install binary.
info "Installing binary to $INSTALL_BIN"
install -m 755 "$BINARY" "$INSTALL_BIN"

# Clean up temp download dir if used.
if [ -n "$TMPDIR" ] && [ -d "$TMPDIR" ]; then
    rm -rf "$TMPDIR"
fi

# Create config directory and install default config if not present.
if [ ! -d "$CONFIG_DIR" ]; then
    info "Creating config directory $CONFIG_DIR"
    mkdir -p "$CONFIG_DIR"
fi

if [ ! -f "$CONFIG_DIR/config.toml" ]; then
    generate_config
else
    warn "Config file already exists at $CONFIG_DIR/config.toml — not overwriting"
fi

# Create data directories.
info "Creating data directories"
mkdir -p "$DATA_DIR"
mkdir -p "$CHECKPOINT_DIR"

# Install systemd service — from local copy or download.
if [ -f "$SCRIPT_DIR/wolfram.service" ]; then
    info "Installing systemd service"
    install -m 644 "$SCRIPT_DIR/wolfram.service" "$SERVICE_FILE"
else
    info "Downloading systemd service file..."
    URL="https://raw.githubusercontent.com/${REPO}/master/wolfram.service"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$URL" -o "$SERVICE_FILE" 2>/dev/null || error "Could not download service file"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$SERVICE_FILE" "$URL" 2>/dev/null || error "Could not download service file"
    fi
fi

# Reload systemd.
info "Reloading systemd daemon"
systemctl daemon-reload

# Enable the service (start on boot).
info "Enabling wolfram service"
systemctl enable wolfram

# Wolfram manages zram devices itself at runtime — no swap file needed.
# Load the zram module if available so wolfram can create devices on startup.
if ! [ -d /sys/class/zram-control ]; then
    modprobe zram 2>/dev/null && info "Loaded zram kernel module" || true
fi

# Clean up any swap files from previous wolfram versions.
if [ -f /var/swapfile.img ]; then
    swapoff /var/swapfile.img 2>/dev/null
    # Check for loop device
    LOOP_DEV=$(losetup -j /var/swapfile.img -O NAME --noheadings 2>/dev/null)
    if [ -n "$LOOP_DEV" ]; then
        swapoff "$LOOP_DEV" 2>/dev/null
        losetup -d "$LOOP_DEV" 2>/dev/null
    fi
    rm -f /var/swapfile.img
    sed -i '\|/var/swapfile.img|d' /etc/fstab 2>/dev/null
    info "Removed old swap file (wolfram now uses zram — no disk needed)"
fi
if [ -f /swapfile ]; then
    swapoff /swapfile 2>/dev/null
    rm -f /swapfile
    sed -i '\|/swapfile|d' /etc/fstab 2>/dev/null
fi
# Remove old wolfram-swap service if present.
if [ -f /etc/systemd/system/wolfram-swap.service ]; then
    systemctl stop wolfram-swap 2>/dev/null
    systemctl disable wolfram-swap 2>/dev/null
    rm -f /etc/systemd/system/wolfram-swap.service
    systemctl daemon-reload
    info "Removed old wolfram-swap service"
fi
echo ""

# Check kernel features.
echo ""
info "Checking kernel features..."

if [ -f /sys/kernel/mm/page_idle/bitmap ]; then
    info "  Idle page tracking: available"
else
    warn "  Idle page tracking: NOT available (need CONFIG_IDLE_PAGE_TRACKING)"
    warn "  Wolfram will still work but with reduced accuracy"
fi

if [ -d /sys/class/zram-control ] || modprobe --dry-run zram 2>/dev/null; then
    info "  zram: available (wolfram creates compressed swap in RAM — no disk needed)"
else
    warn "  zram: not available — wolfram will fall back to zswap if available"
fi

if command -v criu >/dev/null 2>&1; then
    info "  CRIU: available ($(criu --version 2>&1 | head -1))"
else
    info "  CRIU: not installed (optional — needed for process hibernation)"
fi

# Start the service.
info "Starting wolfram service..."
systemctl start wolfram

# Verify it's running.
if systemctl is-active --quiet wolfram 2>/dev/null; then
    info "Wolfram is running"
else
    warn "Wolfram failed to start — check: journalctl -u wolfram -e"
fi

# Done.
echo ""
info "Installation complete!"
echo ""
echo "  Binary:   $INSTALL_BIN"
echo "  Config:   $CONFIG_DIR/config.toml"
echo "  Service:  $SERVICE_FILE"
echo "  Data:     $DATA_DIR"
echo ""
echo "  View logs:      journalctl -u wolfram -f"
echo "  Check savings:  cat /var/lib/wolfram/stats.json"
echo "  Check status:   systemctl status wolfram"
echo "  Stop:           systemctl stop wolfram"
echo "  Reconfigure:    nano $CONFIG_DIR/config.toml && systemctl restart wolfram"
echo ""
