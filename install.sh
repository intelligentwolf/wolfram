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
            info "Downloaded pre-built binary for $ARCH ($LIBC)"
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
    info "Stopping existing wolfram service..."
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
    # Try to install from local copy first, otherwise download it.
    if [ -f "$SCRIPT_DIR/config.example.toml" ]; then
        info "Installing default config to $CONFIG_DIR/config.toml"
        install -m 644 "$SCRIPT_DIR/config.example.toml" "$CONFIG_DIR/config.toml"
    else
        info "Downloading default config..."
        URL="https://raw.githubusercontent.com/${REPO}/master/config.example.toml"
        if command -v curl >/dev/null 2>&1; then
            curl -fsSL "$URL" -o "$CONFIG_DIR/config.toml" 2>/dev/null || warn "Could not download config — create /etc/wolfram/config.toml manually"
        elif command -v wget >/dev/null 2>&1; then
            wget -qO "$CONFIG_DIR/config.toml" "$URL" 2>/dev/null || warn "Could not download config — create /etc/wolfram/config.toml manually"
        fi
    fi
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

# Check kernel features.
echo ""
info "Checking kernel features..."

if [ -f /sys/kernel/mm/page_idle/bitmap ]; then
    info "  Idle page tracking: available"
else
    warn "  Idle page tracking: NOT available (need CONFIG_IDLE_PAGE_TRACKING)"
    warn "  Wolfram will still work but with reduced accuracy"
fi

if [ -f /sys/module/zswap/parameters/enabled ]; then
    ZSWAP=$(cat /sys/module/zswap/parameters/enabled)
    if [ "$ZSWAP" = "Y" ]; then
        info "  zswap: enabled"
    else
        warn "  zswap: disabled — compressed pages will go to disk swap"
        warn "  Enable with: echo Y > /sys/module/zswap/parameters/enabled"
        warn "  To persist, add 'zswap.enabled=1' to kernel command line"
    fi
else
    warn "  zswap: not available in this kernel"
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
